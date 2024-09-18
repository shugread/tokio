//! The task module.
//!
//! The task module contains the code that manages spawned tasks and provides a
//! safe API for the rest of the runtime to use. Each task in a runtime is
//! stored in an `OwnedTasks` or `LocalOwnedTasks` object.
//!
//! # Task reference types
//!
//! A task is usually referenced by multiple handles, and there are several
//! types of handles.
//!
//!  * `OwnedTask` - tasks stored in an `OwnedTasks` or `LocalOwnedTasks` are of this
//!    reference type.
//!
//!  * `JoinHandle` - each task has a `JoinHandle` that allows access to the output
//!    of the task.
//!
//!  * `Waker` - every waker for a task has this reference type. There can be any
//!    number of waker references.
//!
//!  * `Notified` - tracks whether the task is notified.
//!
//!  * `Unowned` - this task reference type is used for tasks not stored in any
//!    runtime. Mainly used for blocking tasks, but also in tests.
//!
//! The task uses a reference count to keep track of how many active references
//! exist. The `Unowned` reference type takes up two ref-counts. All other
//! reference types take up a single ref-count.
//!
//! Besides the waker type, each task has at most one of each reference type.
//!
//! # State
//!
//! The task stores its state in an atomic `usize` with various bitfields for the
//! necessary information. The state has the following bitfields:
//!
//!  * `RUNNING` - Tracks whether the task is currently being polled or cancelled.
//!    This bit functions as a lock around the task.
//!
//!  * `COMPLETE` - Is one once the future has fully completed and has been
//!    dropped. Never unset once set. Never set together with RUNNING.
//!
//!  * `NOTIFIED` - Tracks whether a Notified object currently exists.
//!
//!  * `CANCELLED` - Is set to one for tasks that should be cancelled as soon as
//!    possible. May take any value for completed tasks.
//!
//!  * `JOIN_INTEREST` - Is set to one if there exists a `JoinHandle`.
//!
//!  * `JOIN_WAKER` - Acts as an access control bit for the join handle waker. The
//!    protocol for its usage is described below.
//!
//! The rest of the bits are used for the ref-count.
//!
//! # Fields in the task
//!
//! The task has various fields. This section describes how and when it is safe
//! to access a field.
//!
//!  * The state field is accessed with atomic instructions.
//!
//!  * The `OwnedTask` reference has exclusive access to the `owned` field.
//!
//!  * The Notified reference has exclusive access to the `queue_next` field.
//!
//!  * The `owner_id` field can be set as part of construction of the task, but
//!    is otherwise immutable and anyone can access the field immutably without
//!    synchronization.
//!
//!  * If COMPLETE is one, then the `JoinHandle` has exclusive access to the
//!    stage field. If COMPLETE is zero, then the RUNNING bitfield functions as
//!    a lock for the stage field, and it can be accessed only by the thread
//!    that set RUNNING to one.
//!
//!  * The waker field may be concurrently accessed by different threads: in one
//!    thread the runtime may complete a task and *read* the waker field to
//!    invoke the waker, and in another thread the task's `JoinHandle` may be
//!    polled, and if the task hasn't yet completed, the `JoinHandle` may *write*
//!    a waker to the waker field. The `JOIN_WAKER` bit ensures safe access by
//!    multiple threads to the waker field using the following rules:
//!
//!    1. `JOIN_WAKER` is initialized to zero.
//!
//!    2. If `JOIN_WAKER` is zero, then the `JoinHandle` has exclusive (mutable)
//!       access to the waker field.
//!
//!    3. If `JOIN_WAKER` is one, then the `JoinHandle` has shared (read-only)
//!       access to the waker field.
//!
//!    4. If `JOIN_WAKER` is one and COMPLETE is one, then the runtime has shared
//!       (read-only) access to the waker field.
//!
//!    5. If the `JoinHandle` needs to write to the waker field, then the
//!       `JoinHandle` needs to (i) successfully set `JOIN_WAKER` to zero if it is
//!       not already zero to gain exclusive access to the waker field per rule
//!       2, (ii) write a waker, and (iii) successfully set `JOIN_WAKER` to one.
//!
//!    6. The `JoinHandle` can change `JOIN_WAKER` only if COMPLETE is zero (i.e.
//!       the task hasn't yet completed).
//!
//!    Rule 6 implies that the steps (i) or (iii) of rule 5 may fail due to a
//!    race. If step (i) fails, then the attempt to write a waker is aborted. If
//!    step (iii) fails because COMPLETE is set to one by another thread after
//!    step (i), then the waker field is cleared. Once COMPLETE is one (i.e.
//!    task has completed), the `JoinHandle` will not modify `JOIN_WAKER`. After the
//!    runtime sets COMPLETE to one, it invokes the waker if there is one.
//!
//! All other fields are immutable and can be accessed immutably without
//! synchronization by anyone.
//!
//! # Safety
//!
//! This section goes through various situations and explains why the API is
//! safe in that situation.
//!
//! ## Polling or dropping the future
//!
//! Any mutable access to the future happens after obtaining a lock by modifying
//! the RUNNING field, so exclusive access is ensured.
//!
//! When the task completes, exclusive access to the output is transferred to
//! the `JoinHandle`. If the `JoinHandle` is already dropped when the transition to
//! complete happens, the thread performing that transition retains exclusive
//! access to the output and should immediately drop it.
//!
//! ## Non-Send futures
//!
//! If a future is not Send, then it is bound to a `LocalOwnedTasks`.  The future
//! will only ever be polled or dropped given a `LocalNotified` or inside a call
//! to `LocalOwnedTasks::shutdown_all`. In either case, it is guaranteed that the
//! future is on the right thread.
//!
//! If the task is never removed from the `LocalOwnedTasks`, then it is leaked, so
//! there is no risk that the task is dropped on some other thread when the last
//! ref-count drops.
//!
//! ## Non-Send output
//!
//! When a task completes, the output is placed in the stage of the task. Then,
//! a transition that sets COMPLETE to true is performed, and the value of
//! `JOIN_INTEREST` when this transition happens is read.
//!
//! If `JOIN_INTEREST` is zero when the transition to COMPLETE happens, then the
//! output is immediately dropped.
//!
//! If `JOIN_INTEREST` is one when the transition to COMPLETE happens, then the
//! `JoinHandle` is responsible for cleaning up the output. If the output is not
//! Send, then this happens:
//!
//!  1. The output is created on the thread that the future was polled on. Since
//!     only non-Send futures can have non-Send output, the future was polled on
//!     the thread that the future was spawned from.
//!  2. Since `JoinHandle<Output>` is not Send if Output is not Send, the
//!     `JoinHandle` is also on the thread that the future was spawned from.
//!  3. Thus, the `JoinHandle` will not move the output across threads when it
//!     takes or drops the output.
//!
//! ## Recursive poll/shutdown
//!
//! Calling poll from inside a shutdown call or vice-versa is not prevented by
//! the API exposed by the task module, so this has to be safe. In either case,
//! the lock in the RUNNING bitfield makes the inner call return immediately. If
//! the inner call is a `shutdown` call, then the CANCELLED bit is set, and the
//! poll call will notice it when the poll finishes, and the task is cancelled
//! at that point.

