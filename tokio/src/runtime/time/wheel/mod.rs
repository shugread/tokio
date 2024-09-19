use crate::runtime::time::{TimerHandle, TimerShared};
use crate::time::error::InsertError;

mod level;
pub(crate) use self::level::Expiration;
use self::level::Level;

use std::{array, ptr::NonNull};

use super::EntryList;

/// Timing wheel implementation.
///
/// This type provides the hashed timing wheel implementation that backs `Timer`
/// and `DelayQueue`.
///
/// The structure is generic over `T: Stack`. This allows handling timeout data
/// being stored on the heap or in a slab. In order to support the latter case,
/// the slab must be passed into each function allowing the implementation to
/// lookup timer entries.
///
/// See `Timer` documentation for some implementation notes.
/// 时间轮盘, 有64个槽
#[derive(Debug)]
pub(crate) struct Wheel {
    /// The number of milliseconds elapsed since the wheel started.
    /// 记录从时间轮启动以来经过的毫秒数.
    elapsed: u64,

    /// Timer wheel.
    ///
    /// Levels:
    ///
    /// * 1 ms slots / 64 ms range
    /// * 64 ms slots / ~ 4 sec range
    /// * ~ 4 sec slots / ~ 4 min range
    /// * ~ 4 min slots / ~ 4 hr range
    /// * ~ 4 hr slots / ~ 12 day range
    /// * ~ 12 day slots / ~ 2 yr range
    ///
    /// 由多个 Level 组成的数组,每个 Level 代表一个不同时间精度的层级.
    /// 包含 6 层,每层有 64 个槽,覆盖了从毫秒级到年级别的时间范围.
    levels: Box<[Level; NUM_LEVELS]>,

    /// Entries queued for firing
    /// 存储等待触发的任务
    pending: EntryList,
}

/// Number of levels. Each level has 64 slots. By using 6 levels with 64 slots
/// each, the timer is able to track time up to 2 years into the future with a
/// precision of 1 millisecond.
const NUM_LEVELS: usize = 6;

/// The maximum duration of a `Sleep`.
/// 可管理的最大时间跨度
pub(super) const MAX_DURATION: u64 = (1 << (6 * NUM_LEVELS)) - 1;

impl Wheel {
    /// Creates a new timing wheel.
    /// 创建一个新的时间轮盘
    pub(crate) fn new() -> Wheel {
        Wheel {
            elapsed: 0,
            levels: Box::new(array::from_fn(Level::new)),
            pending: EntryList::new(),
        }
    }

    /// Returns the number of milliseconds that have elapsed since the timing
    /// wheel's creation.
    pub(crate) fn elapsed(&self) -> u64 {
        self.elapsed
    }

    /// Inserts an entry into the timing wheel.
    ///
    /// # Arguments
    ///
    /// * `item`: The item to insert into the wheel.
    ///
    /// # Return
    ///
    /// Returns `Ok` when the item is successfully inserted, `Err` otherwise.
    ///
    /// `Err(Elapsed)` indicates that `when` represents an instant that has
    /// already passed. In this case, the caller should fire the timeout
    /// immediately.
    ///
    /// `Err(Invalid)` indicates an invalid `when` argument as been supplied.
    ///
    /// # Safety
    ///
    /// This function registers item into an intrusive linked list. The caller
    /// must ensure that `item` is pinned and will not be dropped without first
    /// being deregistered.
    /// 将一个任务插入到时间轮中
    pub(crate) unsafe fn insert(
        &mut self,
        item: TimerHandle,
    ) -> Result<u64, (TimerHandle, InsertError)> {
        // 任务的到期时间
        let when = item.sync_when();

        // 任务已经超时
        if when <= self.elapsed {
            return Err((item, InsertError::Elapsed));
        }

        // Get the level at which the entry should be stored
        // 到期时间计算任务应该存放的层级
        let level = self.level_for(when);

        unsafe {
            self.levels[level].add_entry(item);
        }

        debug_assert!({
            self.levels[level]
                .next_expiration(self.elapsed)
                .map(|e| e.deadline >= self.elapsed)
                .unwrap_or(true)
        });

        Ok(when)
    }

    /// Removes `item` from the timing wheel.
    /// 从时间轮盘中移除任务
    pub(crate) unsafe fn remove(&mut self, item: NonNull<TimerShared>) {
        unsafe {
            let when = item.as_ref().cached_when();
            if when == u64::MAX {
                self.pending.remove(item);
            } else {
                debug_assert!(
                    self.elapsed <= when,
                    "elapsed={}; when={}",
                    self.elapsed,
                    when
                );

                let level = self.level_for(when);
                self.levels[level].remove_entry(item);
            }
        }
    }

    /// Instant at which to poll.
    pub(crate) fn poll_at(&self) -> Option<u64> {
        self.next_expiration().map(|expiration| expiration.deadline)
    }

    /// Advances the timer up to the instant represented by `now`.
    /// 获取超时的任务
    pub(crate) fn poll(&mut self, now: u64) -> Option<TimerHandle> {
        loop {
            if let Some(handle) = self.pending.pop_back() {
                return Some(handle);
            }

            match self.next_expiration() {
                Some(ref expiration) if expiration.deadline <= now => {
                    // 有任务超时
                    self.process_expiration(expiration);

                    self.set_elapsed(expiration.deadline);
                }
                _ => {
                    // in this case the poll did not indicate an expiration
                    // _and_ we were not able to find a next expiration in
                    // the current list of timers.  advance to the poll's
                    // current time and do nothing else.
                    // 没有超时任务
                    self.set_elapsed(now);
                    break;
                }
            }
        }

        self.pending.pop_back()
    }

