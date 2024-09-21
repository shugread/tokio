//! A scheduler is initialized with a fixed number of workers. Each worker is
//! driven by a thread. Each worker has a "core" which contains data such as the
//! run queue and other state. When `block_in_place` is called, the worker's
//! "core" is handed off to a new thread allowing the scheduler to continue to
//! make progress while the originating thread blocks.
//!
//! # Shutdown
//!
//! Shutting down the runtime involves the following steps:
//!
//!  1. The Shared::close method is called. This closes the inject queue and
//!     `OwnedTasks` instance and wakes up all worker threads.
//!
//!  2. Each worker thread observes the close signal next time it runs
//!     Core::maintenance by checking whether the inject queue is closed.
//!     The `Core::is_shutdown` flag is set to true.
//!
//!  3. The worker thread calls `pre_shutdown` in parallel. Here, the worker
//!     will keep removing tasks from `OwnedTasks` until it is empty. No new
//!     tasks can be pushed to the `OwnedTasks` during or after this step as it
//!     was closed in step 1.
//!
//!  5. The workers call Shared::shutdown to enter the single-threaded phase of
//!     shutdown. These calls will push their core to `Shared::shutdown_cores`,
//!     and the last thread to push its core will finish the shutdown procedure.
//!
//!  6. The local run queue of each core is emptied, then the inject queue is
//!     emptied.
//!
//! At this point, shutdown has completed. It is not possible for any of the
//! collections to contain any tasks at this point, as each collection was
//! closed first, then emptied afterwards.
//!
//! ## Spawns during shutdown
//!
//! When spawning tasks during shutdown, there are two cases:
//!
//!  * The spawner observes the `OwnedTasks` being open, and the inject queue is
//!    closed.
//!  * The spawner observes the `OwnedTasks` being closed and doesn't check the
//!    inject queue.
//!
//! The first case can only happen if the `OwnedTasks::bind` call happens before
//! or during step 1 of shutdown. In this case, the runtime will clean up the
//! task in step 3 of shutdown.
//!
//! In the latter case, the task was not spawned and the task is immediately
//! cancelled by the spawner.
//!
//! The correctness of shutdown requires both the inject queue and `OwnedTasks`
//! collection to have a closed bit. With a close bit on only the inject queue,
//! spawning could run in to a situation where a task is successfully bound long
//! after the runtime has shut down. With a close bit on only the `OwnedTasks`,
//! the first spawning situation could result in the notification being pushed
//! to the inject queue after step 6 of shutdown, which would leave a task in
//! the inject queue indefinitely. This would be a ref-count cycle and a memory
//! leak.

use crate::loom::sync::{Arc, Mutex};
use crate::runtime;
use crate::runtime::scheduler::multi_thread::{
    idle, queue, Counters, Handle, Idle, Overflow, Parker, Stats, TraceStatus, Unparker,
};
use crate::runtime::scheduler::{inject, Defer, Lock};
use crate::runtime::task::{OwnedTasks, TaskHarnessScheduleHooks};
use crate::runtime::{
    blocking, coop, driver, scheduler, task, Config, SchedulerMetrics, WorkerMetrics,
};
use crate::runtime::{context, TaskHooks};
use crate::util::atomic_cell::AtomicCell;
use crate::util::rand::{FastRand, RngSeedGenerator};

use std::cell::RefCell;
use std::task::Waker;
use std::thread;
use std::time::Duration;

cfg_unstable_metrics! {
    mod metrics;
}

cfg_taskdump! {
    mod taskdump;
}

cfg_not_taskdump! {
    mod taskdump_mock;
}

/// A scheduler worker
/// 调度器的一个工作线程
pub(super) struct Worker {
    /// Reference to scheduler's handle
    /// 调度器的句柄,允许工作线程与调度器共享状态进行通信
    handle: Arc<Handle>,

    /// Index holding this worker's remote state
    /// 当前工作线程在调度器中对应的索引
    index: usize,

    /// Used to hand-off a worker's core to another thread.
    /// 工作线程的核心状态, 包含调度任务所需的数据
    core: AtomicCell<Core>,
}

/// Core data
struct Core {
    /// Used to schedule bookkeeping tasks every so often.
    /// 用于调度的计数器
    tick: u32,

    /// When a task is scheduled from a worker, it is stored in this slot. The
    /// worker will check this slot for a task **before** checking the run
    /// queue. This effectively results in the **last** scheduled task to be run
    /// next (LIFO). This is an optimization for improving locality which
    /// benefits message passing patterns and helps to reduce latency.
    /// 存储最近调度的任务,优化任务执行的局部性.
    lifo_slot: Option<Notified>,

    /// When `true`, locally scheduled tasks go to the LIFO slot. When `false`,
    /// they go to the back of the `run_queue`.
    /// 是否启用 LIFO 调度,控制任务是否被添加到 LIFO 槽中
    lifo_enabled: bool,

    /// The worker-local run queue.
    /// 本地任务队列,用于存储任务,任务优先从本地队列中获取.
    run_queue: queue::Local<Arc<Handle>>,

    /// True if the worker is currently searching for more work. Searching
    /// involves attempting to steal from other workers.
    /// 当前工作线程是否正在搜索任务
    is_searching: bool,

    /// True if the scheduler is being shutdown
    /// 调度器是否正在关闭
    is_shutdown: bool,

