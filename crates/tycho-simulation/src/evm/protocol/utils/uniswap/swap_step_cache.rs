//! The steps an exact-input swap already took from one pool state, so a later quote can start past
//! them.
//!
//! A swap takes the same steps from the same start whatever the amount, and only its last step
//! depends on how much input is left. A step that reaches its target takes the same input, fee
//! and `amount_out` for every input large enough to reach it. So each such step is cached
//! once, as a [`CachedStep`], and a later quote starts from the furthest cached step its input
//! reaches. From there the swap loop runs as it always does, so the last step, the price limit and
//! every error come from the same code as a swap with no cache.
//!
//! The cache sits behind an `Arc`, so the clones of one pool state share it.

use std::{
    fmt,
    sync::{Arc, Mutex, PoisonError},
};

use alloy::primitives::{I256, U256};

use super::{pool_tick::PoolTick, StepComputation, SwapState, FEE_PIPS_DENOMINATOR};

/// Steps cached for one swap direction. A swap that goes past the last cached step runs its
/// remaining steps without caching them, so one direction of one pool state holds at most about
/// 100 KB.
const MAX_CACHED_STEPS: usize = 512;

/// The swap state at the end of a cached step, or at the start of the swap.
#[derive(Clone, Debug)]
pub(crate) struct CachedStep {
    /// The smallest input with which every step up to and including this one reaches its target.
    min_input: U256,
    /// Input spent so far, fees included.
    amount_in: U256,
    amount_out: U256,
    sqrt_price: U256,
    tick: i32,
    liquidity: u128,
    /// Gas so far, without the settlement gas a swap adds at the end.
    gas: U256,
}

impl CachedStep {
    /// The state a swap starts from, before any step.
    pub(crate) fn origin(sqrt_price: U256, tick: i32, liquidity: u128, gas: U256) -> Self {
        CachedStep {
            min_input: U256::ZERO,
            amount_in: U256::ZERO,
            amount_out: U256::ZERO,
            sqrt_price,
            tick,
            liquidity,
            gas,
        }
    }

    /// Moves `state` to the end of this step and returns the gas used so far.
    ///
    /// `amount_specified` is the swap's exact input, with the swap loop's own sign: v3 passes it
    /// positive and v4 negative. The amount left moves towards zero either way.
    pub(crate) fn resume(&self, state: &mut SwapState, amount_specified: I256) -> U256 {
        let spent = I256::from_raw(self.amount_in);
        state.amount_remaining = if amount_specified.is_negative() {
            amount_specified + spent
        } else {
            amount_specified - spent
        };
        state.amount_calculated = -I256::from_raw(self.amount_out);
        state.sqrt_price = self.sqrt_price;
        state.tick = PoolTick::from(self.tick);
        state.liquidity = self.liquidity;
        self.gas
    }
}

/// The cached steps of one swap direction, and the fee they were taken with. The first entry is
/// the start of the swap, before any step.
#[derive(Default)]
struct DirectionCache {
    fee_pips: u32,
    steps: Vec<CachedStep>,
}

/// The swap steps cached for one pool state, one direction each: index 0 sells token1, index 1
/// sells token0.
#[derive(Clone, Default)]
pub(crate) struct SwapStepCache {
    directions: Arc<[Mutex<DirectionCache>; 2]>,
}

impl fmt::Debug for SwapStepCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SwapStepCache")
    }
}

/// Two states compare equal whatever their caches hold.
impl PartialEq for SwapStepCache {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for SwapStepCache {}

impl SwapStepCache {
    /// The smallest input that reaches the end of each cached step of one direction.
    #[cfg(test)]
    pub(crate) fn min_inputs(&self, zero_for_one: bool) -> Vec<U256> {
        self.directions[usize::from(zero_for_one)]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .steps
            .iter()
            .map(|cached| cached.min_input)
            .collect()
    }