// Some task infrastructure is here to support `JoinSet`, which is currently
// unstable. This should be removed once `JoinSet` is stabilized.

//! 任务模块.
//!
//! 任务模块包含管理生成的任务的代码, 并为运行时的其余部分提供
//! 安全的 API.运行时中的每个任务都存储在 `OwnedTasks` 或 `LocalOwnedTasks` 对象中.
//!
//! # 任务引用类型
//!
//! 任务通常由多个句柄引用, 并且有几种
//! 类型的句柄.
//!
//! * `OwnedTask` - 存储在 `OwnedTasks` 或 `LocalOwnedTasks` 中的任务属于此
//! 引用类型.
//!
//! * `JoinHandle` - 每个任务都有一个 `JoinHandle`, 允许访问任务的输出
//!.
//!
//! * `Waker` - 任务的每个唤醒器都具有此引用类型.可以有任意
//! 数量的唤醒器引用.
//!
//! * `Notified` - 跟踪任务是否已通知.
//!
//! * `Unowned` - 此任务引用类型用于未存储在任何
//! 运行时中的任务.主要用于阻止任务, 但也用于测试.
//!
//! 任务使用引用计数来跟踪存在多少个活动引用
//!.`Unowned` 引用类型占用两个引用计数.所有其他
//! 引用类型占用一个引用计数.
//!
//! 除了唤醒器类型外, 每个任务最多具有每种引用类型之一.
//!
//! # 状态
//!
//! 任务将其状态存储在原子 `usize` 中, 其中包含各种位字段, 用于
//! 必要的信息.状态具有以下位字段：
//!
//! * `RUNNING` - 跟踪任务当前是否正在被轮询或取消.
//! 此位用作任务周围的锁.
//!
//! * `COMPLETE` - 一旦Future完全完成并被
//! 删除, 则为 1.一旦设置, 就永远不会取消设置.永远不要与 RUNNING 一起设置.
//!
//! * `NOTIFIED` - 跟踪当前是否存在 Notified 对象.
//!
//! * `CANCELLED` - 对于应尽快取消的任务, 设置为 1.对于已完成的任务, 可以采用任何值.
//!
//! * `JOIN_INTEREST` - 如果存在 `JoinHandle`, 则设置为 1.
//!
//! * `JOIN_WAKER` - 充当连接句柄唤醒器的访问控制位.其使用的
//! 协议如下所述.
//!
//! 其余位用于引用计数.
//!
//! # 任务中的字段
//!
//! 任务有各种字段.本节介绍如何以及何时安全地
//! 访问字段.
//!
//! * 使用原子指令访问状态字段.
//!
//! * `OwnedTask` 引用对 `owned` 字段具有独占访问权限.
//!
//! * Notified 引用对 `queue_next` 字段具有独占访问权限.
//!
//! * `owner_id` 字段可以设置为任务构造的一部分,
//! 否则是不可变的, 任何人都可以不可变地访问该字段而无需
//! 同步.
//!
//! * 如果 COMPLETE 为 1, 则 `JoinHandle` 对
//! stage 字段具有独占访问权限.如果 COMPLETE 为零, 则 RUNNING 位字段用作
//! stage 字段的锁, 并且只能由将 RUNNING 设置为 1 的线程
//! 访问它.
//!
//! * waker 字段可以由不同的线程同时访问：在一个
//! 线程中, 运行时可以完成一项任务并*读取* waker 字段以
//!调用唤醒器, 在另一个线程中, 任务的 `JoinHandle` 可能被
//! 轮询, 如果任务尚未完成, `JoinHandle` 可能会 *写入*
//! 唤醒器到唤醒器字段.`JOIN_WAKER` 位确保通过
//! 多个线程使用以下规则对唤醒器字段进行安全访问：
//!
//! 1. `JOIN_WAKER` 初始化为零.
//!
//! 2. 如果 `JOIN_WAKER` 为零, 则 `JoinHandle` 对唤醒器字段具有独占（可变）
//! 访问权限.
//!
//! 3. 如果 `JOIN_WAKER` 为 1, 则 `JoinHandle` 对唤醒器字段具有共享（只读）
//! 访问权限.
//!
//! 4. 如果 `JOIN_WAKER` 为 1 且 COMPLETE 为 1, 则运行时具有共享
//! （只读）访问唤醒器字段.
//!
//! 5. 如果 `JoinHandle` 需要写入唤醒器字段, 则
//! `JoinHandle` 需要 (i) 成功将 `JOIN_WAKER` 设置为零（如果尚未为零）才能根据规则
//! 2 获得对唤醒器字段的独占访问权限, (ii) 写入唤醒器, 以及 (iii) 成功将 `JOIN_WAKER` 设置为 1.
//!
//! 6. 仅当 COMPLETE 为零（即
//! 任务尚未完成）时, `JoinHandle` 才能更改 `JOIN_WAKER`.
//!
//! 规则 6 意味着规则 5 的步骤 (i) 或 (iii) 可能由于
//! 竞争而失败.如果步骤 (i) 失败, 则写入唤醒器的尝试将被中止.如果
//!步骤 (iii) 失败, 因为 COMPLETE 在步骤 (i) 之后被另一个线程设置为 1, 然后唤醒器字段被清除.一旦 COMPLETE 为 1（即
//! 任务已完成）, `JoinHandle` 将不会修改 `JOIN_WAKER`.在
//! 运行时将 COMPLETE 设置为 1 后, 它会调用唤醒器（如果有）.
//!
//! 所有其他字段都是不可变的, 任何人都可以不可变地访问, 而无需
//! 同步.
//!
//! # 安全
//!
//! 本节介绍了各种情况, 并解释了 why API 在那种情况下是
//! 安全的.
//!
//! ## 轮询或放弃Future
//!
//! 对Future的任何可变访问都是在通过修改
//! RUNNING 字段获得锁定后发生的, 因此可以确保独占访问.
//!
//! 当任务完成时, 对输出的独占访问将转移到
//! `JoinHandle`.如果在转换到
//! 完成时已经删除了 `JoinHandle`, 则执行该转换的线程将保留对输出的独占访问, 并应立即将其删除.
//!
//! ## !Send Future
//!
//! 如果Future不是Send的, 那么它将绑定到 `LocalOwnedTasks`.Future
//! 只会在给定 `LocalNotified` 或在调用
//! `LocalOwnedTasks::shutdown_all` 时被轮询或删除.无论哪种情况, 都可以保证
//! Future在正确的线程上.
//!
//! 如果任务从未从 `LocalOwnedTasks` 中删除, 那么它就会被泄露, 因此
//! 当最后一个
//! 引用计数下降时, 不存在任务被丢弃到其他线程的风险.
//!
//! ## !Send Output
//!
//! 当任务完成时, 输出将放置在任务的阶段中.然后,
//! 执行将 COMPLETE 设置为 true 的转换, 并读取此转换发生时
//! `JOIN_INTEREST` 的值.
//!
//! 如果在转换到 COMPLETE 时 `JOIN_INTEREST` 为零, 则
//! 输出将立即被丢弃.
//!
//! 如果在转换到 COMPLETE 时 `JOIN_INTEREST` 为 1, 则
//! `JoinHandle` 负责清理输出.如果输出未
//!Send, 则会发生以下情况：
//!
//! 1. 输出是在轮询Future的线程上创建的.由于
//!只有非发送的Future才能具有非发送输出, 因此Future是在
//!产生Future的线程上轮询的.
//! 2. 由于如果输出未发送, 则 `JoinHandle<Output>` 不会发送,
//! `JoinHandle` 也在产生Future的线程上.
//! 3. 因此, `JoinHandle` 在获取或丢弃输出时不会跨线程移动输出.
//!
//! ## 递归轮询/关闭
//!
//! 任务模块公开的 API 不会阻止从关闭调用内部调用轮询或反之亦然, 因此这必须是安全的.无论哪种情况,
//! RUNNING 位域中的锁使内部调用立即返回.如果
//! 内部调用是 `shutdown` 调用, 则设置 CANCELLED 位, 并且
//! poll 调用将在轮询完成时注意到它, 并且任务在此时被取消.