    /// True if the scheduler is being traced
    /// 如果正在跟踪调度程序,则为 True
    is_traced: bool,

    /// Parker
    ///
    /// Stored in an `Option` as the parker is added / removed to make the
    /// borrow checker happy.
    /// 当前线程的 parker,用于管理线程的休眠和唤醒
    park: Option<Parker>,

    /// Per-worker runtime stats
    /// 运行状态
    stats: Stats,

    /// How often to check the global queue
    /// 定期检查全局队列的间隔
    global_queue_interval: u32,

    /// Fast random number generator.
    /// 工作线程的本地随机数生成器
    rand: FastRand,
}

/// State shared across all workers
/// 所有工作线程共享的状态信息
pub(crate) struct Shared {
    /// Per-worker remote state. All other workers have access to this and is
    /// how they communicate between each other.
    /// 其他线程用于与工作线程通信的远程句柄数组
    remotes: Box<[Remote]>,

    /// Global task queue used for:
    ///  1. Submit work to the scheduler while **not** currently on a worker thread.
    ///  2. Submit work to the scheduler when a worker run queue is saturated
    ///
    /// 全局任务注入队列,任务可以在不属于任何工作线程的情况下被注入调度器.
    pub(super) inject: inject::Shared<Arc<Handle>>,

    /// Coordinates idle workers
    /// 管理空闲工作线程的状态
    idle: Idle,

    /// Collection of all active tasks spawned onto this executor.
    /// 所有已生成的任务集合
    pub(crate) owned: OwnedTasks<Arc<Handle>>,

    /// Data synchronized by the scheduler mutex
    /// 共享状态
    pub(super) synced: Mutex<Synced>,

    /// Cores that have observed the shutdown signal
    ///
    /// The core is **not** placed back in the worker to avoid it from being
    /// stolen by a thread that was spawned as part of `block_in_place`.
    #[allow(clippy::vec_box)] // we're moving an already-boxed value
    // 保存已经观察到关闭信号的核心状态
    shutdown_cores: Mutex<Vec<Box<Core>>>,

    /// The number of cores that have observed the trace signal.
    /// 追踪调度器的状态
    pub(super) trace_status: TraceStatus,

    /// Scheduler configuration options
    /// 调度器的配置选项
    config: Config,

    /// Collects metrics from the runtime.
    /// 收集调度器的运行时度量和性能指标
    pub(super) scheduler_metrics: SchedulerMetrics,

    // 每个工作线程的度量信息
    pub(super) worker_metrics: Box<[WorkerMetrics]>,

    /// Only held to trigger some code on drop. This is used to get internal
    /// runtime metrics that can be useful when doing performance
    /// investigations. This does nothing (empty struct, no drop impl) unless
    /// the `tokio_internal_mt_counters` `cfg` flag is set.
    _counters: Counters,
}

/// Data synchronized by the scheduler mutex
pub(crate) struct Synced {
    /// Synchronized state for `Idle`.
    /// 用于管理空闲线程的同步状态
    pub(super) idle: idle::Synced,

    /// Synchronized state for `Inject`.
    /// 用于管理全局任务注入队列的同步状态
    pub(crate) inject: inject::Synced,
}

/// Used to communicate with a worker from other threads.
/// 用于与其他线程的工作程序进行通信.
struct Remote {
    /// Steals tasks from this worker.
    /// 允许其他工作线程从当前工作线程中窃取任务
    pub(super) steal: queue::Steal<Arc<Handle>>,

    /// Unparks the associated worker thread
    /// 用于唤醒与当前工作线程关联的工作线程
    unpark: Unparker,
}

/// Thread-local context
pub(crate) struct Context {
    /// Worker
    /// 前线程的 Worker 实例
    worker: Arc<Worker>,

    /// Core data
    /// 当前线程的核心状态
    core: RefCell<Option<Box<Core>>>,

    /// Tasks to wake after resource drivers are polled. This is mostly to
    /// handle yielded tasks.
    /// 用于推迟任务执行的任务队列
    pub(crate) defer: Defer,
}

/// Starts the workers
/// 开启worker
pub(crate) struct Launch(Vec<Arc<Worker>>);

/// Running a task may consume the core. If the core is still available when
/// running the task completes, it is returned. Otherwise, the worker will need
/// to stop processing.
type RunResult = Result<Box<Core>, ()>;

/// A task handle
type Task = task::Task<Arc<Handle>>;

/// A notified task handle
type Notified = task::Notified<Arc<Handle>>;

/// Value picked out of thin-air. Running the LIFO slot a handful of times
/// seems sufficient to benefit from locality. More than 3 times probably is
/// overweighing. The value can be tuned in the future with data that shows
/// improvements.
const MAX_LIFO_POLLS_PER_TICK: usize = 3;

