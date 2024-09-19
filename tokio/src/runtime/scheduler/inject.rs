//! Inject queue used to send wakeups to a work-stealing scheduler

use crate::loom::sync::Mutex;
use crate::runtime::task;

mod pop;
pub(crate) use pop::Pop;

mod shared;
pub(crate) use shared::Shared;

mod synced;
pub(crate) use synced::Synced;

cfg_rt_multi_thread! {
    mod rt_multi_thread;
}

cfg_unstable_metrics! {
    mod metrics;
}

/// Growable, MPMC queue used to inject new tasks into the scheduler and as an
/// overflow queue when the local, fixed-size, array queue overflows.
/// 可增长的 MPMC 队列用于将新任务注入调度程序,
/// 并在本地固定大小数组队列溢出时作为溢出队列.
pub(crate) struct Inject<T: 'static> {
    // 负责存储实际的任务队列和管理队列的核心逻辑
    shared: Shared<T>,
    // 用于保护同步的 Synced 状态,保证在多线程环境下对共享队列的访问是安全的.
    synced: Mutex<Synced>,
}

impl<T: 'static> Inject<T> {
    pub(crate) fn new() -> Inject<T> {
        let (shared, synced) = Shared::new();

        Inject {
            shared,
            synced: Mutex::new(synced),
        }
    }

    // Kind of annoying to have to include the cfg here
    #[cfg(tokio_taskdump)]
    pub(crate) fn is_closed(&self) -> bool {
        let synced = self.synced.lock();
        self.shared.is_closed(&synced)
    }

    /// Closes the injection queue, returns `true` if the queue is open when the
    /// transition is made.
    /// 关闭注入队列,如果在进行转换时队列处于打开状态,则返回`true`.
    pub(crate) fn close(&self) -> bool {
        let mut synced = self.synced.lock();
        self.shared.close(&mut synced)
    }

    /// Pushes a value into the queue.
    ///
    /// This does nothing if the queue is closed.
    /// 将任务压入队列
    pub(crate) fn push(&self, task: task::Notified<T>) {
        let mut synced = self.synced.lock();
        // safety: passing correct `Synced`
        unsafe { self.shared.push(&mut synced, task) }
    }

    // 从队列获取任务
    pub(crate) fn pop(&self) -> Option<task::Notified<T>> {
        if self.shared.is_empty() {
            return None;
        }

        let mut synced = self.synced.lock();
        // safety: passing correct `Synced`
        unsafe { self.shared.pop(&mut synced) }
    }
}