// 此处有一些任务基础结构来支持 `JoinSet`, 它目前
// 不稳定.一旦 `JoinSet` 稳定下来, 就应该将其删除.
#![cfg_attr(not(tokio_unstable), allow(dead_code))]

mod core;
use self::core::Cell;
use self::core::Header;

mod error;
pub use self::error::JoinError;

mod harness;
use self::harness::Harness;

mod id;
#[cfg_attr(not(tokio_unstable), allow(unreachable_pub, unused_imports))]
pub use id::{id, try_id, Id};

#[cfg(feature = "rt")]
mod abort;
mod join;

#[cfg(feature = "rt")]
pub use self::abort::AbortHandle;

pub use self::join::JoinHandle;

mod list;
pub(crate) use self::list::{LocalOwnedTasks, OwnedTasks};

mod raw;
pub(crate) use self::raw::RawTask;

mod state;
use self::state::State;

mod waker;

cfg_taskdump! {
    pub(crate) mod trace;
}

use crate::future::Future;
use crate::util::linked_list;
use crate::util::sharded_list;

use crate::runtime::TaskCallback;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::{fmt, mem};

/// An owned handle to the task, tracked by ref count.
/// 该任务拥有的句柄,由引用计数跟踪.
#[repr(transparent)]
pub(crate) struct Task<S: 'static> {
    raw: RawTask,
    _p: PhantomData<S>,
}