pub(super) fn create(
    size: usize,
    park: Parker,
    driver_handle: driver::Handle,
    blocking_spawner: blocking::Spawner,
    seed_generator: RngSeedGenerator,
    config: Config,
) -> (Arc<Handle>, Launch) {
    let mut cores = Vec::with_capacity(size);
    let mut remotes = Vec::with_capacity(size); // 包含所有Worker的任务
    let mut worker_metrics = Vec::with_capacity(size);

    // Create the local queues
    for _ in 0..size {
        // 任务队列
        let (steal, run_queue) = queue::local();

        let park = park.clone(); // 挂起
        let unpark = park.unpark(); // 恢复
        let metrics = WorkerMetrics::from_config(&config);
        let stats = Stats::new(&metrics);

        cores.push(Box::new(Core {
            tick: 0,
            lifo_slot: None,
            lifo_enabled: !config.disable_lifo_slot,
            run_queue,
            is_searching: false,
            is_shutdown: false,
            is_traced: false,
            park: Some(park),
            global_queue_interval: stats.tuned_global_queue_interval(&config),
            stats,
            rand: FastRand::from_seed(config.seed_generator.next_seed()),
        }));

        remotes.push(Remote { steal, unpark });
        worker_metrics.push(metrics);
    }

    let (idle, idle_synced) = Idle::new(size);
    let (inject, inject_synced) = inject::Shared::new();

    let remotes_len = remotes.len();
    let handle = Arc::new(Handle {
        task_hooks: TaskHooks {
            task_spawn_callback: config.before_spawn.clone(),
            task_terminate_callback: config.after_termination.clone(),
        },
        shared: Shared {
            remotes: remotes.into_boxed_slice(),
            inject,
            idle,
            owned: OwnedTasks::new(size),
            synced: Mutex::new(Synced {
                idle: idle_synced,
                inject: inject_synced,
            }),
            shutdown_cores: Mutex::new(vec![]),
            trace_status: TraceStatus::new(remotes_len),
            config,
            scheduler_metrics: SchedulerMetrics::new(),
            worker_metrics: worker_metrics.into_boxed_slice(),
            _counters: Counters,
        },
        driver: driver_handle,
        blocking_spawner,
        seed_generator,
    });

    // Worker启动器
    let mut launch = Launch(vec![]);

    for (index, core) in cores.drain(..).enumerate() {
        launch.0.push(Arc::new(Worker {
            handle: handle.clone(),
            index,
            core: AtomicCell::new(Some(core)),
        }));
    }

    (handle, launch)
}

#[track_caller]
pub(crate) fn block_in_place<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    // Try to steal the worker core back
    // 在 block_in_place 执行完毕时恢复线程的工作状态
    struct Reset {
        take_core: bool,
        budget: coop::Budget,
    }

    impl Drop for Reset {
        fn drop(&mut self) {
            with_current(|maybe_cx| {
                if let Some(cx) = maybe_cx {
                    if self.take_core {
                        let core = cx.worker.core.take();

                        if core.is_some() {
                            cx.worker.handle.shared.worker_metrics[cx.worker.index]
                                .set_thread_id(thread::current().id());
                        }

                        let mut cx_core = cx.core.borrow_mut();
                        assert!(cx_core.is_none());
                        *cx_core = core;
                    }

                    // Reset the task budget as we are re-entering the
                    // runtime.
                    coop::set(self.budget);
                }
            });
        }
    }

    let mut had_entered = false;
    let mut take_core = false;

    // with_current 函数检查当前是否已经进入了 tokio 运行时
    let setup_result = with_current(|maybe_cx| {
        match (
            crate::runtime::context::current_enter_context(),
            maybe_cx.is_some(),
        ) {
            (context::EnterRuntime::Entered { .. }, true) => {
                // We are on a thread pool runtime thread, so we just need to
                // set up blocking.
                // 处于线程池运行时线程中，因此只需要设置阻塞.
                had_entered = true;
            }
            (
                context::EnterRuntime::Entered {
                    allow_block_in_place,
                },
                false,
            ) => {
                // We are on an executor, but _not_ on the thread pool.  That is
                // _only_ okay if we are in a thread pool runtime's block_on
                // method:
                // 我们在执行器上,但不在线程池上.只有当我们在线程池运行时的 block_on 方法中时才可以
                if allow_block_in_place {
                    had_entered = true;
                    return Ok(());
                } else {
                    // This probably means we are on the current_thread runtime or in a
                    // LocalSet, where it is _not_ okay to block.
                    // 这可能意味着我们处于 current_thread 运行时或 LocalSet 中,不适合阻塞.
                    return Err(
                        "can call blocking only when running on the multi-threaded runtime",
                    );
                }
            }
            (context::EnterRuntime::NotEntered, true) => {
                // This is a nested call to block_in_place (we already exited).
                // All the necessary setup has already been done.
                // 这是对 block_in_place 的嵌套调用(我们已经退出).所有必要的设置都已完成.
                return Ok(());
            }
            (context::EnterRuntime::NotEntered, false) => {
                // We are outside of the tokio runtime, so blocking is fine.
                // We can also skip all of the thread pool blocking setup steps.
                // 在 tokio 运行时之外
                return Ok(());
            }
        }

        let cx = maybe_cx.expect("no .is_some() == false cases above should lead here");

        // Get the worker core. If none is set, then blocking is fine!
        //  获取工作核心.如果没有设置,则阻塞即可!
        let mut core = match cx.core.borrow_mut().take() {
            Some(core) => core,
            None => return Ok(()),
        };

        // If we heavily call `spawn_blocking`, there might be no available thread to
        // run this core. Except for the task in the lifo_slot, all tasks can be
        // stolen, so we move the task out of the lifo_slot to the run_queue.
        // 如果我们大量调用 `spawn_blocking`,可能没有可用的线程来运行此核心.
        // 除了 lifo_slot 中的任务外,所有任务都可以被窃取,因此我们将任务从 lifo_slot 移出到 run_queue.
        if let Some(task) = core.lifo_slot.take() {
            core.run_queue
                .push_back_or_overflow(task, &*cx.worker.handle, &mut core.stats);
        }

        // We are taking the core from the context and sending it to another
        // thread.
        // 我们正在从上下文中获取核心并将其发送到另一个线程.
        take_core = true;

        // The parker should be set here
        assert!(core.park.is_some());

        // In order to block, the core must be sent to another thread for
        // execution.
        //
        // First, move the core back into the worker's shared core slot.
        cx.worker.core.set(core);

        // Next, clone the worker handle and send it to a new thread for
        // processing.
        //
        // Once the blocking task is done executing, we will attempt to
        // steal the core back.
        // 一旦阻塞任务执行完毕，我们将尝试窃回核心.
        let worker = cx.worker.clone();
        runtime::spawn_blocking(move || run(worker));
        Ok(())
    });

    if let Err(panic_message) = setup_result {
        panic!("{}", panic_message);
    }

    if had_entered {
        // Unset the current task's budget. Blocking sections are not
        // constrained by task budgets.
        // 取消设置当前任务的预算.阻塞部分不受任务预算的限制.
        let _reset = Reset {
            take_core,
            budget: coop::stop(),
        };

        crate::runtime::context::exit_runtime(f)
    } else {
        f()
    }
}

