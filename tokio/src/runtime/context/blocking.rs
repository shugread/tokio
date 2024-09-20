use super::{EnterRuntime, CONTEXT};

use crate::loom::thread::AccessError;
use crate::util::markers::NotSendOrSync;

use std::marker::PhantomData;
use std::time::Duration;

/// Guard tracking that a caller has entered a blocking region.
#[must_use]
pub(crate) struct BlockingRegionGuard {
    _p: PhantomData<NotSendOrSync>,
}

pub(crate) struct DisallowBlockInPlaceGuard(bool);

// 尝试进入阻塞区域, 如果已经阻塞, 返回None
pub(crate) fn try_enter_blocking_region() -> Option<BlockingRegionGuard> {
    CONTEXT
        .try_with(|c| {
            if c.runtime.get().is_entered() {
                None
            } else {
                Some(BlockingRegionGuard::new())
            }
            // If accessing the thread-local fails, the thread is terminating
            // and thread-locals are being destroyed. Because we don't know if
            // we are currently in a runtime or not, we default to being
            // permissive.
        })
        // 如果访问线程局部变量失败,则线程将终止,线程局部变量将被销毁.
        // 由于我们不知道当前是否处于运行时,因此我们默认为宽容
        .unwrap_or_else(|_| Some(BlockingRegionGuard::new()))
}

/// Disallows blocking in the current runtime context until the guard is dropped.
/// 不允许在当前运行时上下文中阻塞,直到保护被解除.
pub(crate) fn disallow_block_in_place() -> DisallowBlockInPlaceGuard {
    let reset = CONTEXT.with(|c| {
        if let EnterRuntime::Entered {
            allow_block_in_place: true,
        } = c.runtime.get()
        {
            c.runtime.set(EnterRuntime::Entered {
                allow_block_in_place: false,
            });
            true
        } else {
            false
        }
    });

    DisallowBlockInPlaceGuard(reset)
}

impl BlockingRegionGuard {
    pub(super) fn new() -> BlockingRegionGuard {
        BlockingRegionGuard { _p: PhantomData }
    }

    /// Blocks the thread on the specified future, returning the value with
    /// which that future completes.
    /// 在指定的Future上阻止线程,
    /// 并返回该Future完成的值.
    pub(crate) fn block_on<F>(&mut self, f: F) -> Result<F::Output, AccessError>
    where
        F: std::future::Future,
    {
        use crate::runtime::park::CachedParkThread;

        let mut park = CachedParkThread::new();
        // 阻塞运行
        park.block_on(f)
    }

    /// Blocks the thread on the specified future for **at most** `timeout`
    ///
    /// If the future completes before `timeout`, the result is returned. If
    /// `timeout` elapses, then `Err` is returned.
    /// 带超时的阻塞
    pub(crate) fn block_on_timeout<F>(&mut self, f: F, timeout: Duration) -> Result<F::Output, ()>
    where
        F: std::future::Future,
    {
        use crate::runtime::park::CachedParkThread;
        use std::task::Context;
        use std::task::Poll::Ready;
        use std::time::Instant;

        let mut park = CachedParkThread::new();
        let waker = park.waker().map_err(|_| ())?;
        let mut cx = Context::from_waker(&waker);

        pin!(f);
        let when = Instant::now() + timeout;

        loop {
            if let Ready(v) = crate::runtime::coop::budget(|| f.as_mut().poll(&mut cx)) {
                return Ok(v);
            }

            let now = Instant::now();

            if now >= when {
                return Err(());
            }

            park.park_timeout(when - now);
        }
    }
}

impl Drop for DisallowBlockInPlaceGuard {
    fn drop(&mut self) {
        if self.0 {
            // XXX: Do we want some kind of assertion here, or is "best effort" okay?
            CONTEXT.with(|c| {
                if let EnterRuntime::Entered {
                    allow_block_in_place: false,
                } = c.runtime.get()
                {
                    c.runtime.set(EnterRuntime::Entered {
                        allow_block_in_place: true,
                    });
                }
            });
        }
    }
}