unsafe impl<S> Send for Task<S> {}
unsafe impl<S> Sync for Task<S> {}

/// A task was notified.
/// 已通知的任务
#[repr(transparent)]
pub(crate) struct Notified<S: 'static>(Task<S>);

// safety: This type cannot be used to touch the task without first verifying
// that the value is on a thread where it is safe to poll the task.
unsafe impl<S: Schedule> Send for Notified<S> {}
unsafe impl<S: Schedule> Sync for Notified<S> {}

/// A non-Send variant of Notified with the invariant that it is on a thread
/// where it is safe to poll it.
/// !Send的Notified
#[repr(transparent)]
pub(crate) struct LocalNotified<S: 'static> {
    task: Task<S>,
    _not_send: PhantomData<*const ()>,
}

/// A task that is not owned by any `OwnedTasks`. Used for blocking tasks.
/// This type holds two ref-counts.
/// 不属于任何"OwnedTasks"的任务. 用于阻止任务. 此类型包含两个引用计数.
pub(crate) struct UnownedTask<S: 'static> {
    raw: RawTask,
    _p: PhantomData<S>,
}

// safety: This type can only be created given a Send task.
unsafe impl<S> Send for UnownedTask<S> {}
unsafe impl<S> Sync for UnownedTask<S> {}