impl Launch {
    pub(crate) fn launch(mut self) {
        // 开启worker
        for worker in self.0.drain(..) {
            runtime::spawn_blocking(move || run(worker));
        }
    }
}

/// 开启线程并运行开启worker
fn run(worker: Arc<Worker>) {
    /// 捕获panic
    #[allow(dead_code)]
    struct AbortOnPanic;

    impl Drop for AbortOnPanic {
        fn drop(&mut self) {
            if std::thread::panicking() {
                eprintln!("worker thread panicking; aborting process");
                std::process::abort();
            }
        }
    }

    // Catching panics on worker threads in tests is quite tricky. Instead, when
    // debug assertions are enabled, we just abort the process.
    #[cfg(debug_assertions)]
    let _abort_on_panic = AbortOnPanic;

    // Acquire a core. If this fails, then another thread is running this
    // worker and there is nothing further to do.
    // 获取核心.如果失败,则另一个线程正在运行此worker,并且无需执行其他操作.
    let core = match worker.core.take() {
        Some(core) => core,
        None => return,
    };

    worker.handle.shared.worker_metrics[worker.index].set_thread_id(thread::current().id());

    let handle = scheduler::Handle::MultiThread(worker.handle.clone());

    crate::runtime::context::enter_runtime(&handle, true, |_| {
        // Set the worker context.
        let cx = scheduler::Context::MultiThread(Context {
            worker,
            core: RefCell::new(None),
            defer: Defer::new(),
        });

        // 设置当前线程的调度器为cx, 并且只在f运行期间有效
        context::set_scheduler(&cx, || {
            let cx = cx.expect_multi_thread();

            // This should always be an error. It only returns a `Result` to support
            // using `?` to short circuit.
            assert!(cx.run(core).is_err());

            // Check if there are any deferred tasks to notify. This can happen when
            // the worker core is lost due to `block_in_place()` being called from
            // within the task.
            // 检查是否有任何需要通知的延迟任务
            cx.defer.wake();
        });
    });
}

impl Context {
    fn run(&self, mut core: Box<Core>) -> RunResult {
        // Reset `lifo_enabled` here in case the core was previously stolen from
        // a task that had the LIFO slot disabled.
        // 如果核心先前被从禁用了 LIFO 槽的任务中窃取,请在此处重置 `lifo_enabled`.
        self.reset_lifo_enabled(&mut core);

        // Start as "processing" tasks as polling tasks from the local queue
        // will be one of the first things we do.
        core.stats.start_processing_scheduled_tasks();

        // 没有停止时, 一直在循环
        while !core.is_shutdown {
            self.assert_lifo_enabled_is_correct(&core);

            if core.is_traced {
                core = self.worker.handle.trace_core(core);
            }

            // Increment the tick
            // tick加1
            core.tick();

            // Run maintenance, if needed
            core = self.maintenance(core);

            // First, check work available to the current worker.
            // 获取当前worker的任务
            if let Some(task) = core.next_task(&self.worker) {
                core = self.run_task(task, core)?;
                continue;
            }

            // We consumed all work in the queues and will start searching for work.
            core.stats.end_processing_scheduled_tasks();

            // There is no more **local** work to process, try to steal work
            // from other workers.
            // 获取其他线程的任务
            if let Some(task) = core.steal_work(&self.worker) {
                // Found work, switch back to processing
                core.stats.start_processing_scheduled_tasks();
                core = self.run_task(task, core)?;
            } else {
                // Wait for work
                // 等待任务
                core = if !self.defer.is_empty() {
                    self.park_timeout(core, Some(Duration::from_millis(0)))
                } else {
                    self.park(core)
                };
                core.stats.start_processing_scheduled_tasks();
            }
        }

        core.pre_shutdown(&self.worker);
        // Signal shutdown
        self.worker.handle.shutdown_core(core);
        Err(())
    }

