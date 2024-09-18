use crate::future::Future;
use crate::runtime::task::core::{Cell, Core, Header, Trailer};
use crate::runtime::task::state::{Snapshot, State};
use crate::runtime::task::waker::waker_ref;
use crate::runtime::task::{Id, JoinError, Notified, RawTask, Schedule, Task};

use crate::runtime::TaskMeta;
use std::any::Any;
use std::mem;
use std::mem::ManuallyDrop;
use std::panic;
use std::ptr::NonNull;
use std::task::{Context, Poll, Waker};

/// Typed raw task handle.
/// 原始任务句柄.
pub(super) struct Harness<T: Future, S: 'static> {
    cell: NonNull<Cell<T, S>>,
}

impl<T, S> Harness<T, S>
where
    T: Future,
    S: 'static,
{
    pub(super) unsafe fn from_raw(ptr: NonNull<Header>) -> Harness<T, S> {
        Harness {
            cell: ptr.cast::<Cell<T, S>>(),
        }
    }

    fn header_ptr(&self) -> NonNull<Header> {
        self.cell.cast()
    }

    fn header(&self) -> &Header {
        unsafe { &*self.header_ptr().as_ptr() }
    }

    fn state(&self) -> &State {
        &self.header().state
    }

    fn trailer(&self) -> &Trailer {
        unsafe { &self.cell.as_ref().trailer }
    }

    fn core(&self) -> &Core<T, S> {
        unsafe { &self.cell.as_ref().core }
    }
}

/// Task operations that can be implemented without being generic over the
/// scheduler or task. Only one version of these methods should exist in the
/// final binary.
/// 无需通过调度程序或任务通用即可实现的任务操作.
/// 最终二进制文件中应仅存在这些方法的一个版本.
impl RawTask {
    pub(super) fn drop_reference(self) {
        if self.state().ref_dec() {
            self.dealloc();
        }
    }

    /// This call consumes a ref-count and notifies the task. This will create a
    /// new Notified and submit it if necessary.
    ///
    /// The caller does not need to hold a ref-count besides the one that was
    /// passed to this call.
    /// 此调用使用引用计数并通知任务.这将创建一个新的通知并在必要时提交.
    /// 除了传递给此调用的引用计数之外,调用者不需要保存引用计数.
    pub(super) fn wake_by_val(&self) {
        use super::state::TransitionToNotifiedByVal;

        match self.state().transition_to_notified_by_val() {
            TransitionToNotifiedByVal::Submit => {
                // The caller has given us a ref-count, and the transition has
                // created a new ref-count, so we now hold two. We turn the new
                // ref-count Notified and pass it to the call to `schedule`.
                //
                // The old ref-count is retained for now to ensure that the task
                // is not dropped during the call to `schedule` if the call
                // drops the task it was given.
                // 调用者给了我们一个引用计数,转换创建了一个新的引用计数,所以我们现在持有两个.
                // 我们将新的引用计数变为 Notified,并将其传递给 `schedule` 调用.
                // 旧的引用计数暂时保留,以确保在 `schedule` 调用期间,如果调用放弃给定的任务,则不会放弃该任务.
                self.schedule();

                // Now that we have completed the call to schedule, we can
                // release our ref-count.
                // 现在我们已经完成了对 schedule 的调用,我们可以释放我们的引用计数.
                self.drop_reference();
            }
            TransitionToNotifiedByVal::Dealloc => {
                self.dealloc();
            }
            TransitionToNotifiedByVal::DoNothing => {}
        }
    }

