//! This module has containers for storing the tasks spawned on a scheduler. The
//! `OwnedTasks` container is thread-safe but can only store tasks that
//! implement Send. The `LocalOwnedTasks` container is not thread safe, but can
//! store non-Send tasks.
//!
//! The collections can be closed to prevent adding new tasks during shutdown of
//! the scheduler with the collection.
//! 此模块具有用于存储在调度程序上生成的任务的容器.
//! `OwnedTasks` 容器是线程安全的,但只能存储实现 Send 的任务.
//! `LocalOwnedTasks` 容器不是线程安全的,但可以存储非 Send 任务.
//!
//! 可以关闭集合以防止在使用该集合关闭调度程序期间添加新任务.

use crate::future::Future;
use crate::loom::cell::UnsafeCell;
use crate::runtime::task::{JoinHandle, LocalNotified, Notified, Schedule, Task};
use crate::util::linked_list::{Link, LinkedList};
use crate::util::sharded_list;

use crate::loom::sync::atomic::{AtomicBool, Ordering};
use std::marker::PhantomData;
use std::num::NonZeroU64;

// The id from the module below is used to verify whether a given task is stored
// in this OwnedTasks, or some other task. The counter starts at one so we can
// use `None` for tasks not owned by any list.
//
// The safety checks in this file can technically be violated if the counter is
// overflown, but the checks are not supposed to ever fail unless there is a
// bug in Tokio, so we accept that certain bugs would not be caught if the two
// mixed up runtimes happen to have the same id.
// 下面模块中的 id 用于验证给定任务是否存储在此 OwnedTasks 中,还是其他任务中.
// 计数器从一开始,因此我们可以对不属于任何列表的任务使用"None".
//
// 如果计数器溢出,则从技术上讲,此文件中的安全检查可能会被违反,但除非 Tokio 中存在错误,
// 否则检查不应该失败,因此我们接受如果两个混合运行时恰好具有相同的 id,则某些错误不会被捕获.
cfg_has_atomic_u64! {
    use std::sync::atomic::AtomicU64;

    static NEXT_OWNED_TASKS_ID: AtomicU64 = AtomicU64::new(1);

    fn get_next_id() -> NonZeroU64 {
        loop {
            let id = NEXT_OWNED_TASKS_ID.fetch_add(1, Ordering::Relaxed);
            if let Some(id) = NonZeroU64::new(id) {
                return id;
            }
        }
    }
}

cfg_not_has_atomic_u64! {
    use std::sync::atomic::AtomicU32;

    static NEXT_OWNED_TASKS_ID: AtomicU32 = AtomicU32::new(1);

    fn get_next_id() -> NonZeroU64 {
        loop {
            let id = NEXT_OWNED_TASKS_ID.fetch_add(1, Ordering::Relaxed);
            if let Some(id) = NonZeroU64::new(u64::from(id)) {
                return id;
            }
        }
    }
}

pub(crate) struct OwnedTasks<S: 'static> {
    list: List<S>,
    pub(crate) id: NonZeroU64,
    closed: AtomicBool,
}

type List<S> = sharded_list::ShardedList<Task<S>, <Task<S> as Link>::Target>;

pub(crate) struct LocalOwnedTasks<S: 'static> {
    inner: UnsafeCell<OwnedTasksInner<S>>,
    pub(crate) id: NonZeroU64,
    _not_send_or_sync: PhantomData<*const ()>,
}

struct OwnedTasksInner<S: 'static> {
    list: LinkedList<Task<S>, <Task<S> as Link>::Target>,
    closed: bool,
}