    /// Starts a swap of `input` at the furthest cached step it reaches, and returns the recorder
    /// that caches the steps after it.
    ///
    /// Returns `None` when `fee_pips` is 1,000,000 or more, or when the cache was taken from a
    /// start other than `origin` or with another fee.
    pub(crate) fn begin(
        &self,
        zero_for_one: bool,
        origin: CachedStep,
        input: U256,
        fee_pips: u32,
    ) -> Option<StepRecorder<'_>> {
        if fee_pips >= FEE_PIPS_DENOMINATOR {
            return None;
        }
        let cache_lock = &self.directions[usize::from(zero_for_one)];
        let mut cache = cache_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match cache.steps.first() {
            None => {
                cache.fee_pips = fee_pips;
                cache.steps.push(origin);
            }
            Some(first)
                if cache.fee_pips == fee_pips &&
                    first.sqrt_price == origin.sqrt_price &&
                    first.tick == origin.tick &&
                    first.liquidity == origin.liquidity => {}
            Some(_) => return None,
        }
        // The swap's start needs no input, so at least one entry is reached.
        let index = cache
            .steps
            .partition_point(|cached| cached.min_input <= input) -
            1;
        let last = cache.steps[index].clone();
        Some(StepRecorder { cache: cache_lock, input, index, last })
    }
}

/// Caches the steps of one swap that go past the end of its cache.
pub(crate) struct StepRecorder<'a> {
    cache: &'a Mutex<DirectionCache>,
    input: U256,
    index: usize,
    last: CachedStep,
}

impl StepRecorder<'_> {
    /// The cached step the swap starts from.
    pub(crate) fn start(&self) -> &CachedStep {
        &self.last
    }

    /// How many steps the swap took before the cached step it starts from.
    pub(crate) fn steps_taken(&self) -> usize {
        self.index
    }

    /// Caches the step the swap loop just applied to `state`, when this swap's input took the step
    /// all the way to its target. Returns `None` once a step depends on the input or the cache is
    /// full, since no later step can then be cached.
    ///
    /// A step reaches its target when the input left after fees covers its input, that is when
    /// `left * (1e6 - fee) / 1e6 >= amount_in`. So the smallest input that reaches it is the input
    /// spent before it plus `ceil(amount_in * 1e6 / (1e6 - fee))`, which equals the step's
    /// `amount_in_with_fee` since its fee is `ceil(amount_in * fee / (1e6 - fee))`.
    pub(crate) fn after_step(
        mut self,
        state: &SwapState,
        step: &StepComputation,
        reached_target: bool,
        gas: U256,
    ) -> Option<Self> {
        if !reached_target {
            return None;
        }
        // A swap stops when no input is left, so a step runs only with at least one unit left.
        let threshold = self.last.amount_in.checked_add(
            step.amount_in_with_fee
                .max(U256::from(1u64)),
        )?;
        if self.input < threshold {
            return None;
        }
        let amount_in = self
            .last
            .amount_in
            .checked_add(step.amount_in_with_fee)?;
        let amount_out = self
            .last
            .amount_out
            .checked_add(step.amount_out)?;
        let cached = CachedStep {
            min_input: self.last.min_input.max(threshold),
            amount_in,
            amount_out,
            sqrt_price: state.sqrt_price,
            tick: state.tick.value(),
            liquidity: state.liquidity,
            gas,
        };
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Another quote may have cached this step first. Every input that reaches a step caches
        // the same values, so the cached one serves.
        if cache.steps.len() == self.index + 1 {
            if cache.steps.len() >= MAX_CACHED_STEPS {
                return None;
            }
            cache.steps.push(cached.clone());
        }
        drop(cache);
        self.index += 1;
        self.last = cached;
        Some(self)
    }
}

/// A pool and amounts that the v3 and v4 step cache tests share.
#[cfg(test)]
pub(crate) mod test_fixtures {
    use std::str::FromStr;

    use num_bigint::BigUint;
    use tycho_common::{
        hex_bytes::Bytes,
        models::{token::Token, Chain},
    };

    use crate::evm::protocol::utils::uniswap::tick_list::TickInfo;

    pub(crate) const LIQUIDITY: u128 = 377952820878029838;
    pub(crate) const SQRT_PRICE: &str = "28437325270877025820973479874632004";
    pub(crate) const TICK: i32 = 255830;

