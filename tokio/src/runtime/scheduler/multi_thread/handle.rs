use crate::future::Future;
use crate::loom::sync::Arc;
use crate::runtime::scheduler::multi_thread::worker;
use crate::runtime::{
    blocking, driver,
    task::{self, JoinHandle},
    TaskHooks, TaskMeta,
};
use crate::util::RngSeedGenerator;

use std::fmt;

mod metrics;

cfg_taskdump! {
    mod taskdump;
}

/// Handle to the multi thread scheduler
/// 多线程调度器句柄
pub(crate) struct Handle {
    /// Task spawner
    /// woker共享数据
    pub(super) shared: worker::Shared,

    /// Resource driver handles
    /// 资源驱动器的句柄,用于与系统 I/O,定时器等资源交互.
    pub(crate) driver: driver::Handle,

    /// Blocking pool spawner
    /// 阻塞任务的生成器,管理阻塞任务的执行.
    pub(crate) blocking_spawner: blocking::Spawner,

    /// Current random number generator seed
    /// 当前的随机数生成器种子
    pub(crate) seed_generator: RngSeedGenerator,

    /// User-supplied hooks to invoke for things
    /// 用户提供的任务钩子
    pub(crate) task_hooks: TaskHooks,
}

impl Handle {
    /// Spawns a future onto the thread pool
    /// 将Future加入线程池
    pub(crate) fn spawn<F>(me: &Arc<Self>, future: F, id: task::Id) -> JoinHandle<F::Output>
    where
        F: crate::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        Self::bind_new_task(me, future, id)
    }

    pub(crate) fn shutdown(&self) {
        self.close();
    }

    pub(super) fn bind_new_task<T>(me: &Arc<Self>, future: T, id: task::Id) -> JoinHandle<T::Output>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        // 绑定任务
        let (handle, notified) = me.shared.owned.bind(future, me.clone(), id);

        me.task_hooks.spawn(&TaskMeta {
            #[cfg(tokio_unstable)]
            id,
            _phantom: Default::default(),
        });

        // 将任务加入调度队列
        me.schedule_option_task_without_yield(notified);

        handle
    }
}

cfg_unstable! {
    use std::num::NonZeroU64;

    impl Handle {
        pub(crate) fn owned_id(&self) -> NonZeroU64 {
            self.shared.owned.id
        }
    }
}

impl fmt::Debug for Handle {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("multi_thread::Handle { ... }").finish()
    }
}