    // 运行任务
    fn run_task(&self, task: Notified, mut core: Box<Core>) -> RunResult {
        let task = self.worker.handle.shared.owned.assert_owner(task);

        // Make sure the worker is not in the **searching** state. This enables
        // another idle worker to try to steal work.
        // 确保core不是searching状态
        core.transition_from_searching(&self.worker);

        self.assert_lifo_enabled_is_correct(&core);

        // Measure the poll start time. Note that we may end up polling other
        // tasks under this measurement. In this case, the tasks came from the
        // LIFO slot and are considered part of the current task for scheduling
        // purposes. These tasks inherent the "parent"'s limits.
        // 测量轮询开始时间
        core.stats.start_poll();

        // Make the core available to the runtime context
        // 使核心可用于运行时上下文
        *self.core.borrow_mut() = Some(core);

        // Run the task
        coop::budget(|| {
            // 运行任务
            task.run();
            let mut lifo_polls = 0;

            // As long as there is budget remaining and a task exists in the
            // `lifo_slot`, then keep running.
            // 只要还有预算剩余,并且 `lifo_slot` 中存在任务,则继续运行.
            loop {
                // Check if we still have the core. If not, the core was stolen
                // by another worker.
                // 检查我们是否还拥有核心.如果没有,则核心已被其他worker偷走.
                let mut core = match self.core.borrow_mut().take() {
                    Some(core) => core,
                    None => {
                        // In this case, we cannot call `reset_lifo_enabled()`
                        // because the core was stolen. The stealer will handle
                        // that at the top of `Context::run`
                        return Err(());
                    }
                };

                // Check for a task in the LIFO slot
                // 检查 LIFO 槽中的任务
                let task = match core.lifo_slot.take() {
                    Some(task) => task,
                    None => {
                        self.reset_lifo_enabled(&mut core);
                        core.stats.end_poll();
                        return Ok(core);
                    }
                };

                // 没有budget, 结束运行
                if !coop::has_budget_remaining() {
                    core.stats.end_poll();

                    // Not enough budget left to run the LIFO task, push it to
                    // the back of the queue and return.
                    core.run_queue.push_back_or_overflow(
                        task,
                        &*self.worker.handle,
                        &mut core.stats,
                    );
                    // If we hit this point, the LIFO slot should be enabled.
                    // There is no need to reset it.
                    debug_assert!(core.lifo_enabled);
                    return Ok(core);
                }

                // Track that we are about to run a task from the LIFO slot.
                // 跟踪我们即将从 LIFO 槽运行的任务.
                lifo_polls += 1;
                super::counters::inc_lifo_schedules();

                // Disable the LIFO slot if we reach our limit
                //
                // In ping-ping style workloads where task A notifies task B,
                // which notifies task A again, continuously prioritizing the
                // LIFO slot can cause starvation as these two tasks will
                // repeatedly schedule the other. To mitigate this, we limit the
                // number of times the LIFO slot is prioritized.
                // 如果达到限制,则禁用 LIFO 槽
                if lifo_polls >= MAX_LIFO_POLLS_PER_TICK {
                    core.lifo_enabled = false;
                    super::counters::inc_lifo_capped();
                }

                // Run the LIFO task, then loop
                // 运行 LIFO 任务,然后循环
                *self.core.borrow_mut() = Some(core);
                let task = self.worker.handle.shared.owned.assert_owner(task);
                task.run();
            }
        })
    }

    fn reset_lifo_enabled(&self, core: &mut Core) {
        core.lifo_enabled = !self.worker.handle.shared.config.disable_lifo_slot;
    }

    fn assert_lifo_enabled_is_correct(&self, core: &Core) {
        debug_assert_eq!(
            core.lifo_enabled,
            !self.worker.handle.shared.config.disable_lifo_slot
        );
    }

    fn maintenance(&self, mut core: Box<Core>) -> Box<Core> {
        if core.tick % self.worker.handle.shared.config.event_interval == 0 {
            super::counters::inc_num_maintenance();

            core.stats.end_processing_scheduled_tasks();

            // Call `park` with a 0 timeout. This enables the I/O driver, timer, ...
            // to run without actually putting the thread to sleep.
            // 唤醒io, 定时器
            core = self.park_timeout(core, Some(Duration::from_millis(0)));

            // Run regularly scheduled maintenance
            core.maintenance(&self.worker);

            core.stats.start_processing_scheduled_tasks();
        }

        core
    }

    /// Parks the worker thread while waiting for tasks to execute.
    ///
    /// This function checks if indeed there's no more work left to be done before parking.
    /// Also important to notice that, before parking, the worker thread will try to take
    /// ownership of the Driver (IO/Time) and dispatch any events that might have fired.
    /// Whenever a worker thread executes the Driver loop, all waken tasks are scheduled
    /// in its own local queue until the queue saturates (ntasks > `LOCAL_QUEUE_CAPACITY`).
    /// When the local queue is saturated, the overflow tasks are added to the injection queue
    /// from where other workers can pick them up.
    /// Also, we rely on the workstealing algorithm to spread the tasks amongst workers
    /// after all the IOs get dispatched
    /// 在等待任务执行时停放工作线程.
    fn park(&self, mut core: Box<Core>) -> Box<Core> {
        if let Some(f) = &self.worker.handle.shared.config.before_park {
            f();
        }

        if core.transition_to_parked(&self.worker) {
            while !core.is_shutdown && !core.is_traced {
                core.stats.about_to_park();
                core.stats
                    .submit(&self.worker.handle.shared.worker_metrics[self.worker.index]);

                core = self.park_timeout(core, None);

                core.stats.unparked();

                // Run regularly scheduled maintenance
                core.maintenance(&self.worker);

                if core.transition_from_parked(&self.worker) {
                    break;
                }
            }
        }

        if let Some(f) = &self.worker.handle.shared.config.after_unpark {
            f();
        }
        core
    }

