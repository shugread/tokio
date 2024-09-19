use crate::runtime::{Config, MetricsBatch, WorkerMetrics};

use std::time::{Duration, Instant};

/// Per-worker statistics. This is used for both tuning the scheduler and
/// reporting runtime-level metrics/stats.
/// 追踪每个工作线程的统计信息
pub(crate) struct Stats {
    /// The metrics batch used to report runtime-level metrics/stats to the
    /// user.
    /// 度量数据的批处理结构,用于收集并报告与工作线程相关的统计信息
    batch: MetricsBatch,

    /// Instant at which work last resumed (continued after park).
    ///
    /// This duplicates the value stored in `MetricsBatch`. We will unify
    /// `Stats` and `MetricsBatch` when we stabilize metrics.
    /// 记录上次任务处理开始的时间
    processing_scheduled_tasks_started_at: Instant,

    /// Number of tasks polled in the batch of scheduled tasks
    /// 当前批处理中被轮询的任务数
    tasks_polled_in_batch: usize,

    /// Exponentially-weighted moving average of time spent polling scheduled a
    /// task.
    ///
    /// Tracked in nanoseconds, stored as a `f64` since that is what we use with
    /// the EWMA calculations
    /// 任务轮询时间的指数加权移动平均数(EWMA),单位是纳秒
    task_poll_time_ewma: f64,
}

/// How to weigh each individual poll time, value is plucked from thin air.
/// 控制单个轮询时间对加权移动平均数的影响程度
const TASK_POLL_TIME_EWMA_ALPHA: f64 = 0.1;

/// Ideally, we wouldn't go above this, value is plucked from thin air.
/// 全局队列轮询间隔,单位为纳秒
const TARGET_GLOBAL_QUEUE_INTERVAL: f64 = Duration::from_micros(200).as_nanos() as f64;

/// Max value for the global queue interval. This is 2x the previous default
/// 在全局队列轮询间隔内允许轮询的最大任务数
const MAX_TASKS_POLLED_PER_GLOBAL_QUEUE_INTERVAL: u32 = 127;

/// This is the previous default
/// 目标的每个全局队列轮询间隔内轮询的任务数量
const TARGET_TASKS_POLLED_PER_GLOBAL_QUEUE_INTERVAL: u32 = 61;

impl Stats {
    pub(crate) fn new(worker_metrics: &WorkerMetrics) -> Stats {
        // Seed the value with what we hope to see.
        let task_poll_time_ewma =
            TARGET_GLOBAL_QUEUE_INTERVAL / TARGET_TASKS_POLLED_PER_GLOBAL_QUEUE_INTERVAL as f64;

        Stats {
            batch: MetricsBatch::new(worker_metrics),
            processing_scheduled_tasks_started_at: Instant::now(),
            tasks_polled_in_batch: 0,
            task_poll_time_ewma,
        }
    }

    // 动态调整全局队列的轮询间隔
    pub(crate) fn tuned_global_queue_interval(&self, config: &Config) -> u32 {
        // If an interval is explicitly set, don't tune.
        if let Some(configured) = config.global_queue_interval {
            return configured;
        }

        // As of Rust 1.45, casts from f64 -> u32 are saturating, which is fine here.
        let tasks_per_interval = (TARGET_GLOBAL_QUEUE_INTERVAL / self.task_poll_time_ewma) as u32;

        // If we are using self-tuning, we don't want to return less than 2 as that would result in the
        // global queue always getting checked first.
        tasks_per_interval.clamp(2, MAX_TASKS_POLLED_PER_GLOBAL_QUEUE_INTERVAL)
    }

    // 提交当前批处理中的统计数据
    pub(crate) fn submit(&mut self, to: &WorkerMetrics) {
        self.batch.submit(to, self.task_poll_time_ewma as u64);
    }

    // 准备进入休眠(Park)时调用
    pub(crate) fn about_to_park(&mut self) {
        self.batch.about_to_park();
    }

    // 从休眠状态被唤醒时调用
    pub(crate) fn unparked(&mut self) {
        self.batch.unparked();
    }

    // 增加本地任务调度计数
    pub(crate) fn inc_local_schedule_count(&mut self) {
        self.batch.inc_local_schedule_count();
    }

    // 开始处理已调度的任务时调用
    pub(crate) fn start_processing_scheduled_tasks(&mut self) {
        self.batch.start_processing_scheduled_tasks();

        self.processing_scheduled_tasks_started_at = Instant::now();
        self.tasks_polled_in_batch = 0;
    }

    // 结束处理任务时调用
    pub(crate) fn end_processing_scheduled_tasks(&mut self) {
        self.batch.end_processing_scheduled_tasks();

        // Update the EWMA task poll time
        if self.tasks_polled_in_batch > 0 {
            let now = Instant::now();

            // If we "overflow" this conversion, we have bigger problems than
            // slightly off stats.
            let elapsed = (now - self.processing_scheduled_tasks_started_at).as_nanos() as f64;
            let num_polls = self.tasks_polled_in_batch as f64;

            // Calculate the mean poll duration for a single task in the batch
            let mean_poll_duration = elapsed / num_polls;

            // Compute the alpha weighted by the number of tasks polled this batch.
            let weighted_alpha = 1.0 - (1.0 - TASK_POLL_TIME_EWMA_ALPHA).powf(num_polls);

            // Now compute the new weighted average task poll time.
            self.task_poll_time_ewma = weighted_alpha * mean_poll_duration
                + (1.0 - weighted_alpha) * self.task_poll_time_ewma;
        }
    }

    // 开始轮询一个任务时调用
    pub(crate) fn start_poll(&mut self) {
        self.batch.start_poll();

        self.tasks_polled_in_batch += 1;
    }

    // 结束轮询一个任务时调用
    pub(crate) fn end_poll(&mut self) {
        self.batch.end_poll();
    }

    // 增加任务被偷取的数量
    pub(crate) fn incr_steal_count(&mut self, by: u16) {
        self.batch.incr_steal_count(by);
    }

    // 增加任务偷取的操作数
    pub(crate) fn incr_steal_operations(&mut self) {
        self.batch.incr_steal_operations();
    }

    // 增加溢出任务计数
    pub(crate) fn incr_overflow_count(&mut self) {
        self.batch.incr_overflow_count();
    }
}