    /// This call notifies the task. It will not consume any ref-counts, but the
    /// caller should hold a ref-count.  This will create a new Notified and
    /// submit it if necessary.
    /// 此调用通知任务.它不会消耗任何引用计数,
    /// 但调用者应保留一个引用计数.这将创建一个新的通知并必要时提交它.
    pub(super) fn wake_by_ref(&self) {
        use super::state::TransitionToNotifiedByRef;

        match self.state().transition_to_notified_by_ref() {
            TransitionToNotifiedByRef::Submit => {
                // The transition above incremented the ref-count for a new task
                // and the caller also holds a ref-count. The caller's ref-count
                // ensures that the task is not destroyed even if the new task
                // is dropped before `schedule` returns.
                // 上述转换增加了新任务的引用计数,调用者也持有一个引用计数.
                // 调用者的引用计数确保即使在 `schedule` 返回之前删除新任务,也不会销毁该任务.
                self.schedule();
            }
            TransitionToNotifiedByRef::DoNothing => {}
        }
    }

    /// Remotely aborts the task.
    ///
    /// The caller should hold a ref-count, but we do not consume it.
    ///
    /// This is similar to `shutdown` except that it asks the runtime to perform
    /// the shutdown. This is necessary to avoid the shutdown happening in the
    /// wrong thread for non-Send tasks.
    /// 远程中止任务.
    pub(super) fn remote_abort(&self) {
        if self.state().transition_to_notified_and_cancel() {
            // The transition has created a new ref-count, which we turn into
            // a Notified and pass to the task.
            //
            // Since the caller holds a ref-count, the task cannot be destroyed
            // before the call to `schedule` returns even if the call drops the
            // `Notified` internally.
            self.schedule();
        }
    }

    /// Try to set the waker notified when the task is complete. Returns true if
    /// the task has already completed. If this call returns false, then the
    /// waker will not be notified.
    pub(super) fn try_set_join_waker(&self, waker: &Waker) -> bool {
        can_read_output(self.header(), self.trailer(), waker)
    }
}