    fn park_timeout(&self, mut core: Box<Core>, duration: Option<Duration>) -> Box<Core> {
        self.assert_lifo_enabled_is_correct(&core);

        // Take the parker out of core
        let mut park = core.park.take().expect("park missing");

        // Store `core` in context
        *self.core.borrow_mut() = Some(core);

        // Park thread
        // 挂起线程
        if let Some(timeout) = duration {
            park.park_timeout(&self.worker.handle.driver, timeout);
        } else {
            park.park(&self.worker.handle.driver);
        }

        self.defer.wake();

        // Remove `core` from context
        core = self.core.borrow_mut().take().expect("core missing");

        // Place `park` back in `core`
        core.park = Some(park);

        if core.should_notify_others() {
            self.worker.handle.notify_parked_local();
        }

        core
    }

    pub(crate) fn defer(&self, waker: &Waker) {
        self.defer.defer(waker);
    }

    #[allow(dead_code)]
    pub(crate) fn get_worker_index(&self) -> usize {
        self.worker.index
    }
}

impl Core {
    /// Increment the tick
    /// 增加tick
    fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    /// Return the next notified task available to this worker.
    /// 返回该worker可用的下一个任务
    fn next_task(&mut self, worker: &Worker) -> Option<Notified> {
        if self.tick % self.global_queue_interval == 0 {
            // Update the global queue interval, if needed
            // 如果需要,更新全局队列间隔
            self.tune_global_queue_interval(worker);

            // 尝试获取远程任务
            worker
                .handle
                .next_remote_task()
                .or_else(|| self.next_local_task())
        } else {
            // 先尝试获取lifo_slot任务, 再获取run_queue的任务
            let maybe_task = self.next_local_task();

            if maybe_task.is_some() {
                return maybe_task;
            }

            if worker.inject().is_empty() {
                return None;
            }
            // 将inject中的部分任务, 转移到run_queue中

            // Other threads can only **remove** tasks from the current worker's
            // `run_queue`. So, we can be confident that by the time we call
            // `run_queue.push_back` below, there will be *at least* `cap`
            // available slots in the queue.
            let cap = usize::min(
                self.run_queue.remaining_slots(),
                self.run_queue.max_capacity() / 2,
            );

            // The worker is currently idle, pull a batch of work from the
            // injection queue. We don't want to pull *all* the work so other
            // workers can also get some.
            let n = usize::min(
                worker.inject().len() / worker.handle.shared.remotes.len() + 1,
                cap,
            );

            // Take at least one task since the first task is returned directly
            // and not pushed onto the local queue.
            let n = usize::max(1, n);

            let mut synced = worker.handle.shared.synced.lock();
            // safety: passing in the correct `inject::Synced`.
            let mut tasks = unsafe { worker.inject().pop_n(&mut synced.inject, n) };

            // Pop the first task to return immediately
            let ret = tasks.next();

            // Push the rest of the on the run queue
            self.run_queue.push_back(tasks);

            ret
        }
    }

    fn next_local_task(&mut self) -> Option<Notified> {
        self.lifo_slot.take().or_else(|| self.run_queue.pop())
    }

    /// Function responsible for stealing tasks from another worker
    ///
    /// Note: Only if less than half the workers are searching for tasks to steal
    /// a new worker will actually try to steal. The idea is to make sure not all
    /// workers will be trying to steal at the same time.
    /// 从另一个 worker 窃取任务
    fn steal_work(&mut self, worker: &Worker) -> Option<Notified> {
        // 判断是否获取远程任务
        if !self.transition_to_searching(worker) {
            return None;
        }

        let num = worker.handle.shared.remotes.len();
        // Start from a random worker
        let start = self.rand.fastrand_n(num as u32) as usize;

        for i in 0..num {
            let i = (start + i) % num;

            // Don't steal from ourself! We know we don't have work.
            // 跳过自己
            if i == worker.index {
                continue;
            }

            let target = &worker.handle.shared.remotes[i];
            // 获取其他worker的任务
            if let Some(task) = target
                .steal
                .steal_into(&mut self.run_queue, &mut self.stats)
            {
                return Some(task);
            }
        }

        // Fallback on checking the global queue
        // 回退到检查全局队列
        worker.handle.next_remote_task()
    }

    fn transition_to_searching(&mut self, worker: &Worker) -> bool {
        if !self.is_searching {
            self.is_searching = worker.handle.shared.idle.transition_worker_to_searching();
        }

        self.is_searching
    }

    fn transition_from_searching(&mut self, worker: &Worker) {
        if !self.is_searching {
            return;
        }

        self.is_searching = false;
        worker.handle.transition_worker_from_searching();
    }

    fn has_tasks(&self) -> bool {
        self.lifo_slot.is_some() || self.run_queue.has_tasks()
    }

