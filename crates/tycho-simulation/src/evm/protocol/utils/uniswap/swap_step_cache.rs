//! The steps an exact-input swap already took from one pool state, so a later quote can start past
//! them.
//!
//! A swap takes the same steps from the same start whatever the amount, and only its last step
//! depends on how much input is left. A step that reaches its target takes the same input, fee
//! and `amount_out` for every input large enough to reach it. So the swap's progress after each
//! such step is cached once, as a [`CachedSwapProgress`], and a later quote starts from the
//! furthest cached progress its input reaches. From there the swap loop runs as it always does, so
//! the last step, the price limit and every error come from the same code as a swap with no cache.
//!
//! [`SwapStepCache`] only stores progress. [`StepCacheRun`] decides, for one swap, which cached
//! progress it starts from and which progress it caches.
//!
//! The cache sits behind an `Arc`, so the clones of one pool state share it.

use std::{
    fmt,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use alloy::primitives::U256;

use super::{StepComputation, SwapState, FEE_PIPS_DENOMINATOR};

/// The most steps that one direction of one pool state caches, about 100 KB. A swap that goes past
/// the last cached step takes its other steps without caching them.
const MAX_CACHED_STEPS: usize = 512;

/// The price, tick, liquidity, fee and gas that a swap starts from. Cached steps hold only for a
/// swap that starts from the same values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StepCacheKey {
    sqrt_price: U256,
    tick: i32,
    liquidity: u128,
    fee_pips: u32,
    start_gas: U256,
}

impl StepCacheKey {
    /// The key of a swap that starts from these values. Returns `None` when `fee_pips` is
    /// 1,000,000 or more, since no input then takes a step to its target.
    pub(crate) fn new(
        sqrt_price: U256,
        tick: i32,
        liquidity: u128,
        fee_pips: u32,
        start_gas: U256,
    ) -> Option<Self> {
        (fee_pips < FEE_PIPS_DENOMINATOR).then_some(StepCacheKey {
            sqrt_price,
            tick,
            liquidity,
            fee_pips,
            start_gas,
        })
    }
}

/// A swap after its first `steps_taken` steps, each taken all the way to its target: the input,
/// output and gas of those steps together, and the pool's price, tick and liquidity after them.
#[derive(Clone, Debug)]
pub(crate) struct CachedSwapProgress {
    pub(crate) steps_taken: usize,
    /// The smallest swap input that takes all `steps_taken` steps to their targets.
    ///
    /// It is `amount_in` when every step spent input. A step can spend no input, for example to
    /// cross a range with no liquidity. The swap loop runs a step only while input is left, so
    /// such a step needs one more unit than the input spent before it.
    pub(crate) min_input: U256,
    /// Input the steps spent, fees included.
    pub(crate) amount_in: U256,
    pub(crate) amount_out: U256,
    pub(crate) sqrt_price: U256,
    pub(crate) tick: i32,
    pub(crate) liquidity: u128,
    /// Gas after the steps, start gas included, without the settlement gas a swap adds at the end.
    pub(crate) gas: U256,
}

/// `previous` with one more step: the step that the swap loop just applied to `state`. Pass `None`
/// for `previous` before the first step. Returns `None` when `input` does not take the step to its
/// target.
///
/// A step reaches its target when the input left after fees covers its input, that is when
/// `left * (1e6 - fee) / 1e6 >= amount_in`. So the smallest input that reaches it is the input
/// spent before it plus `ceil(amount_in * 1e6 / (1e6 - fee))`, which equals the step's
/// `amount_in_with_fee` since its fee is `ceil(amount_in * fee / (1e6 - fee))`.
fn next_progress(
    previous: Option<&CachedSwapProgress>,
    input: U256,
    step: &StepComputation,
    state: &SwapState,
    gas: U256,
) -> Option<CachedSwapProgress> {
    if state.sqrt_price != step.sqrt_price_next {
        return None;
    }
    let (steps_before, min_input_before, amount_in_before, amount_out_before) = match previous {
        Some(previous) => {
            (previous.steps_taken, previous.min_input, previous.amount_in, previous.amount_out)
        }
        None => (0, U256::ZERO, U256::ZERO, U256::ZERO),
    };
    let min_input = amount_in_before
        .checked_add(
            step.amount_in_with_fee
                .max(U256::from(1u64)),
        )?
        .max(min_input_before);
    if input < min_input {
        return None;
    }
    Some(CachedSwapProgress {
        steps_taken: steps_before + 1,
        min_input,
        amount_in: amount_in_before.checked_add(step.amount_in_with_fee)?,
        amount_out: amount_out_before.checked_add(step.amount_out)?,
        sqrt_price: state.sqrt_price,
        tick: state.tick.value(),
        liquidity: state.liquidity,
        gas,
    })
}