impl<T, S> Harness<T, S>
where
    T: Future,
    S: Schedule,
{
    pub(super) fn drop_reference(self) {
        if self.state().ref_dec() {
            self.dealloc();
        }
    }

    /// Polls the inner future. A ref-count is consumed.
    ///
    /// All necessary state checks and transitions are performed.
    /// Panics raised while polling the future are handled.
    /// 轮询内部Future.消耗引用计数.
    pub(super) fn poll(self) {
        // We pass our ref-count to `poll_inner`.
        // 将引用计数传递给`poll_inner`.
        match self.poll_inner() {
            PollFuture::Notified => {
                // The `poll_inner` call has given us two ref-counts back.
                // We give one of them to a new task and call `yield_now`.
                // `poll_inner` 调用返回了两个引用计数.我们将其中一个提供给新任务并调用 `yield_now`.
                self.core()
                    .scheduler
                    .yield_now(Notified(self.get_new_task()));

                // The remaining ref-count is now dropped. We kept the extra
                // ref-count until now to ensure that even if the `yield_now`
                // call drops the provided task, the task isn't deallocated
                // before after `yield_now` returns.
                self.drop_reference();
            }
            PollFuture::Complete => {
                self.complete();
            }
            PollFuture::Dealloc => {
                self.dealloc();
            }
            PollFuture::Done => (),
        }
    }

    /// Polls the task and cancel it if necessary. This takes ownership of a
    /// ref-count.
    ///
    /// If the return value is Notified, the caller is given ownership of two
    /// ref-counts.
    ///
    /// If the return value is Complete, the caller is given ownership of a
    /// single ref-count, which should be passed on to `complete`.
    ///
    /// If the return value is `Dealloc`, then this call consumed the last
    /// ref-count and the caller should call `dealloc`.
    ///
    /// Otherwise the ref-count is consumed and the caller should not access
    /// `self` again.
    /// 轮询任务并在必要时取消它.这将获得引用计数的所有权.
    /// 如果返回值为 Notified,则调用者将获得两个引用计数的所有权.
    /// 如果返回值为 Complete,则调用者将获得单个引用计数的所有权,该引用计数应传递给 `complete`.
    /// 如果返回值为 `Dealloc`,则此调用消耗了最后一个引用计数,调用者应调用 `dealloc`.
    /// 否则,引用计数将被消耗,调用者不应再次访问 `self`.
    fn poll_inner(&self) -> PollFuture {
        use super::state::{TransitionToIdle, TransitionToRunning};

        match self.state().transition_to_running() {
            TransitionToRunning::Success => {
                // Separated to reduce LLVM codegen
                fn transition_result_to_poll_future(result: TransitionToIdle) -> PollFuture {
                    match result {
                        TransitionToIdle::Ok => PollFuture::Done,
                        TransitionToIdle::OkNotified => PollFuture::Notified,
                        TransitionToIdle::OkDealloc => PollFuture::Dealloc,
                        TransitionToIdle::Cancelled => PollFuture::Complete,
                    }
                }
                let header_ptr = self.header_ptr();
                let waker_ref = waker_ref::<S>(&header_ptr);
                let cx = Context::from_waker(&waker_ref);
                // 轮询Future
                let res = poll_future(self.core(), cx);

                if res == Poll::Ready(()) {
                    // The future completed. Move on to complete the task.
                    // Future已完成.
                    return PollFuture::Complete;
                }

                let transition_res = self.state().transition_to_idle();
                if let TransitionToIdle::Cancelled = transition_res {
                    // The transition to idle failed because the task was
                    // cancelled during the poll.
                    // 由于轮询期间任务被取消,因此转换为空闲状态失败.
                    cancel_task(self.core());
                }
                transition_result_to_poll_future(transition_res)
            }
            TransitionToRunning::Cancelled => {
                cancel_task(self.core());
                PollFuture::Complete
            }
            TransitionToRunning::Failed => PollFuture::Done,
            TransitionToRunning::Dealloc => PollFuture::Dealloc,
        }
    }

    /// Forcibly shuts down the task.
    ///
    /// Attempt to transition to `Running` in order to forcibly shutdown the
    /// task. If the task is currently running or in a state of completion, then
    /// there is nothing further to do. When the task completes running, it will
    /// notice the `CANCELLED` bit and finalize the task.
    /// 强制关闭任务.
    pub(super) fn shutdown(self) {
        if !self.state().transition_to_shutdown() {
            // The task is concurrently running. No further work needed.
            self.drop_reference();
            return;
        }

        // By transitioning the lifecycle to `Running`, we have permission to
        // drop the future.
        cancel_task(self.core());
        self.complete();
    }

    pub(super) fn dealloc(self) {
        // Observe that we expect to have mutable access to these objects
        // because we are going to drop them. This only matters when running
        // under loom.
        self.trailer().waker.with_mut(|_| ());
        self.core().stage.with_mut(|_| ());

        // Safety: The caller of this method just transitioned our ref-count to
        // zero, so it is our responsibility to release the allocation.
        //
        // We don't hold any references into the allocation at this point, but
        // it is possible for another thread to still hold a `&State` into the
        // allocation if that other thread has decremented its last ref-count,
        // but has not yet returned from the relevant method on `State`.
        //
        // However, the `State` type consists of just an `AtomicUsize`, and an
        // `AtomicUsize` wraps the entirety of its contents in an `UnsafeCell`.
        // As explained in the documentation for `UnsafeCell`, such references
        // are allowed to be dangling after their last use, even if the
        // reference has not yet gone out of scope.
        unsafe {
            drop(Box::from_raw(self.cell.as_ptr()));
        }
    }

    // ===== join handle =====

    /// Read the task output into `dst`.
    /// 读取任务输出到`dst`
    pub(super) fn try_read_output(self, dst: &mut Poll<super::Result<T::Output>>, waker: &Waker) {
        if can_read_output(self.header(), self.trailer(), waker) {
            *dst = Poll::Ready(self.core().take_output());
        }
    }

    pub(super) fn drop_join_handle_slow(self) {
        // Try to unset `JOIN_INTEREST`. This must be done as a first step in
        // case the task concurrently completed.
        // 尝试取消设置 `JOIN_INTEREST`.如果任务同时完成,则必须首先执行此操作.
        if self.state().unset_join_interested().is_err() {
            // It is our responsibility to drop the output. This is critical as
            // the task output may not be `Send` and as such must remain with
            // the scheduler or `JoinHandle`. i.e. if the output remains in the
            // task structure until the task is deallocated, it may be dropped
            // by a Waker on any arbitrary thread.
            //
            // Panics are delivered to the user via the `JoinHandle`. Given that
            // they are dropping the `JoinHandle`, we assume they are not
            // interested in the panic and swallow it.
            let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                self.core().drop_future_or_output();
            }));
        }

        // Drop the `JoinHandle` reference, possibly deallocating the task
        self.drop_reference();
    }

    // ====== internal ======

    /// Completes the task. This method assumes that the state is RUNNING.
    /// 完成任务.此方法假定状态为 RUNNING.
    fn complete(self) {
        // The future has completed and its output has been written to the task
        // stage. We transition from running to complete.

        let snapshot = self.state().transition_to_complete();

        // We catch panics here in case dropping the future or waking the
        // JoinHandle panics.
        let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            if !snapshot.is_join_interested() {
                // The `JoinHandle` is not interested in the output of
                // this task. It is our responsibility to drop the
                // output.
                // 丢弃任务输出
                self.core().drop_future_or_output();
            } else if snapshot.is_join_waker_set() {
                // Notify the waker. Reading the waker field is safe per rule 4
                // in task/mod.rs, since the JOIN_WAKER bit is set and the call
                // to transition_to_complete() above set the COMPLETE bit.
                // JoinHandle获取返回值
                self.trailer().wake_join();
            }
        }));

        // We catch panics here in case invoking a hook panics.
        //
        // We call this in a separate block so that it runs after the task appears to have
        // completed and will still run if the destructor panics.
        if let Some(f) = self.trailer().hooks.task_terminate_callback.as_ref() {
            let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                f(&TaskMeta {
                    #[cfg(tokio_unstable)]
                    id: self.core().task_id,
                    _phantom: Default::default(),
                })
            }));
        }

        // The task has completed execution and will no longer be scheduled.
        // 任务完成, 不再调度
        let num_release = self.release();

        if self.state().transition_to_terminal(num_release) {
            self.dealloc();
        }
    }

    /// Releases the task from the scheduler. Returns the number of ref-counts
    /// that should be decremented.
    /// 从调度程序中释放任务.返回应减少的引用计数的数.
    fn release(&self) -> usize {
        // We don't actually increment the ref-count here, but the new task is
        // never destroyed, so that's ok.
        let me = ManuallyDrop::new(self.get_new_task());

        if let Some(task) = self.core().scheduler.release(&me) {
            mem::forget(task);
            2
        } else {
            1
        }
    }

    /// Creates a new task that holds its own ref-count.
    ///
    /// # Safety
    ///
    /// Any use of `self` after this call must ensure that a ref-count to the
    /// task holds the task alive until after the use of `self`. Passing the
    /// returned Task to any method on `self` is unsound if dropping the Task
    /// could drop `self` before the call on `self` returned.
    /// 创建一个拥有自己的引用计数的新任务.
    fn get_new_task(&self) -> Task<S> {
        // safety: The header is at the beginning of the cell, so this cast is
        // safe.
        unsafe { Task::from_raw(self.cell.cast()) }
    }
}