impl<S: 'static> OwnedTasks<S> {
    pub(crate) fn new(num_cores: usize) -> Self {
        let shard_size = Self::gen_shared_list_size(num_cores);
        Self {
            list: List::new(shard_size),
            closed: AtomicBool::new(false),
            id: get_next_id(),
        }
    }

    /// Binds the provided task to this `OwnedTasks` instance. This fails if the
    /// `OwnedTasks` has been closed.
    /// 将提供的任务绑定到此 `OwnedTasks` 实例.如果 `OwnedTasks` 已关闭,则此操作会失败.
    pub(crate) fn bind<T>(
        &self,
        task: T,
        scheduler: S,
        id: super::Id,
    ) -> (JoinHandle<T::Output>, Option<Notified<S>>)
    where
        S: Schedule,
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        let (task, notified, join) = super::new_task(task, scheduler, id);
        let notified = unsafe { self.bind_inner(task, notified) };
        (join, notified)
    }

    /// The part of `bind` that's the same for every type of future.
    unsafe fn bind_inner(&self, task: Task<S>, notified: Notified<S>) -> Option<Notified<S>>
    where
        S: Schedule,
    {
        unsafe {
            // safety: We just created the task, so we have exclusive access
            // to the field.
            // 设置owner_id
            task.header().set_owner_id(self.id);
        }

        let shard = self.list.lock_shard(&task);
        // Check the closed flag in the lock for ensuring all that tasks
        // will shut down after the OwnedTasks has been closed.
        // 检查锁中的关闭标志,以确保所有任务将在 OwnedTasks 关闭后关闭.
        if self.closed.load(Ordering::Acquire) {
            drop(shard);
            task.shutdown();
            return None;
        }
        // 保存task到OwnerTasks中
        shard.push(task);
        Some(notified)
    }

    /// Asserts that the given task is owned by this `OwnedTasks` and convert it to
    /// a `LocalNotified`, giving the thread permission to poll this task.
    /// 断言给定的任务由此`OwnedTasks`拥有,并将其转换为`LocalNotified`,从而授予线程轮询此任务的权限.
    #[inline]
    pub(crate) fn assert_owner(&self, task: Notified<S>) -> LocalNotified<S> {
        debug_assert_eq!(task.header().get_owner_id(), Some(self.id));
        // safety: All tasks bound to this OwnedTasks are Send, so it is safe
        // to poll it on this thread no matter what thread we are on.
        // 安全性:所有绑定到此 OwnedTasks 的任务都将被发送,因此无论我们在哪个线程上,都可以安全地在此线程上轮询它.
        LocalNotified {
            task: task.0,
            _not_send: PhantomData,
        }
    }

    /// Shuts down all tasks in the collection. This call also closes the
    /// collection, preventing new items from being added.
    ///
    /// The parameter start determines which shard this method will start at.
    /// Using different values for each worker thread reduces contention.
    /// 关闭集合中的所有任务.
    /// 此调用还会关闭集合,从而阻止添加新任务.
    /// 参数 start 确定此方法将从哪个分片启动.为每个工作线程使用不同的值可减少争用.
    pub(crate) fn close_and_shutdown_all(&self, start: usize)
    where
        S: Schedule,
    {
        self.closed.store(true, Ordering::Release);
        for i in start..self.get_shard_size() + start {
            loop {
                let task = self.list.pop_back(i);
                match task {
                    Some(task) => {
                        task.shutdown();
                    }
                    None => break,
                }
            }
        }
    }

    #[inline]
    pub(crate) fn get_shard_size(&self) -> usize {
        self.list.shard_size()
    }

    pub(crate) fn num_alive_tasks(&self) -> usize {
        self.list.len()
    }

    cfg_64bit_metrics! {
        pub(crate) fn spawned_tasks_count(&self) -> u64 {
            self.list.added()
        }
    }

    pub(crate) fn remove(&self, task: &Task<S>) -> Option<Task<S>> {
        // If the task's owner ID is `None` then it is not part of any list and
        // doesn't need removing.
        // 如果任务的所有者 ID 为`None`,则它不属于任何列表的一部分,不需要删除.
        let task_id = task.header().get_owner_id()?;

        assert_eq!(task_id, self.id);

        // safety: We just checked that the provided task is not in some other
        // linked list.
        unsafe { self.list.remove(task.header_ptr()) }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// Generates the size of the sharded list based on the number of worker threads.
    ///
    /// The sharded lock design can effectively alleviate
    /// lock contention performance problems caused by high concurrency.
    ///
    /// However, as the number of shards increases, the memory continuity between
    /// nodes in the intrusive linked list will diminish. Furthermore,
    /// the construction time of the sharded list will also increase with a higher number of shards.
    ///
    /// Due to the above reasons, we set a maximum value for the shared list size,
    /// denoted as `MAX_SHARED_LIST_SIZE`.
    /// 根据工作线程数生成分片列表的大小.
    /// 分片锁设计可以有效缓解高并发带来的锁争用性能问题.
    /// 但是随着分片数量的增加,侵入式链表中节点之间的内存连续性会降低.此外,分片数量的增加也会增加分片列表的构建时间.
    /// 鉴于上述原因,我们为共享列表大小设置了一个最大值,记为 `MAX_SHARED_LIST_SIZE`.
    fn gen_shared_list_size(num_cores: usize) -> usize {
        const MAX_SHARED_LIST_SIZE: usize = 1 << 16;
        usize::min(MAX_SHARED_LIST_SIZE, num_cores.next_power_of_two() * 4)
    }
}