/// Task result sent back.
pub(crate) type Result<T> = std::result::Result<T, JoinError>;

/// Hooks for scheduling tasks which are needed in the task harness.
/// 用于调度任务线束中所需的任务的挂钩.
#[derive(Clone)]
pub(crate) struct TaskHarnessScheduleHooks {
    pub(crate) task_terminate_callback: Option<TaskCallback>,
}

pub(crate) trait Schedule: Sync + Sized + 'static {
    /// The task has completed work and is ready to be released. The scheduler
    /// should release it immediately and return it. The task module will batch
    /// the ref-dec with setting other options.
    ///
    /// If the scheduler has already released the task, then None is returned.
    /// 任务已完成工作并准备释放. 调度程序应立即释放并返回. 任务模块将批量处理 ref-dec 并设置其他选项.
    /// 如果调度程序已释放任务, 则返回 None.
    fn release(&self, task: &Task<Self>) -> Option<Task<Self>>;

    /// Schedule the task
    /// 调度任务
    fn schedule(&self, task: Notified<Self>);

    fn hooks(&self) -> TaskHarnessScheduleHooks;

    /// Schedule the task to run in the near future, yielding the thread to
    /// other tasks.
    /// 调度任务在不久的将来运行,将线程让给其他任务.
    fn yield_now(&self, task: Notified<Self>) {
        self.schedule(task);
    }

    /// Polling the task resulted in a panic. Should the runtime shutdown?
    /// 轮询任务导致崩溃. 运行时是否应该关闭？
    fn unhandled_panic(&self) {
        // 默认情况下不执行任何操作. 这保留了 1.0 的行为.
        // By default, do nothing. This maintains the 1.0 behavior.
    }
}