fn can_read_output(header: &Header, trailer: &Trailer, waker: &Waker) -> bool {
    // Load a snapshot of the current task state
    let snapshot = header.state.load();

    debug_assert!(snapshot.is_join_interested());

    if !snapshot.is_complete() {
        // If the task is not complete, try storing the provided waker in the
        // task's waker field.
        // 如果任务未完成,尝试将提供的唤醒器存储在任务的waker字段中.

        let res = if snapshot.is_join_waker_set() {
            // If JOIN_WAKER is set, then JoinHandle has previously stored a
            // waker in the waker field per step (iii) of rule 5 in task/mod.rs.

            // Optimization: if the stored waker and the provided waker wake the
            // same task, then return without touching the waker field. (Reading
            // the waker field below is safe per rule 3 in task/mod.rs.)
            // 如果存储的waker和提供的waker唤醒同一个任务,则返回而不触碰 waker 字段.
            if unsafe { trailer.will_wake(waker) } {
                return false;
            }

            // Otherwise swap the stored waker with the provided waker by
            // following the rule 5 in task/mod.rs.
            // 设置提供的waker
            header
                .state
                .unset_waker()
                .and_then(|snapshot| set_join_waker(header, trailer, waker.clone(), snapshot))
        } else {
            // If JOIN_WAKER is unset, then JoinHandle has mutable access to the
            // waker field per rule 2 in task/mod.rs; therefore, skip step (i)
            // of rule 5 and try to store the provided waker in the waker field.
            // 如果JOIN_WAKER没有设置, 设置waker
            set_join_waker(header, trailer, waker.clone(), snapshot)
        };

        match res {
            Ok(_) => return false,
            Err(snapshot) => {
                assert!(snapshot.is_complete());
            }
        }
    }
    true
}