cfg_taskdump! {
    impl<S: 'static> OwnedTasks<S> {
        /// Locks the tasks, and calls `f` on an iterator over them.
        /// 锁定任务,并在其迭代器上调用`f`.
        pub(crate) fn for_each<F>(&self, f: F)
        where
            F: FnMut(&Task<S>),
        {
            self.list.for_each(f);
        }
    }
}

impl<S: 'static> LocalOwnedTasks<S> {
    pub(crate) fn new() -> Self {
        Self {
            inner: UnsafeCell::new(OwnedTasksInner {
                list: LinkedList::new(),
                closed: false,
            }),
            id: get_next_id(),
            _not_send_or_sync: PhantomData,
        }
    }

    pub(crate) fn bind<T>(
        &self,
        task: T,
        scheduler: S,
        id: super::Id,
    ) -> (JoinHandle<T::Output>, Option<Notified<S>>)
    where
        S: Schedule,
        T: Future + 'static,
        T::Output: 'static,
    {
        let (task, notified, join) = super::new_task(task, scheduler, id);

        unsafe {
            // safety: We just created the task, so we have exclusive access
            // to the field.
            task.header().set_owner_id(self.id);
        }

        if self.is_closed() {
            drop(notified);
            task.shutdown();
            (join, None)
        } else {
            self.with_inner(|inner| {
                inner.list.push_front(task);
            });
            (join, Some(notified))
        }
    }

    /// Shuts down all tasks in the collection. This call also closes the
    /// collection, preventing new items from being added.
    pub(crate) fn close_and_shutdown_all(&self)
    where
        S: Schedule,
    {
        self.with_inner(|inner| inner.closed = true);

        while let Some(task) = self.with_inner(|inner| inner.list.pop_back()) {
            task.shutdown();
        }
    }

    pub(crate) fn remove(&self, task: &Task<S>) -> Option<Task<S>> {
        // If the task's owner ID is `None` then it is not part of any list and
        // doesn't need removing.
        let task_id = task.header().get_owner_id()?;

        assert_eq!(task_id, self.id);

        self.with_inner(|inner|
            // safety: We just checked that the provided task is not in some
            // other linked list.
            unsafe { inner.list.remove(task.header_ptr()) })
    }

    /// Asserts that the given task is owned by this `LocalOwnedTasks` and convert
    /// it to a `LocalNotified`, giving the thread permission to poll this task.
    #[inline]
    pub(crate) fn assert_owner(&self, task: Notified<S>) -> LocalNotified<S> {
        assert_eq!(task.header().get_owner_id(), Some(self.id));

        // safety: The task was bound to this LocalOwnedTasks, and the
        // LocalOwnedTasks is not Send or Sync, so we are on the right thread
        // for polling this task.
        LocalNotified {
            task: task.0,
            _not_send: PhantomData,
        }
    }

    #[inline]
    fn with_inner<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut OwnedTasksInner<S>) -> T,
    {
        // safety: This type is not Sync, so concurrent calls of this method
        // can't happen.  Furthermore, all uses of this method in this file make
        // sure that they don't call `with_inner` recursively.
        self.inner.with_mut(|ptr| unsafe { f(&mut *ptr) })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.with_inner(|inner| inner.closed)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.with_inner(|inner| inner.list.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // This test may run in parallel with other tests, so we only test that ids
    // come in increasing order.
    #[test]
    fn test_id_not_broken() {
        let mut last_id = get_next_id();

        for _ in 0..1000 {
            let next_id = get_next_id();
            assert!(last_id < next_id);
            last_id = next_id;
        }
    }
}