/// The cached progress of one swap direction, all from `key`. `progress[i]` covers the first
/// `i + 1` steps, so the list is sorted by `steps_taken` and by `min_input`.
#[derive(Default)]
struct DirectionCache {
    key: Option<StepCacheKey>,
    progress: Vec<CachedSwapProgress>,
}

/// The swap steps cached for one pool state, stored apart for each direction: index 0 sells
/// token1, index 1 sells token0.
#[derive(Clone, Default)]
pub(crate) struct SwapStepCache {
    directions: Arc<[Mutex<DirectionCache>; 2]>,
}

impl fmt::Debug for SwapStepCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SwapStepCache")
    }
}

/// Pool states derive `PartialEq`, and the cache holds no pool data, so any two caches are equal.
impl PartialEq for SwapStepCache {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for SwapStepCache {}

impl SwapStepCache {
    /// The smallest input that reaches each cached step of one direction, one value per step.
    #[cfg(test)]
    pub(crate) fn min_inputs(&self, zero_for_one: bool) -> Vec<U256> {
        self.direction(zero_for_one)
            .progress
            .iter()
            .map(|progress| progress.min_input)
            .collect()
    }

    /// The furthest cached progress that `input` reaches, or `None` when `input` reaches no cached
    /// step or the cache holds progress from another key.
    pub(crate) fn get(
        &self,
        zero_for_one: bool,
        key: StepCacheKey,
        input: U256,
    ) -> Option<CachedSwapProgress> {
        let cache = self.direction(zero_for_one);
        if cache.key != Some(key) {
            return None;
        }
        let reached = cache
            .progress
            .partition_point(|progress| progress.min_input <= input);
        reached
            .checked_sub(1)
            .map(|last| cache.progress[last].clone())
    }

    /// Adds `progress` after the last cached progress of its direction. Returns `false` when the
    /// cache cannot hold it: the cache is full, holds progress from another key, or has no entry
    /// for the step before it. Returns `true` when another quote already cached the same step,
    /// since every input that reaches a step caches the same values.
    pub(crate) fn insert(
        &self,
        zero_for_one: bool,
        key: StepCacheKey,
        progress: &CachedSwapProgress,
    ) -> bool {
        let mut cache = self.direction(zero_for_one);
        if cache.progress.is_empty() {
            cache.key = Some(key);
        } else if cache.key != Some(key) {
            return false;
        }
        let cached = cache.progress.len();
        if progress.steps_taken <= cached {
            return true;
        }
        if progress.steps_taken > cached + 1 || cached >= MAX_CACHED_STEPS {
            return false;
        }
        cache.progress.push(progress.clone());
        true
    }

    fn direction(&self, zero_for_one: bool) -> MutexGuard<'_, DirectionCache> {
        self.directions[usize::from(zero_for_one)]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The step cache's part in one swap: the cached progress the swap starts from, and the progress
/// it caches after each step.
pub(crate) struct StepCacheRun<'a> {
    cache: &'a SwapStepCache,
    zero_for_one: bool,
    /// `None` when the swap does not use the cache, and after a step that cannot be cached.
    key: Option<StepCacheKey>,
    input: U256,
    /// The progress after the last step this swap started from or cached.
    last: Option<CachedSwapProgress>,
}

impl<'a> StepCacheRun<'a> {
    /// Finds the furthest cached progress that `input` reaches. A `None` key turns the cache off
    /// for this swap.
    pub(crate) fn new(
        cache: &'a SwapStepCache,
        zero_for_one: bool,
        key: Option<StepCacheKey>,
        input: U256,
    ) -> Self {
        let last = key.and_then(|key| cache.get(zero_for_one, key, input));
        StepCacheRun { cache, zero_for_one, key, input, last }
    }

    /// Moves `state` to the cached progress the swap starts from, and returns that progress.
    /// Returns `None`, and leaves `state` unchanged, when no cached progress applies.
    pub(crate) fn apply_cached_progress(
        &self,
        state: &mut SwapState,
    ) -> Option<&CachedSwapProgress> {
        let progress = self.last.as_ref()?;
        state.apply_cached_progress(progress);
        Some(progress)
    }