    fn should_notify_others(&self) -> bool {
        // If there are tasks available to steal, but this worker is not
        // looking for tasks to steal, notify another worker.
        // 如果有可窃取的任务,但此工作人员没有寻找可窃取的任务,则通知另一个工作人员.
        if self.is_searching {
            return false;
        }
        self.lifo_slot.is_some() as usize + self.run_queue.len() > 1
    }

    /// Prepares the worker state for parking.
    ///
    /// Returns true if the transition happened, false if there is work to do first.
    fn transition_to_parked(&mut self, worker: &Worker) -> bool {
        // Workers should not park if they have work to do
        if self.has_tasks() || self.is_traced {
            return false;
        }

        // When the final worker transitions **out** of searching to parked, it
        // must check all the queues one last time in case work materialized
        // between the last work scan and transitioning out of searching.
        let is_last_searcher = worker.handle.shared.idle.transition_worker_to_parked(
            &worker.handle.shared,
            worker.index,
            self.is_searching,
        );

        // The worker is no longer searching. Setting this is the local cache
        // only.
        self.is_searching = false;

        if is_last_searcher {
            worker.handle.notify_if_work_pending();
        }

        true
    }

    /// Returns `true` if the transition happened.
    fn transition_from_parked(&mut self, worker: &Worker) -> bool {
        // If a task is in the lifo slot/run queue, then we must unpark regardless of
        // being notified
        if self.has_tasks() {
            // When a worker wakes, it should only transition to the "searching"
            // state when the wake originates from another worker *or* a new task
            // is pushed. We do *not* want the worker to transition to "searching"
            // when it wakes when the I/O driver receives new events.
            self.is_searching = !worker
                .handle
                .shared
                .idle
                .unpark_worker_by_id(&worker.handle.shared, worker.index);
            return true;
        }

        if worker
            .handle
            .shared
            .idle
            .is_parked(&worker.handle.shared, worker.index)
        {
            return false;
        }

        // When unparked, the worker is in the searching state.
        self.is_searching = true;
        true
    }

    /// Runs maintenance work such as checking the pool's state.
    fn maintenance(&mut self, worker: &Worker) {
        self.stats
            .submit(&worker.handle.shared.worker_metrics[worker.index]);

        if !self.is_shutdown {
            // Check if the scheduler has been shutdown
            let synced = worker.handle.shared.synced.lock();
            self.is_shutdown = worker.inject().is_closed(&synced.inject);
        }

        if !self.is_traced {
            // Check if the worker should be tracing.
            self.is_traced = worker.handle.shared.trace_status.trace_requested();
        }
    }

    /// Signals all tasks to shut down, and waits for them to complete. Must run
    /// before we enter the single-threaded phase of shutdown processing.
    /// 发出关闭所有任务的信号，并等待它们完成
    fn pre_shutdown(&mut self, worker: &Worker) {
        // Start from a random inner list
        let start = self
            .rand
            .fastrand_n(worker.handle.shared.owned.get_shard_size() as u32);
        // Signal to all tasks to shut down.
        worker
            .handle
            .shared
            .owned
            .close_and_shutdown_all(start as usize);

        self.stats
            .submit(&worker.handle.shared.worker_metrics[worker.index]);
    }

    /// Shuts down the core.
    fn shutdown(&mut self, handle: &Handle) {
        // Take the core
        let mut park = self.park.take().expect("park missing");

        // Drain the queue
        while self.next_local_task().is_some() {}

        park.shutdown(&handle.driver);
    }

    fn tune_global_queue_interval(&mut self, worker: &Worker) {
        let next = self
            .stats
            .tuned_global_queue_interval(&worker.handle.shared.config);

        // Smooth out jitter
        if u32::abs_diff(self.global_queue_interval, next) > 2 {
            self.global_queue_interval = next;
        }
    }
}

impl Worker {
    /// Returns a reference to the scheduler's injection queue.
    fn inject(&self) -> &inject::Shared<Arc<Handle>> {
        &self.handle.shared.inject
    }
}

// TODO: Move `Handle` impls into handle.rs
impl task::Schedule for Arc<Handle> {
    fn release(&self, task: &Task) -> Option<Task> {
        self.shared.owned.remove(task)
    }

    fn schedule(&self, task: Notified) {
        self.schedule_task(task, false);
    }

    fn hooks(&self) -> TaskHarnessScheduleHooks {
        TaskHarnessScheduleHooks {
            task_terminate_callback: self.task_hooks.task_terminate_callback.clone(),
        }
    }

    fn yield_now(&self, task: Notified) {
        self.schedule_task(task, true);
    }
}

impl Handle {
    // 调度任务
    pub(super) fn schedule_task(&self, task: Notified, is_yield: bool) {
        with_current(|maybe_cx| {
            if let Some(cx) = maybe_cx {
                // Make sure the task is part of the **current** scheduler.
                if self.ptr_eq(&cx.worker.handle) {
                    // And the current thread still holds a core
                    if let Some(core) = cx.core.borrow_mut().as_mut() {
                        self.schedule_local(core, task, is_yield);
                        return;
                    }
                }
            }

            // Otherwise, use the inject queue.
            self.push_remote_task(task);
            self.notify_parked_remote();
        });
    }