    pub(crate) fn wbtc_weth() -> (Token, Token) {
        let wbtc = Token::new(
            &Bytes::from_str("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599").unwrap(),
            "WBTC",
            8,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        let weth = Token::new(
            &Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            "WETH",
            18,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        );
        (wbtc, weth)
    }

    pub(crate) fn test_ticks() -> Vec<TickInfo> {
        [
            (255760, 1759015528199933i128),
            (255770, 6393138051835308),
            (255780, 228206673808681),
            (255820, 1319490609195820),
            (255830, 678916926147901),
            (255840, 12208947683433103),
            (255850, 1177970713095301),
            (255860, 8752304680520407),
            (255880, 1486478248067104),
            (255890, 1878744276123248),
            (255900, 77340284046725227),
        ]
        .into_iter()
        .map(|(index, net_liquidity)| TickInfo::new(index, net_liquidity).unwrap())
        .collect()
    }

    /// 70 amounts, from `smallest` and doubling each time, which ends far past the pool's depth.
    pub(crate) fn test_amounts(smallest: u64) -> Vec<BigUint> {
        (0..70u32)
            .map(|doubling| BigUint::from(smallest) << doubling)
            .collect()
    }

    /// The amounts ascending, descending, and the even positions before the odd ones.
    pub(crate) fn amount_orders(amounts: &[BigUint]) -> Vec<Vec<BigUint>> {
        let mut even_then_odd: Vec<BigUint> = amounts
            .iter()
            .step_by(2)
            .cloned()
            .collect();
        even_then_odd.extend(
            amounts
                .iter()
                .skip(1)
                .step_by(2)
                .cloned(),
        );
        vec![amounts.to_vec(), amounts.iter().rev().cloned().collect(), even_then_odd]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEE_PIPS: u32 = 3_000;

    fn origin() -> CachedStep {
        CachedStep::origin(U256::from(1_000u64), 10, 5_000, U256::from(100u64))
    }

    fn state_at(sqrt_price: u64, tick: i32) -> SwapState {
        SwapState {
            amount_remaining: I256::ZERO,
            amount_calculated: I256::ZERO,
            sqrt_price: U256::from(sqrt_price),
            tick: PoolTick::from(tick),
            liquidity: 5_000,
        }
    }

    fn step(amount_in_with_fee: u64, amount_out: u64) -> StepComputation {
        StepComputation {
            sqrt_price_start: U256::ZERO,
            tick_next: 0,
            sqrt_price_next: U256::ZERO,
            amount_in_with_fee: U256::from(amount_in_with_fee),
            amount_out: U256::from(amount_out),
        }
    }

    /// Caches one step that spends 1,000 with its fee, so it needs an input of exactly 1,000.
    fn cache_one_step(cache: &SwapStepCache, input: u64) -> Option<StepRecorder<'_>> {
        cache
            .begin(true, origin(), U256::from(input), FEE_PIPS)?
            .after_step(&state_at(900, 9), &step(1_000, 1_994), true, U256::from(150u64))
    }

    #[test]
    fn test_begin_empty_cache() {
        let cache = SwapStepCache::default();

        let recorder = cache
            .begin(true, origin(), U256::from(5u64), FEE_PIPS)
            .unwrap();

        assert_eq!(recorder.start().sqrt_price, U256::from(1_000u64));
        assert_eq!(cache.min_inputs(true), [U256::ZERO]);
        assert!(cache.min_inputs(false).is_empty());
    }

    #[test]
    fn test_after_step_input_below_threshold() {
        let cache = SwapStepCache::default();

        let recorder = cache_one_step(&cache, 999);

        assert!(recorder.is_none());
        assert_eq!(cache.min_inputs(true), [U256::ZERO]);
    }

    #[test]
    fn test_after_step_input_at_threshold() {
        let cache = SwapStepCache::default();

        let recorder = cache_one_step(&cache, 1_000);

        assert!(recorder.is_some());
        assert_eq!(cache.min_inputs(true), [U256::ZERO, U256::from(1_000u64)]);
    }

    #[test]
    fn test_begin_resumes_from_the_furthest_cached_step() {
        let cache = SwapStepCache::default();
        cache_one_step(&cache, 1_000).unwrap();

        let below = cache
            .begin(true, origin(), U256::from(999u64), FEE_PIPS)
            .unwrap();
        let at = cache
            .begin(true, origin(), U256::from(1_000u64), FEE_PIPS)
            .unwrap();

        assert_eq!(below.start().sqrt_price, U256::from(1_000u64));
        assert_eq!(at.start().sqrt_price, U256::from(900u64));
        assert_eq!(at.start().amount_in, U256::from(1_000u64));
        assert_eq!(at.start().gas, U256::from(150u64));
    }

    #[test]
    fn test_after_step_zero_amount_in_with_no_input_left() {
        let cache = SwapStepCache::default();
        let recorder = cache_one_step(&cache, 1_000).unwrap();

        let recorder =
            recorder.after_step(&state_at(800, 8), &step(0, 0), true, U256::from(200u64));

        assert!(recorder.is_none());
        assert_eq!(cache.min_inputs(true).len(), 2);
    }

    #[test]
    fn test_after_step_zero_amount_in_with_input_left() {
        let cache = SwapStepCache::default();
        let recorder = cache_one_step(&cache, 1_001).unwrap();

        let recorder =
            recorder.after_step(&state_at(800, 8), &step(0, 0), true, U256::from(200u64));

        assert!(recorder.is_some());
        assert_eq!(
            cache.min_inputs(true),
            [U256::ZERO, U256::from(1_000u64), U256::from(1_001u64)]
        );
    }

    #[test]
    fn test_after_step_not_reaching_target() {
        let cache = SwapStepCache::default();
        let recorder = cache
            .begin(true, origin(), U256::from(5_000u64), FEE_PIPS)
            .unwrap();

        let recorder =
            recorder.after_step(&state_at(900, 9), &step(1_000, 1_994), false, U256::from(150u64));

        assert!(recorder.is_none());
        assert_eq!(cache.min_inputs(true), [U256::ZERO]);
    }

    #[test]
    fn test_begin_other_origin_or_fee() {
        let cache = SwapStepCache::default();
        cache_one_step(&cache, 1_000).unwrap();
        let moved = CachedStep::origin(U256::from(999u64), 10, 5_000, U256::from(100u64));

        let from_moved = cache.begin(true, moved, U256::from(1_000u64), FEE_PIPS);
        let other_fee = cache.begin(true, origin(), U256::from(1_000u64), FEE_PIPS + 1);
        let whole_fee = cache.begin(true, origin(), U256::from(1_000u64), FEE_PIPS_DENOMINATOR);

        assert!(from_moved.is_none());
        assert!(other_fee.is_none());
        assert!(whole_fee.is_none());
    }

    #[test]
    fn test_after_step_full_cache() {
        let cache = SwapStepCache::default();
        let mut recorder = cache
            .begin(true, origin(), U256::MAX, FEE_PIPS)
            .unwrap();
        for _ in 1..MAX_CACHED_STEPS {
            recorder = recorder
                .after_step(&state_at(900, 9), &step(1_000, 1_994), true, U256::from(150u64))
                .unwrap();
        }

        let recorder =
            recorder.after_step(&state_at(900, 9), &step(1_000, 1_994), true, U256::from(150u64));

        assert!(recorder.is_none());
        assert_eq!(cache.min_inputs(true).len(), MAX_CACHED_STEPS);
    }

    #[test]
    fn test_resume_amount_sign() {
        let cache = SwapStepCache::default();
        cache_one_step(&cache, 1_000).unwrap();
        let recorder = cache
            .begin(true, origin(), U256::from(5_000u64), FEE_PIPS)
            .unwrap();
        let mut v3_state = state_at(0, 0);
        let mut v4_state = state_at(0, 0);

        let gas = recorder
            .start()
            .resume(&mut v3_state, I256::try_from(5_000).unwrap());
        recorder
            .start()
            .resume(&mut v4_state, I256::try_from(-5_000).unwrap());

        assert_eq!(gas, U256::from(150u64));
        assert_eq!(v3_state.amount_remaining, I256::try_from(4_000).unwrap());
        assert_eq!(v4_state.amount_remaining, I256::try_from(-4_000).unwrap());
        assert_eq!(v3_state.amount_calculated, I256::try_from(-1_994).unwrap());
        assert_eq!(v3_state.sqrt_price, U256::from(900u64));
    }
}