cfg_rt! {
    /// This is the constructor for a new task. Three references to the task are
    /// created. The first task reference is usually put into an `OwnedTasks`
    /// immediately. The Notified is sent to the scheduler as an ordinary
    /// notification.
    /// 新任务的构造函数. 将创建对该任务的三个引用.
    fn new_task<T, S>(
        task: T,
        scheduler: S,
        id: Id,
    ) -> (Task<S>, Notified<S>, JoinHandle<T::Output>)
    where
        S: Schedule,
        T: Future + 'static,
        T::Output: 'static,
    {
        let raw = RawTask::new::<T, S>(task, scheduler, id);
        let task = Task {
            raw,
            _p: PhantomData,
        };
        let notified = Notified(Task {
            raw,
            _p: PhantomData,
        });
        let join = JoinHandle::new(raw);

        (task, notified, join)
    }

    /// Creates a new task with an associated join handle. This method is used
    /// only when the task is not going to be stored in an `OwnedTasks` list.
    ///
    /// Currently only blocking tasks use this method.
    /// 创建具有关联连接句柄的新任务. 仅当任务不会存储在"OwnedTasks"列表中时才使用此方法.
    /// 目前只有阻塞任务使用此方法.
    pub(crate) fn unowned<T, S>(task: T, scheduler: S, id: Id) -> (UnownedTask<S>, JoinHandle<T::Output>)
    where
        S: Schedule,
        T: Send + Future + 'static,
        T::Output: Send + 'static,
    {
        let (task, notified, join) = new_task(task, scheduler, id);

        // This transfers the ref-count of task and notified into an UnownedTask.
        // This is valid because an UnownedTask holds two ref-counts.
        // 将任务和通知的引用计数转移到 UnownedTask 中.这是有效的, 因为 UnownedTask 拥有两个引用计数.
        let unowned = UnownedTask {
            raw: task.raw,
            _p: PhantomData,
        };
        std::mem::forget(task);
        std::mem::forget(notified);

        (unowned, join)
    }
}

impl<S: 'static> Task<S> {
    unsafe fn new(raw: RawTask) -> Task<S> {
        Task {
            raw,
            _p: PhantomData,
        }
    }

    unsafe fn from_raw(ptr: NonNull<Header>) -> Task<S> {
        Task::new(RawTask::from_raw(ptr))
    }

    #[cfg(all(
        tokio_unstable,
        tokio_taskdump,
        feature = "rt",
        target_os = "linux",
        any(target_arch = "aarch64", target_arch = "x86", target_arch = "x86_64")
    ))]
    pub(super) fn as_raw(&self) -> RawTask {
        self.raw
    }

    fn header(&self) -> &Header {
        self.raw.header()
    }

    fn header_ptr(&self) -> NonNull<Header> {
        self.raw.header_ptr()
    }

    cfg_taskdump! {
        /// Notify the task for task dumping.
        ///
        /// Returns `None` if the task has already been notified.
        pub(super) fn notify_for_tracing(&self) -> Option<Notified<S>> {
            if self.as_raw().state().transition_to_notified_for_tracing() {
                // SAFETY: `transition_to_notified_for_tracing` increments the
                // refcount.
                Some(unsafe { Notified(Task::new(self.raw)) })
            } else {
                None
            }
        }

        /// Returns a [task ID] that uniquely identifies this task relative to other
        /// currently spawned tasks.
        ///
        /// [task ID]: crate::task::Id
        #[cfg(tokio_unstable)]
        #[cfg_attr(docsrs, doc(cfg(tokio_unstable)))]
        pub(crate) fn id(&self) -> crate::task::Id {
            // Safety: The header pointer is valid.
            unsafe { Header::get_id(self.raw.header_ptr()) }
        }
    }
}

impl<S: 'static> Notified<S> {
    fn header(&self) -> &Header {
        self.0.header()
    }
}

impl<S: 'static> Notified<S> {
    pub(crate) unsafe fn from_raw(ptr: RawTask) -> Notified<S> {
        Notified(Task::new(ptr))
    }
}

impl<S: 'static> Notified<S> {
    pub(crate) fn into_raw(self) -> RawTask {
        let raw = self.0.raw;
        mem::forget(self);
        raw
    }
}

