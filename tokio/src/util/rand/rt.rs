use super::{FastRand, RngSeed};

use std::sync::Mutex;

/// A deterministic generator for seeds (and other generators).
///
/// Given the same initial seed, the generator will output the same sequence of seeds.
///
/// Since the seed generator will be kept in a runtime handle, we need to wrap `FastRand`
/// in a Mutex to make it thread safe. Different to the `FastRand` that we keep in a
/// thread local store, the expectation is that seed generation will not need to happen
/// very frequently, so the cost of the mutex should be minimal.
/// 种子生成器
#[derive(Debug)]
pub(crate) struct RngSeedGenerator {
    /// Internal state for the seed generator. We keep it in a Mutex so that we can safely
    /// use it across multiple threads.
    /// 种子生成器的内部状态.我们将其保存在 Mutex 中,以便我们可以安全地在多个线程中使用它.
    state: Mutex<FastRand>,
}

impl RngSeedGenerator {
    /// Returns a new generator from the provided seed.
    /// 从提供的种子返回一个新的生成器.
    pub(crate) fn new(seed: RngSeed) -> Self {
        Self {
            state: Mutex::new(FastRand::from_seed(seed)),
        }
    }

    /// Returns the next seed in the sequence.
    /// 返回序列中的下一个种子.
    pub(crate) fn next_seed(&self) -> RngSeed {
        let mut rng = self
            .state
            .lock()
            .expect("RNG seed generator is internally corrupt");

        let s = rng.fastrand();
        let r = rng.fastrand();

        RngSeed::from_pair(s, r)
    }

    /// Directly creates a generator using the next seed.
    /// 直接使用下一个种子创建一个生成器.
    pub(crate) fn next_generator(&self) -> Self {
        RngSeedGenerator::new(self.next_seed())
    }
}

impl FastRand {
    /// Replaces the state of the random number generator with the provided seed, returning
    /// the seed that represents the previous state of the random number generator.
    ///
    /// The random number generator will become equivalent to one created with
    /// the same seed.
    /// 替换随机数种子
    pub(crate) fn replace_seed(&mut self, seed: RngSeed) -> RngSeed {
        let old_seed = RngSeed::from_pair(self.one, self.two);

        self.one = seed.s;
        self.two = seed.r;

        old_seed
    }
}