    /// Caches the progress after the step the swap loop just applied to `state`, with `gas` as the
    /// gas used so far. The cache holds only consecutive steps from the swap's start, so a step
    /// that cannot be cached ends caching for the rest of the swap.
    pub(crate) fn cache_progress(&mut self, step: &StepComputation, state: &SwapState, gas: U256) {
        let Some(key) = self.key else {
            return;
        };
        let progress = next_progress(self.last.as_ref(), self.input, step, state, gas);
        let inserted = progress
            .as_ref()
            .is_some_and(|progress| {
                self.cache
                    .insert(self.zero_for_one, key, progress)
            });
        if inserted {
            self.last = progress;
        } else {
            self.key = None;
        }
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

    /// Three orders of the amounts: ascending, descending, and the amounts at even indices before
    /// those at odd indices.
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
    use alloy::primitives::I256;

    use super::*;
    use crate::evm::protocol::utils::uniswap::pool_tick::PoolTick;

    const FEE_PIPS: u32 = 3_000;
    const START_GAS: u64 = 100;

    fn key() -> StepCacheKey {
        StepCacheKey::new(U256::from(1_000u64), 10, 5_000, FEE_PIPS, U256::from(START_GAS)).unwrap()
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

    /// A step to `sqrt_price_next` that spends `amount_in_with_fee` and returns `amount_out`.
    fn step(sqrt_price_next: u64, amount_in_with_fee: u64, amount_out: u64) -> StepComputation {
        StepComputation {
            sqrt_price_start: U256::ZERO,
            tick_next: 0,
            sqrt_price_next: U256::from(sqrt_price_next),
            amount_in_with_fee: U256::from(amount_in_with_fee),
            amount_out: U256::from(amount_out),
        }
    }

    /// The first step spends 1,000, fee included, and reaches its target price of 900.
    fn first_step(input: u64) -> Option<CachedSwapProgress> {
        next_progress(
            None,
            U256::from(input),
            &step(900, 1_000, 1_994),
            &state_at(900, 9),
            U256::from(150u64),
        )
    }

    /// A second step that spends no input and reaches its target price of 800.
    fn second_step(previous: &CachedSwapProgress, input: u64) -> Option<CachedSwapProgress> {
        next_progress(
            Some(previous),
            U256::from(input),
            &step(800, 0, 0),
            &state_at(800, 8),
            U256::from(200u64),
        )
    }

    #[test]
    fn test_next_progress_input_below_min_input() {
        let progress = first_step(999);

        assert!(progress.is_none());
    }

    #[test]
    fn test_next_progress_input_at_min_input() {
        let progress = first_step(1_000).unwrap();

        assert_eq!(progress.steps_taken, 1);
        assert_eq!(progress.min_input, U256::from(1_000u64));
        assert_eq!(progress.amount_in, U256::from(1_000u64));
        assert_eq!(progress.amount_out, U256::from(1_994u64));
        assert_eq!(progress.gas, U256::from(150u64));
    }

    #[test]
    fn test_next_progress_step_short_of_its_target() {
        let progress = next_progress(
            None,
            U256::from(5_000u64),
            &step(800, 1_000, 1_994),
            &state_at(900, 9),
            U256::from(150u64),
        );

        assert!(progress.is_none());
    }

    #[test]
    fn test_next_progress_zero_amount_in_with_no_input_left() {
        let previous = first_step(1_000).unwrap();

        let progress = second_step(&previous, 1_000);

        assert!(progress.is_none());
    }

    #[test]
    fn test_next_progress_zero_amount_in_with_input_left() {
        let previous = first_step(1_001).unwrap();

        let progress = second_step(&previous, 1_001).unwrap();

        assert_eq!(progress.steps_taken, 2);
        assert_eq!(progress.min_input, U256::from(1_001u64));
        assert_eq!(progress.amount_in, U256::from(1_000u64));
    }

    #[test]
    fn test_key_fee_at_denominator() {
        let key = StepCacheKey::new(
            U256::from(1_000u64),
            10,
            5_000,
            FEE_PIPS_DENOMINATOR,
            U256::from(START_GAS),
        );

        assert!(key.is_none());
    }

    #[test]
    fn test_get_empty_cache() {
        let cache = SwapStepCache::default();

        let progress = cache.get(true, key(), U256::MAX);

        assert!(progress.is_none());
    }

    #[test]
    fn test_get_furthest_progress_the_input_reaches() {
        let cache = SwapStepCache::default();
        let first = first_step(1_001).unwrap();
        let second = second_step(&first, 1_001).unwrap();
        cache.insert(true, key(), &first);
        cache.insert(true, key(), &second);

        let below = cache.get(true, key(), U256::from(999u64));
        let at_first = cache.get(true, key(), U256::from(1_000u64));
        let at_second = cache.get(true, key(), U256::from(1_001u64));
        let other_direction = cache.get(false, key(), U256::from(1_001u64));

        assert!(below.is_none());
        assert_eq!(at_first.unwrap().steps_taken, 1);
        assert_eq!(at_second.unwrap().steps_taken, 2);
        assert!(other_direction.is_none());
    }

    #[test]
    fn test_get_other_key() {
        let cache = SwapStepCache::default();
        cache.insert(true, key(), &first_step(1_000).unwrap());
        let start_gas = U256::from(START_GAS);
        let moved = StepCacheKey::new(U256::from(999u64), 10, 5_000, FEE_PIPS, start_gas).unwrap();
        let other_fee =
            StepCacheKey::new(U256::from(1_000u64), 10, 5_000, FEE_PIPS + 1, start_gas).unwrap();
        let other_gas =
            StepCacheKey::new(U256::from(1_000u64), 10, 5_000, FEE_PIPS, start_gas + start_gas)
                .unwrap();

        let from_moved = cache.get(true, moved, U256::MAX);
        let with_other_fee = cache.get(true, other_fee, U256::MAX);
        let with_other_gas = cache.get(true, other_gas, U256::MAX);

        assert!(from_moved.is_none());
        assert!(with_other_fee.is_none());
        assert!(with_other_gas.is_none());
    }

    #[test]
    fn test_insert_step_already_cached() {
        let cache = SwapStepCache::default();
        let first = first_step(1_000).unwrap();
        cache.insert(true, key(), &first);

        let inserted = cache.insert(true, key(), &first);

        assert!(inserted);
        assert_eq!(cache.min_inputs(true), [U256::from(1_000u64)]);
    }

    #[test]
    fn test_insert_other_key() {
        let cache = SwapStepCache::default();
        cache.insert(true, key(), &first_step(1_000).unwrap());
        let moved =
            StepCacheKey::new(U256::from(999u64), 10, 5_000, FEE_PIPS, U256::from(START_GAS))
                .unwrap();

        let inserted = cache.insert(true, moved, &first_step(1_000).unwrap());

        assert!(!inserted);
    }

    #[test]
    fn test_insert_gap() {
        let cache = SwapStepCache::default();
        let mut second = first_step(1_000).unwrap();
        second.steps_taken = 2;

        let inserted = cache.insert(true, key(), &second);

        assert!(!inserted);
        assert!(cache.min_inputs(true).is_empty());
    }

    #[test]
    fn test_insert_full_cache() {
        let cache = SwapStepCache::default();
        let mut progress = first_step(1_000).unwrap();
        for steps_taken in 1..=MAX_CACHED_STEPS {
            progress.steps_taken = steps_taken;
            cache.insert(true, key(), &progress);
        }
        progress.steps_taken = MAX_CACHED_STEPS + 1;

        let inserted = cache.insert(true, key(), &progress);

        assert!(!inserted);
        assert_eq!(cache.min_inputs(true).len(), MAX_CACHED_STEPS);
    }

    #[test]
    fn test_run_caches_until_a_step_cannot_be_cached() {
        let cache = SwapStepCache::default();
        let mut run = StepCacheRun::new(&cache, true, Some(key()), U256::from(1_000u64));

        run.cache_progress(&step(900, 1_000, 1_994), &state_at(900, 9), U256::from(150u64));
        run.cache_progress(&step(800, 0, 0), &state_at(800, 8), U256::from(200u64));
        run.cache_progress(&step(700, 0, 0), &state_at(700, 7), U256::from(250u64));

        assert_eq!(cache.min_inputs(true), [U256::from(1_000u64)]);
    }

    #[test]
    fn test_run_applies_the_furthest_cached_progress() {
        let cache = SwapStepCache::default();
        cache.insert(true, key(), &first_step(1_000).unwrap());
        let run = StepCacheRun::new(&cache, true, Some(key()), U256::from(5_000u64));
        let mut state = state_at(1_000, 10);
        state.amount_remaining = I256::try_from(5_000).unwrap();

        let progress = run.apply_cached_progress(&mut state);

        assert_eq!(progress.unwrap().gas, U256::from(150u64));
        assert_eq!(state.amount_remaining, I256::try_from(4_000).unwrap());
        assert_eq!(state.sqrt_price, U256::from(900u64));
    }

    #[test]
    fn test_run_without_key() {
        let cache = SwapStepCache::default();
        cache.insert(true, key(), &first_step(1_000).unwrap());
        let run = StepCacheRun::new(&cache, true, None, U256::from(5_000u64));
        let mut state = state_at(1_000, 10);

        let progress = run.apply_cached_progress(&mut state);

        assert!(progress.is_none());
        assert_eq!(state.sqrt_price, U256::from(1_000u64));
    }
}