    pub(super) fn schedule_option_task_without_yield(&self, task: Option<Notified>) {
        if let Some(task) = task {
            self.schedule_task(task, false);
        }
    }

    // 如果`yield`,则必须始终将任务推到队列的后面,以便执行其他任务.
    // 如果不是`yield`,则灵活性更高,任务可能会排到队列的前面.
    fn schedule_local(&self, core: &mut Core, task: Notified, is_yield: bool) {
        core.stats.inc_local_schedule_count();

        // Spawning from the worker thread. If scheduling a "yield" then the
        // task must always be pushed to the back of the queue, enabling other
        // tasks to be executed. If **not** a yield, then there is more
        // flexibility and the task may go to the front of the queue.
        let should_notify = if is_yield || !core.lifo_enabled {
            core.run_queue
                .push_back_or_overflow(task, self, &mut core.stats);
            true
        } else {
            // Push to the LIFO slot
            let prev = core.lifo_slot.take();
            let ret = prev.is_some();

            if let Some(prev) = prev {
                core.run_queue
                    .push_back_or_overflow(prev, self, &mut core.stats);
            }

            core.lifo_slot = Some(task);

            ret
        };

        // Only notify if not currently parked. If `park` is `None`, then the
        // scheduling is from a resource driver. As notifications often come in
        // batches, the notification is delayed until the park is complete.
        if should_notify && core.park.is_some() {
            self.notify_parked_local();
        }
    }

    fn next_remote_task(&self) -> Option<Notified> {
        if self.shared.inject.is_empty() {
            return None;
        }

        let mut synced = self.shared.synced.lock();
        // safety: passing in correct `idle::Synced`
        unsafe { self.shared.inject.pop(&mut synced.inject) }
    }

    fn push_remote_task(&self, task: Notified) {
        self.shared.scheduler_metrics.inc_remote_schedule_count();

        let mut synced = self.shared.synced.lock();
        // safety: passing in correct `idle::Synced`
        unsafe {
            self.shared.inject.push(&mut synced.inject, task);
        }
    }

    pub(super) fn close(&self) {
        if self
            .shared
            .inject
            .close(&mut self.shared.synced.lock().inject)
        {
            self.notify_all();
        }
    }

    fn notify_parked_local(&self) {
        super::counters::inc_num_inc_notify_local();

        if let Some(index) = self.shared.idle.worker_to_notify(&self.shared) {
            super::counters::inc_num_unparks_local();
            self.shared.remotes[index].unpark.unpark(&self.driver);
        }
    }

    fn notify_parked_remote(&self) {
        if let Some(index) = self.shared.idle.worker_to_notify(&self.shared) {
            self.shared.remotes[index].unpark.unpark(&self.driver);
        }
    }

    pub(super) fn notify_all(&self) {
        for remote in &self.shared.remotes[..] {
            remote.unpark.unpark(&self.driver);
        }
    }

    fn notify_if_work_pending(&self) {
        for remote in &self.shared.remotes[..] {
            if !remote.steal.is_empty() {
                self.notify_parked_local();
                return;
            }
        }

        if !self.shared.inject.is_empty() {
            self.notify_parked_local();
        }
    }

    fn transition_worker_from_searching(&self) {
        if self.shared.idle.transition_worker_from_searching() {
            // We are the final searching worker. Because work was found, we
            // need to notify another worker.
            self.notify_parked_local();
        }
    }

    /// Signals that a worker has observed the shutdown signal and has replaced
    /// its core back into its handle.
    ///
    /// If all workers have reached this point, the final cleanup is performed.
    fn shutdown_core(&self, core: Box<Core>) {
        let mut cores = self.shared.shutdown_cores.lock();
        cores.push(core);

        if cores.len() != self.shared.remotes.len() {
            return;
        }

        debug_assert!(self.shared.owned.is_empty());

        for mut core in cores.drain(..) {
            core.shutdown(self);
        }

        // Drain the injection queue
        //
        // We already shut down every task, so we can simply drop the tasks.
        while let Some(task) = self.next_remote_task() {
            drop(task);
        }
    }

    fn ptr_eq(&self, other: &Handle) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Overflow<Arc<Handle>> for Handle {
    fn push(&self, task: task::Notified<Arc<Handle>>) {
        self.push_remote_task(task);
    }

    fn push_batch<I>(&self, iter: I)
    where
        I: Iterator<Item = task::Notified<Arc<Handle>>>,
    {
        unsafe {
            self.shared.inject.push_batch(self, iter);
        }
    }
}

pub(crate) struct InjectGuard<'a> {
    lock: crate::loom::sync::MutexGuard<'a, Synced>,
}

impl<'a> AsMut<inject::Synced> for InjectGuard<'a> {
    fn as_mut(&mut self) -> &mut inject::Synced {
        &mut self.lock.inject
    }
}

impl<'a> Lock<inject::Synced> for &'a Handle {
    type Handle = InjectGuard<'a>;

    fn lock(self) -> Self::Handle {
        InjectGuard {
            lock: self.shared.synced.lock(),
        }
    }
}

#[track_caller]
fn with_current<R>(f: impl FnOnce(Option<&Context>) -> R) -> R {
    use scheduler::Context::MultiThread;

    context::with_scheduler(|ctx| match ctx {
        Some(MultiThread(ctx)) => f(Some(ctx)),
        _ => f(None),
    })
}