fn set_join_waker(
    header: &Header,
    trailer: &Trailer,
    waker: Waker,
    snapshot: Snapshot,
) -> Result<Snapshot, Snapshot> {
    assert!(snapshot.is_join_interested());
    assert!(!snapshot.is_join_waker_set());

    // Safety: Only the `JoinHandle` may set the `waker` field. When
    // `JOIN_INTEREST` is **not** set, nothing else will touch the field.
    unsafe {
        trailer.set_waker(Some(waker));
    }

    // Update the `JoinWaker` state accordingly
    let res = header.state.set_join_waker();

    // If the state could not be updated, then clear the join waker
    // 如果状态无法更新,则清除连接waker
    if res.is_err() {
        unsafe {
            trailer.set_waker(None);
        }
    }

    res
}

enum PollFuture {
    Complete,
    Notified,
    Done,
    Dealloc,
}

/// Cancels the task and store the appropriate error in the stage field.
/// 取消任务并将适当的错误存储在阶段字段中.
fn cancel_task<T: Future, S: Schedule>(core: &Core<T, S>) {
    // Drop the future from a panic guard.
    let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        core.drop_future_or_output();
    }));

    core.store_output(Err(panic_result_to_join_error(core.task_id, res)));
}

fn panic_result_to_join_error(
    task_id: Id,
    res: Result<(), Box<dyn Any + Send + 'static>>,
) -> JoinError {
    match res {
        Ok(()) => JoinError::cancelled(task_id),
        Err(panic) => JoinError::panic(task_id, panic),
    }
}

/// Polls the future. If the future completes, the output is written to the
/// stage field.
fn poll_future<T: Future, S: Schedule>(core: &Core<T, S>, cx: Context<'_>) -> Poll<()> {
    // Poll the future.
    // 获取future返回值
    let output = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        struct Guard<'a, T: Future, S: Schedule> {
            core: &'a Core<T, S>,
        }
        impl<'a, T: Future, S: Schedule> Drop for Guard<'a, T, S> {
            fn drop(&mut self) {
                // If the future panics on poll, we drop it inside the panic
                // guard.
                self.core.drop_future_or_output();
            }
        }
        let guard = Guard { core };
        let res = guard.core.poll(cx);
        mem::forget(guard);
        res
    }));

    // Prepare output for being placed in the core stage.
    let output = match output {
        Ok(Poll::Pending) => return Poll::Pending,
        Ok(Poll::Ready(output)) => Ok(output),
        Err(panic) => Err(panic_to_error(&core.scheduler, core.task_id, panic)),
    };

    // Catch and ignore panics if the future panics on drop.
    // 保存future返回值
    let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        core.store_output(output);
    }));

    if res.is_err() {
        core.scheduler.unhandled_panic();
    }

    Poll::Ready(())
}

#[cold]
fn panic_to_error<S: Schedule>(
    scheduler: &S,
    task_id: Id,
    panic: Box<dyn Any + Send + 'static>,
) -> JoinError {
    scheduler.unhandled_panic();
    JoinError::panic(task_id, panic)
}
