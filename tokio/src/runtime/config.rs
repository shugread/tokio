#![cfg_attr(
    any(not(all(tokio_unstable, feature = "full")), target_family = "wasm"),
    allow(dead_code)
)]
use crate::runtime::{Callback, TaskCallback};
use crate::util::RngSeedGenerator;

pub(crate) struct Config {
    /// How many ticks before pulling a task from the global/remote queue?
    /// 从全局/远程队列中提取任务之前需要多少个刻度?
    pub(crate) global_queue_interval: Option<u32>,

    /// How many ticks before yielding to the driver for timer and I/O events?
    /// 在将计时器和 I/O 事件交给驱动程序之前需要多少个刻度?
    pub(crate) event_interval: u32,

    /// How big to make each worker's local queue
    /// 每个 worker 的本地队列有多大
    pub(crate) local_queue_capacity: usize,

    /// Callback for a worker parking itself
    /// worker挂起回调
    pub(crate) before_park: Option<Callback>,

    /// Callback for a worker unparking itself
    /// woker恢复回调
    pub(crate) after_unpark: Option<Callback>,

    /// To run before each task is spawned.
    /// 任务进入回调
    pub(crate) before_spawn: Option<TaskCallback>,

    /// To run after each task is terminated.
    /// 任务中断后回调
    pub(crate) after_termination: Option<TaskCallback>,

    /// The multi-threaded scheduler includes a per-worker LIFO slot used to
    /// store the last scheduled task. This can improve certain usage patterns,
    /// especially message passing between tasks. However, this LIFO slot is not
    /// currently stealable.
    ///
    /// Eventually, the LIFO slot **will** become stealable, however as a
    /// stop-gap, this unstable option lets users disable the LIFO task.
    /// 多线程调度程序包含一个用于存储最后调度任务的每个工作程序 LIFO 槽.
    /// 这可以改善某些使用模式,尤其是任务之间的消息传递.
    pub(crate) disable_lifo_slot: bool,

    /// Random number generator seed to configure runtimes to act in a
    /// deterministic way.
    /// 随机数生成器种子用于配置运行时以确定性的方式运行.
    pub(crate) seed_generator: RngSeedGenerator,

    /// How to build poll time histograms
    pub(crate) metrics_poll_count_histogram: Option<crate::runtime::HistogramBuilder>,

    #[cfg(tokio_unstable)]
    /// How to respond to unhandled task panics.
    pub(crate) unhandled_panic: crate::runtime::UnhandledPanic,
}