impl<S: Schedule> Task<S> {
    /// Preemptively cancels the task as part of the shutdown process.
    /// 作为关闭过程的一部分,预先取消该任务.
    pub(crate) fn shutdown(self) {
        let raw = self.raw;
        mem::forget(self);
        raw.shutdown();
    }
}

impl<S: Schedule> LocalNotified<S> {
    /// Runs the task.
    /// 运行任务
    pub(crate) fn run(self) {
        let raw = self.task.raw;
        mem::forget(self);
        raw.poll();
    }
}

impl<S: Schedule> UnownedTask<S> {
    // Used in test of the inject queue.
    #[cfg(test)]
    #[cfg_attr(target_family = "wasm", allow(dead_code))]
    pub(super) fn into_notified(self) -> Notified<S> {
        Notified(self.into_task())
    }

    fn into_task(self) -> Task<S> {
        // Convert into a task.
        let task = Task {
            raw: self.raw,
            _p: PhantomData,
        };
        mem::forget(self);

        // Drop a ref-count since an UnownedTask holds two.
        // 由于 UnownedTask 拥有两个引用计数, 因此删除一个引用计数.
        task.header().state.ref_dec();

        task
    }

    pub(crate) fn run(self) {
        let raw = self.raw;
        mem::forget(self);

        // Transfer one ref-count to a Task object.
        // 将一个引用计数传送给 Task 对象.
        let task = Task::<S> {
            raw,
            _p: PhantomData,
        };

        // Use the other ref-count to poll the task.
        // 推动任务
        raw.poll();
        // Decrement our extra ref-count
        // 减少额外引用计数
        drop(task);
    }

    pub(crate) fn shutdown(self) {
        self.into_task().shutdown();
    }
}

impl<S: 'static> Drop for Task<S> {
    fn drop(&mut self) {
        // Decrement the ref count
        // 减少引用计数
        if self.header().state.ref_dec() {
            // Deallocate if this is the final ref count
            // 如果这是最终的引用计数,则释放
            self.raw.dealloc();
        }
    }
}

impl<S: 'static> Drop for UnownedTask<S> {
    fn drop(&mut self) {
        // Decrement the ref count
        if self.raw.header().state.ref_dec_twice() {
            // Deallocate if this is the final ref count
            // 如果这是最终的引用计数,则释放
            self.raw.dealloc();
        }
    }
}

impl<S> fmt::Debug for Task<S> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "Task({:p})", self.header())
    }
}

impl<S> fmt::Debug for Notified<S> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "task::Notified({:p})", self.0.header())
    }
}

/// # Safety
///
/// Tasks are pinned.
unsafe impl<S> linked_list::Link for Task<S> {
    type Handle = Task<S>;
    type Target = Header;

    fn as_raw(handle: &Task<S>) -> NonNull<Header> {
        handle.raw.header_ptr()
    }

    unsafe fn from_raw(ptr: NonNull<Header>) -> Task<S> {
        Task::from_raw(ptr)
    }

    unsafe fn pointers(target: NonNull<Header>) -> NonNull<linked_list::Pointers<Header>> {
        self::core::Trailer::addr_of_owned(Header::get_trailer(target))
    }
}

/// # Safety
///
/// The id of a task is never changed after creation of the task, so the return value of
/// `get_shard_id` will not change. (The cast may throw away the upper 32 bits of the task id, but
/// the shard id still won't change from call to call.)
/// 任务创建后,其 ID 永远不会改变,因此 `get_shard_id` 的返回值不会改变.(转换可能会丢弃任务 ID 的高 32 位,但共享 ID 仍然不会在每次调用时改变.)
unsafe impl<S> sharded_list::ShardedListItem for Task<S> {
    unsafe fn get_shard_id(target: NonNull<Self::Target>) -> usize {
        // SAFETY: The caller guarantees that `target` points at a valid task.
        let task_id = unsafe { Header::get_id(target) };
        task_id.0.get() as usize
    }
}
