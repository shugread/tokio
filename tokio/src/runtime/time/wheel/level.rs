use crate::runtime::time::{EntryList, TimerHandle, TimerShared};

use std::{array, fmt, ptr::NonNull};

/// Wheel for a single level in the timer. This wheel contains 64 slots.
/// 时间分层
pub(crate) struct Level {
    // 分层索引
    level: usize,

    /// Bit field tracking which slots currently contain entries.
    ///
    /// Using a bit field to track slots that contain entries allows avoiding a
    /// scan to find entries. This field is updated when entries are added or
    /// removed from a slot.
    ///
    /// The least-significant bit represents slot zero.
    /// 插槽掩码
    occupied: u64,

    /// Slots. We access these via the EntryInner `current_list` as well, so this needs to be an `UnsafeCell`.
    /// 任务列表
    slot: [EntryList; LEVEL_MULT],
}

/// Indicates when a slot must be processed next.
/// 表示何时必须下一步处理插槽.
#[derive(Debug)]
pub(crate) struct Expiration {
    /// The level containing the slot.
    /// 分层
    pub(crate) level: usize,

    /// The slot index.
    /// 插槽索引
    pub(crate) slot: usize,

    /// The instant at which the slot needs to be processed.
    /// 处理插槽的时间
    pub(crate) deadline: u64,
}

/// Level multiplier.
///
/// Being a power of 2 is very important.
const LEVEL_MULT: usize = 64;

impl Level {
    pub(crate) fn new(level: usize) -> Level {
        Level {
            level,
            occupied: 0,
            slot: array::from_fn(|_| EntryList::default()),
        }
    }

    /// Finds the slot that needs to be processed next and returns the slot and
    /// `Instant` at which this slot must be processed.
    /// 获取下一次超时
    pub(crate) fn next_expiration(&self, now: u64) -> Option<Expiration> {
        // Use the `occupied` bit field to get the index of the next slot that
        // needs to be processed.
        // 使用`occupied`位字段来获取下一个需要处理的槽的索引.
        let slot = self.next_occupied_slot(now)?;

        // From the slot index, calculate the `Instant` at which it needs to be
        // processed. This value *must* be in the future with respect to `now`.

        let level_range = level_range(self.level);
        let slot_range = slot_range(self.level);

        // Compute the start date of the current level by masking the low bits
        // of `now` (`level_range` is a power of 2).
        // 层级的开始时间
        let level_start = now & !(level_range - 1);
        // 超时时间
        let mut deadline = level_start + slot as u64 * slot_range;

        if deadline <= now {
            // A timer is in a slot "prior" to the current time. This can occur
            // because we do not have an infinite hierarchy of timer levels, and
            // eventually a timer scheduled for a very distant time might end up
            // being placed in a slot that is beyond the end of all of the
            // arrays.
            //
            // To deal with this, we first limit timers to being scheduled no
            // more than MAX_DURATION ticks in the future; that is, they're at
            // most one rotation of the top level away. Then, we force timers
            // that logically would go into the top+1 level, to instead go into
            // the top level's slots.
            //
            // What this means is that the top level's slots act as a
            // pseudo-ring buffer, and we rotate around them indefinitely. If we
            // compute a deadline before now, and it's the top level, it
            // therefore means we're actually looking at a slot in the future.
            debug_assert_eq!(self.level, super::NUM_LEVELS - 1);

            deadline += level_range;
        }

        debug_assert!(
            deadline >= now,
            "deadline={:016X}; now={:016X}; level={}; lr={:016X}, sr={:016X}, slot={}; occupied={:b}",
            deadline,
            now,
            self.level,
            level_range,
            slot_range,
            slot,
            self.occupied
        );

        Some(Expiration {
            level: self.level,
            slot,
            deadline,
        })
    }

    fn next_occupied_slot(&self, now: u64) -> Option<usize> {
        if self.occupied == 0 {
            return None;
        }

        // Get the slot for now using Maths
        // 占用的槽位
        let now_slot = (now / slot_range(self.level)) as usize;
        // 槽位的掩码
        let occupied = self.occupied.rotate_right(now_slot as u32);
        // 第一个被占用的槽
        let zeros = occupied.trailing_zeros() as usize;
        // 实际的槽号
        let slot = (zeros + now_slot) % LEVEL_MULT;

        Some(slot)
    }

    // 添加任务
    pub(crate) unsafe fn add_entry(&mut self, item: TimerHandle) {
        let slot = slot_for(item.cached_when(), self.level);

        self.slot[slot].push_front(item);

        // 设置掩码
        self.occupied |= occupied_bit(slot);
    }

    // 移除任务
    pub(crate) unsafe fn remove_entry(&mut self, item: NonNull<TimerShared>) {
        let slot = slot_for(unsafe { item.as_ref().cached_when() }, self.level);

        unsafe { self.slot[slot].remove(item) };
        if self.slot[slot].is_empty() {
            // The bit is currently set
            debug_assert!(self.occupied & occupied_bit(slot) != 0);

            // Unset the bit
            // 取消掩码
            self.occupied ^= occupied_bit(slot);
        }
    }

    pub(crate) fn take_slot(&mut self, slot: usize) -> EntryList {
        self.occupied &= !occupied_bit(slot);

        std::mem::take(&mut self.slot[slot])
    }
}

impl fmt::Debug for Level {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Level")
            .field("occupied", &self.occupied)
            .finish()
    }
}

fn occupied_bit(slot: usize) -> u64 {
    1 << slot
}

/// 根据level计数槽的范围
fn slot_range(level: usize) -> u64 {
    LEVEL_MULT.pow(level as u32) as u64
}

// 计数层级的范围
fn level_range(level: usize) -> u64 {
    LEVEL_MULT as u64 * slot_range(level)
}

/// Converts a duration (milliseconds) and a level to a slot position.
/// 将时间和层级转换成插槽
fn slot_for(duration: u64, level: usize) -> usize {
    ((duration >> (level * 6)) % LEVEL_MULT as u64) as usize
}

#[cfg(all(test, not(loom)))]
mod test {
    use super::*;

    #[test]
    fn test_slot_for() {
        for pos in 0..64 {
            assert_eq!(pos as usize, slot_for(pos, 0));
        }

        for level in 1..5 {
            for pos in level..64 {
                let a = pos * 64_usize.pow(level as u32);
                assert_eq!(pos, slot_for(a as u64, level));
            }
        }
    }
}