    /// Returns the instant at which the next timeout expires.
    /// 返回时间轮中下一个超时的时间
    fn next_expiration(&self) -> Option<Expiration> {
        if !self.pending.is_empty() {
            // Expire immediately as we have things pending firing
            return Some(Expiration {
                level: 0,
                slot: 0,
                deadline: self.elapsed,
            });
        }

        // Check all levels
        // 检测所有分片
        for (level_num, level) in self.levels.iter().enumerate() {
            if let Some(expiration) = level.next_expiration(self.elapsed) {
                // There cannot be any expirations at a higher level that happen
                // before this one.
                debug_assert!(self.no_expirations_before(level_num + 1, expiration.deadline));

                return Some(expiration);
            }
        }

        None
    }

    /// Returns the tick at which this timer wheel next needs to perform some
    /// processing, or None if there are no timers registered.
    /// 返回此计时器轮下次需要执行某些处理的刻度.如果没有注册计时器,则返回 None.
    pub(super) fn next_expiration_time(&self) -> Option<u64> {
        self.next_expiration().map(|ex| ex.deadline)
    }

    /// Used for debug assertions
    fn no_expirations_before(&self, start_level: usize, before: u64) -> bool {
        let mut res = true;

        for level in &self.levels[start_level..] {
            if let Some(e2) = level.next_expiration(self.elapsed) {
                if e2.deadline < before {
                    res = false;
                }
            }
        }

        res
    }

    /// iteratively find entries that are between the wheel's current
    /// time and the expiration time.  for each in that population either
    /// queue it for notification (in the case of the last level) or tier
    /// it down to the next level (in all other cases).
    /// 处理已超时的任务
    pub(crate) fn process_expiration(&mut self, expiration: &Expiration) {
        // Note that we need to take _all_ of the entries off the list before
        // processing any of them. This is important because it's possible that
        // those entries might need to be reinserted into the same slot.
        //
        // This happens only on the highest level, when an entry is inserted
        // more than MAX_DURATION into the future. When this happens, we wrap
        // around, and process some entries a multiple of MAX_DURATION before
        // they actually need to be dropped down a level. We then reinsert them
        // back into the same position; we must make sure we don't then process
        // those entries again or we'll end up in an infinite loop.
        let mut entries = self.take_entries(expiration);

        // 依次处理
        while let Some(item) = entries.pop_back() {
            if expiration.level == 0 {
                debug_assert_eq!(unsafe { item.cached_when() }, expiration.deadline);
            }

            // Try to expire the entry; this is cheap (doesn't synchronize) if
            // the timer is not expired, and updates cached_when.
            match unsafe { item.mark_pending(expiration.deadline) } {
                Ok(()) => {
                    // Item was expired
                    // 任务超时, 将任务添加到pending队列
                    self.pending.push_front(item);
                }
                Err(expiration_tick) => {
                    // 任务没有过期, 重新添加到时间轮
                    let level = level_for(expiration.deadline, expiration_tick);
                    unsafe {
                        self.levels[level].add_entry(item);
                    }
                }
            }
        }
    }

    fn set_elapsed(&mut self, when: u64) {
        assert!(
            self.elapsed <= when,
            "elapsed={:?}; when={:?}",
            self.elapsed,
            when
        );

        if when > self.elapsed {
            self.elapsed = when;
        }
    }

    /// Obtains the list of entries that need processing for the given expiration.
    /// 获取在给定到期时间内需要处理的任务列表.
    fn take_entries(&mut self, expiration: &Expiration) -> EntryList {
        self.levels[expiration.level].take_slot(expiration.slot)
    }

    fn level_for(&self, when: u64) -> usize {
        level_for(self.elapsed, when)
    }
}

/// 它根据任务的到期时间计算该任务应该存放在时间轮的哪个层级
fn level_for(elapsed: u64, when: u64) -> usize {
    const SLOT_MASK: u64 = (1 << 6) - 1;

    // Mask in the trailing bits ignored by the level calculation in order to cap
    // the possible leading zeros
    // 避免在时间差很小时计算出的前导零数量过多的问题
    let mut masked = elapsed ^ when | SLOT_MASK;

    if masked >= MAX_DURATION {
        // Fudge the timer into the top level
        masked = MAX_DURATION - 1;
    }

    let leading_zeros = masked.leading_zeros() as usize;
    let significant = 63 - leading_zeros;

    significant / NUM_LEVELS
}

#[cfg(all(test, not(loom)))]
mod test {
    use super::*;

    #[test]
    fn test_level_for() {
        for pos in 0..64 {
            assert_eq!(
                0,
                level_for(0, pos),
                "level_for({}) -- binary = {:b}",
                pos,
                pos
            );
        }

        for level in 1..5 {
            for pos in level..64 {
                let a = pos * 64_usize.pow(level as u32);
                assert_eq!(
                    level,
                    level_for(0, a as u64),
                    "level_for({}) -- binary = {:b}",
                    a,
                    a
                );

                if pos > level {
                    let a = a - 1;
                    assert_eq!(
                        level,
                        level_for(0, a as u64),
                        "level_for({}) -- binary = {:b}",
                        a,
                        a
                    );
                }

                if pos < 64 {
                    let a = a + 1;
                    assert_eq!(
                        level,
                        level_for(0, a as u64),
                        "level_for({}) -- binary = {:b}",
                        a,
                        a
                    );
                }
            }
        }
    }
}
