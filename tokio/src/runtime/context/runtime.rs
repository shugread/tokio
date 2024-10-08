use super::{BlockingRegionGuard, SetCurrentGuard, CONTEXT};

use crate::runtime::scheduler;
use crate::util::rand::{FastRand, RngSeed};

use std::fmt;

#[derive(Debug, Clone, Copy)]
#[must_use]
pub(crate) enum EnterRuntime {
    /// Currently in a runtime context.
    /// 当前在运行时的上下文中
    #[cfg_attr(not(feature = "rt"), allow(dead_code))]
    Entered { allow_block_in_place: bool },

    /// Not in a runtime context **or** a blocking region.
    /// 不在运行时上下文中或阻塞区域中
    NotEntered,
}

/// Guard tracking that a caller has entered a runtime context.
/// 保护跟踪调用者已进入运行时上下文
#[must_use]
pub(crate) struct EnterRuntimeGuard {
    /// Tracks that the current thread has entered a blocking function call.
    /// 跟踪当前线程已进入阻塞函数调用.
    pub(crate) blocking: BlockingRegionGuard,

    #[allow(dead_code)] // Only tracking the guard.
    // 跟踪守卫
    pub(crate) handle: SetCurrentGuard,

    // Tracks the previous random number generator seed
    // 跟踪前一个随机数生成器种子
    old_seed: RngSeed,
}

/// Marks the current thread as being within the dynamic extent of an
/// executor.
/// 保证当前线程只有一个任务阻塞
#[track_caller]
pub(crate) fn enter_runtime<F, R>(handle: &scheduler::Handle, allow_block_in_place: bool, f: F) -> R
where
    F: FnOnce(&mut BlockingRegionGuard) -> R,
{
    let maybe_guard = CONTEXT.with(|c| {
        if c.runtime.get().is_entered() {
            None
        } else {
            // Set the entered flag
            c.runtime.set(EnterRuntime::Entered {
                allow_block_in_place,
            });

            // Generate a new seed
            // 生成新的随机数种子
            let rng_seed = handle.seed_generator().next_seed();

            // Swap the RNG seed
            let mut rng = c.rng.get().unwrap_or_else(FastRand::new);
            let old_seed = rng.replace_seed(rng_seed);
            c.rng.set(Some(rng));

            Some(EnterRuntimeGuard {
                blocking: BlockingRegionGuard::new(),
                handle: c.set_current(handle),
                old_seed,
            })
        }
    });

    if let Some(mut guard) = maybe_guard {
        return f(&mut guard.blocking);
    }

    panic!(
        "Cannot start a runtime from within a runtime. This happens \
            because a function (like `block_on`) attempted to block the \
            current thread while the thread is being used to drive \
            asynchronous tasks."
    );
}

impl fmt::Debug for EnterRuntimeGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Enter").finish()
    }
}

impl Drop for EnterRuntimeGuard {
    fn drop(&mut self) {
        CONTEXT.with(|c| {
            assert!(c.runtime.get().is_entered());
            c.runtime.set(EnterRuntime::NotEntered);
            // Replace the previous RNG seed
            // 替换前一个随机数种子
            let mut rng = c.rng.get().unwrap_or_else(FastRand::new);
            rng.replace_seed(self.old_seed.clone());
            c.rng.set(Some(rng));
        });
    }
}

impl EnterRuntime {
    pub(crate) fn is_entered(self) -> bool {
        matches!(self, EnterRuntime::Entered { .. })
    }
}
