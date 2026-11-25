use std::{any::Any, collections::HashMap};

use alloy::primitives::{address, aliases::U24, Address, I128, I256, U256, U512};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, BlockContext},
    },
    Bytes,
};

use super::{
    super::launchpad::{exact_input, swap_bound, INT128_MAX},
    decode_paused, MontraPoolConfig, PAUSED_ATTRIBUTE, POOL_CONFIG_ATTRIBUTES,
};
use crate::evm::protocol::{
    uniswap_v4::{
        hooks::{
            hook_handler::HookHandler,
            models::{
                AfterSwapDelta, AfterSwapParameters, BeforeSwapDelta, BeforeSwapOutput,
                BeforeSwapParameters, HookSpotFees, SwapOutcome, SwapParams, WithGasEstimate,
            },
        },
        state::UniswapV4State,
    },
    utils::{
        solidity_math::{mul_div, truncate_to_u256},
        uniswap::{
            lp_fee::OVERRIDE_FEE_FLAG, tick_math::get_sqrt_ratio_at_tick, FEE_PIPS_DENOMINATOR,
        },
    },
};

/// Montra's `LaunchHook` on Elysium (chain id 99901), deployed at block 588510 and not behind a
/// proxy. The low 14 bits of the address, `0x38ec`, grant `beforeSwap`, `afterSwap` and both
/// return-delta flags among the swap callbacks.
pub const MONTRA_HOOK_ELYSIUM: Address = address!("6b1216a40b65c83e9c9e18a562c446697b8cf8ec");

/// The immutable `lpLineHold()` of [`MONTRA_HOOK_ELYSIUM`]: the seconds the price must hold past
/// the LP line, in total, before the LP fee switches on. 600 at block 6002000.
pub(crate) const MONTRA_LP_LINE_HOLD_SECONDS_ELYSIUM: u64 = 600;

/// Gas of the hook's `beforeSwap` on a buy, the call frame alone: the lower median over the 261
/// buys of `tests/assets/hooks/montra/replay.json` (one window per 500,000 blocks from 588510 to
/// 6521167, each swap the first in its transaction through the hook), read from
/// `debug_traceTransaction` call traces. The range is 10,891 to 56,710; about 8% exceed 30,000,
/// writing an accrual slot from zero (a zero-to-nonzero SSTORE). The swap that switches a pool's
/// LP fee on also calls `updateDynamicLPFee` and costs 33,092 (block 713192), once per pool.
/// Re-measure by re-recording when the hook changes.
pub(crate) const MONTRA_BUY_BEFORE_SWAP_GAS: u64 = 22_491;

/// Gas of the hook's `afterSwap` on a buy, measured the same way: range 3,720 to 8,787.
pub(crate) const MONTRA_BUY_AFTER_SWAP_GAS: u64 = 5_570;

/// Gas of the hook's `beforeSwap` on a sell, measured the same way over the recording's 132
/// sells: range 1,913 to 3,913.
pub(crate) const MONTRA_SELL_BEFORE_SWAP_GAS: u64 = 3_913;

/// Gas of the hook's `afterSwap` on a sell, measured the same way: range 24,533 to 59,069, about
/// 10% above 30,000, writing an accrual slot from zero.
pub(crate) const MONTRA_SELL_AFTER_SWAP_GAS: u64 = 24_818;

/// Gas the PoolManager spends per hook call outside the call frames above: half of the
/// difference between the lower median, over the same swaps, of the PoolManager `swap` frame
/// minus both hook frames (32,616) and the lower median `swap` frame of the 58 hookless Elysium
/// v4 swaps in blocks 5990000-5990400 (27,256), as the recording holds them.
///
/// The difference is what `Hooks.sol` adds around each of the two calls: building and
/// ABI-encoding the call (`PoolKey`, `SwapParams`, hook data), the `CALL` itself (the hook is a
/// warm account after its first call), decoding and checking the returned selector and deltas,
/// and applying the hook's delta to the swap's `BalanceDelta` and the hook's own account delta.
/// The trace does not split those further, so the two calls are taken to cost the same. The
/// hookless sample may take a different number of swap steps than a Montra swap; the step gas
/// itself comes from `SwapGasCosts::UNISWAP_V4`, fitted on Ethereum and not re-measured on
/// Elysium. A quote assumes an external sender: the hook exempts its factory's own swaps from
/// its fees, which no router path can be.
pub(crate) const MONTRA_HOOK_DISPATCH_GAS: u64 = 2_680;

/// Native model of Montra's `LaunchHook` for one pool. The hook has no published source; the
/// model follows its behaviour as measured on chain and on forks of Elysium (`eth_call` probes
/// and replayed swaps, cited by block beside each test in `tests.rs`), and the replay test
/// (`tests/assets/hooks/montra/replay.json`) checks it against recorded swaps to the wei.
///
/// Every exact-input swap pays a fee in the pool's quote asset of `h = rate - lpInForce` pips:
/// - a buy (input = quote) pays `floor(amountIn * h / 1e6)` out of its input in `beforeSwap`, and
///   the pool swaps the rest;
/// - a sell pays `floor(grossOut * h / 1e6)` out of the pool's output in `afterSwap`.
///
/// `rate` is the pool's total fee for sells, and for buys the anti-snipe rate, which decays to
/// the total over the launch's window ([`MontraPoolConfig::buy_fee_pips`]). `lpInForce` is the
/// LP fee the pool charges inside the swap math, which `before_swap` returns as the LP fee
/// override ([`MontraPoolConfig::lp_fee_in_force_pips`]).
///
/// A quote depends on the block timestamp, so the same state quotes differently in a later
/// block, in two ways only: a buy's anti-snipe rate decays every second until
/// `earlyBuyFeeUntil`, and the LP fee switches on once a pool at `LpCrossed` has held past its LP
/// line for [`MONTRA_LP_LINE_HOLD_SECONDS_ELYSIUM`]. `apply_block` moves the handler to the block
/// a quote is for, and reports a change when one of those two did or when the terms fail to
/// compute before or after the move.
///
/// That block's timestamp is an estimate: the decoder prices every state for the next block at
/// the latest header's timestamp plus the chain's `block_time_secs`, 1 on Elysium. Elysium makes
/// blocks about every 100 ms with whole-second timestamps, so the next block usually carries the
/// same timestamp as the latest one (15 of 30 consecutive blocks sampled), and a quote is then
/// one second ahead of the block the swap lands in. Inside an anti-snipe window the buy rate
/// falls by `(start - total) / window` pips per second, so such a buy is quoted at a rate up to
/// one second's decay below what it pays (97,000 pips for a 10-second window from 990,000 to
/// 20,000; about 540 for 1,800 seconds), and its output is overstated by that much. The LP fee
/// can likewise be quoted on one second before it switches on, which understates a quote. Outside
/// a window and an LP hold the timestamp changes nothing.
///
/// The hook refuses a swap the pool cannot fill in full (`PartialFillRejected`) and a sell that
/// leaves the price past the pool's opening tick (`PastOpening`); `check_swap_outcome` refuses
/// both with `InvalidInput`. A swap whose output is 0 settles on chain, as a sell close enough to
/// the opening does, but no router can take it: its minimum output would have to be 0.
/// `after_swap` refuses it with `InvalidInput`, and `max_amount_in` reports a limit of 0 where
/// the output at a direction's bound is 0.
///
/// A paused pool cannot trade: [`HookHandler::swaps_enabled`] is `false`, so `get_limits`
/// reports `(0, 0)` for it whatever its terms. It keeps the spot price of the terms it holds.
///
/// Terms the hook would refuse ([`MontraPoolConfig::check_terms`]) make every quote and spot
/// price of the pool fail with `InvalidInput` naming them, whether or not the pool is paused,
/// since neither can be computed from them. `get_limits` of an unpaused pool fails the same way,
/// through [`HookHandler::max_amount_in`]. `before_swap` refuses a paused pool's swaps with
/// `InvalidInput` naming the pause once its terms are ones the hook accepts.
///
/// `query_pool_swap` refuses every Montra pool with `InvalidInput`, because [`Self::spot_fees`]
/// answers for both directions and the query runs the pool's swap math without the hook.
///
/// A quote leaves the handler's terms as they were: the stage machine (graduation, the LP line
/// crossing and its hold) advances from the `PoolConfig` write each swap makes, which the indexer
/// publishes, and at a fixed block timestamp no stage change alters a later quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MontraHookHandler {
    address: Address,
    config: MontraPoolConfig,
    paused: bool,
    /// Timestamp of the block quotes are for: the decoded block's at first, then the one
    /// `apply_block` last moved to.
    block_timestamp: u64,
}

/// What a quote depends on at a block timestamp: the buy rate and the LP fee in force. The sell
/// rate, the total fee, does not move with time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QuotingTerms {
    buy_fee_pips: u32,
    /// `None` while the pool charges no LP fee ([`MontraPoolConfig::lp_fee_in_force_pips`]).
    lp_fee_pips: Option<u32>,
}

impl MontraHookHandler {
    pub fn new(
        address: Address,
        config: MontraPoolConfig,
        paused: bool,
        block_timestamp: u64,
    ) -> Self {
        Self { address, config, paused, block_timestamp }
    }

    /// The terms at the handler's block timestamp. Fails as
    /// [`MontraPoolConfig::buy_fee_pips`] does.
    fn quoting_terms(&self) -> Result<QuotingTerms, SimulationError> {
        Ok(QuotingTerms {
            buy_fee_pips: self
                .config
                .buy_fee_pips(self.block_timestamp)?,
            lp_fee_pips: self
                .config
                .lp_fee_in_force_pips(self.block_timestamp),
        })
    }

    /// Whether a swap in the direction given by `zero_for_one` sells the quote asset into the
    /// pool, which is what the hook calls a buy.
    fn is_buy(&self, zero_for_one: bool) -> bool {
        zero_for_one == self.config.quote_is_currency0()
    }

    /// The fees of a swap in the direction given by `zero_for_one`: the LP fee in force, which
    /// the swap runs at as its LP fee override (`None`: the pool's own slot0 fee, 0 for a Montra
    /// pool, whose key carries the dynamic-fee flag), and the hook's fee `h = rate - lpInForce`
    /// in pips.
    ///
    /// Refuses with `InvalidInput` the terms [`MontraPoolConfig::check_terms`] refuses. Terms it
    /// accepts keep `lp <= total <= rate`, so the subtraction does not saturate.
    fn fees(&self, zero_for_one: bool) -> Result<HookSpotFees, SimulationError> {
        let terms = self.quoting_terms()?;
        let rate = if self.is_buy(zero_for_one) {
            terms.buy_fee_pips
        } else {
            self.config.total_fee_pips()
        };
        Ok(HookSpotFees {
            lp_fee_override: terms.lp_fee_pips,
            hook_fee_pips: rate.saturating_sub(terms.lp_fee_pips.unwrap_or(0)),
        })
    }

    fn refuse(&self, reason: &str) -> SimulationError {
        SimulationError::InvalidInput(format!("montra hook {}: {reason}", self.address), None)
    }

    /// What the hook does to a swap whose pool output is `gross_out`, in the direction given by
    /// `zero_for_one`: the fee it takes from that output (a sell's `floor(gross_out * h / 1e6)`
    /// at the sell's hook fee [`Self::fees`], nothing on a buy) and what the swapper receives
    /// after it.
    ///
    /// Refuses as [`Self::fees`] does and fails as [`hook_fee`] does. A sell's hook fee is below
    /// the total fee, at most 10%, so the fee never exceeds the output.
    fn split_output(
        &self,
        zero_for_one: bool,
        gross_out: U256,
    ) -> Result<OutputSplit, SimulationError> {
        let fee = if self.is_buy(zero_for_one) {
            U256::ZERO
        } else {
            hook_fee(gross_out, self.fees(zero_for_one)?.hook_fee_pips)?
        };
        Ok(OutputSplit { hook_fee: fee, swapper_out: gross_out.saturating_sub(fee) })
    }
}

/// A swap's pool output split between the hook's fee and the swapper.
struct OutputSplit {
    /// What the hook takes from the output.
    hook_fee: U256,
    /// What the swapper receives.
    swapper_out: U256,
}

/// `floor(amount * fee_pips / 1e6)`, the hook's fee on `amount`.
///
/// Fails if the fee does not fit 256 bits, which a fee of at most 100% on a `U256` never does.
fn hook_fee(amount: U256, fee_pips: u32) -> Result<U256, SimulationError> {
    mul_div(amount, U256::from(fee_pips), U256::from(FEE_PIPS_DENOMINATOR)).map_err(|e| {
        SimulationError::FatalError(format!(
            "montra hook: the fee of {fee_pips} pips on {amount}: {e}"
        ))
    })
}

/// `1e6 - fee_pips`, the share of an amount a fee of `fee_pips` leaves; fails when nothing is
/// left.
fn kept_pips(fee_pips: u32) -> Result<u32, SimulationError> {
    FEE_PIPS_DENOMINATOR
        .checked_sub(fee_pips)
        .filter(|kept| *kept > 0)
        .ok_or_else(|| {
            SimulationError::FatalError(format!(
                "montra hook: a fee of {fee_pips} pips leaves nothing to swap"
            ))
        })
}

/// The largest gross buy input `A` whose net `A - floor(A * h / 1e6)` the pool can still
/// swap within `max_net`.
///
/// The net is `ceil(A * (1e6 - h) / 1e6)`, which is at most `max_net` exactly when
/// `A * (1e6 - h) <= max_net * 1e6`, so the largest such `A` is
/// `floor(max_net * 1e6 / (1e6 - h))` (`test_max_gross_buy_is_the_largest_fitting_input` checks
/// it against the forward formula). Fails when the fee leaves nothing to swap or the result
/// overflows 256 bits.
pub(super) fn max_gross_buy(max_net: U256, hook_fee_pips: u32) -> Result<U256, SimulationError> {
    let kept_pips = kept_pips(hook_fee_pips)?;
    mul_div(max_net, U256::from(FEE_PIPS_DENOMINATOR), U256::from(kept_pips)).map_err(|e| {
        SimulationError::FatalError(format!(
            "montra hook: the gross buy for a net of {max_net} at {hook_fee_pips} pips: {e}"
        ))
    })
}

/// The square-root price at `tick`, inside the range a swap's price limit allows
/// ([`swap_bound`]); TickMath's own extreme is outside it at tick spacings 1, 2, 4 and 8.
fn swap_bound_sqrt_price(tick: i32, zero_for_one: bool) -> Result<U256, SimulationError> {
    Ok(swap_bound(get_sqrt_ratio_at_tick(tick)?, zero_for_one))
}

/// The largest exact input a swap sitting at `sqrt_price` with active `liquidity` takes
/// without moving the price, at a total swap fee of `fee_pips`.
///
/// v4-core's `SwapMath.computeSwapStep`, on a last step that stops short of its target,
/// swaps `less = floor(input * (1e6 - fee) / 1e6)` and keeps the rest as fee, and
/// `SqrtPriceMath.getNextSqrtPriceFromInput` moves the price by `less` rounded towards the
/// current price. For a token1 input (`getNextSqrtPriceFromAmount1RoundingDown`) the new
/// price is `sqrtP + floor(less * 2^96 / L)`, which stays put while `less * 2^96 < L`. For a
/// token0 input (`getNextSqrtPriceFromAmount0RoundingUp`) it is
/// `ceil(L * 2^96 * sqrtP / (L * 2^96 + less * sqrtP))`, which stays put while
/// `L * 2^96 > (sqrtP - 1) * sqrtP * less`. Such input settles with no output. With no
/// liquidity the step moves the price to its target whatever the input, so nothing is
/// absorbed.
pub(super) fn input_absorbed_at_price(
    sqrt_price: U256,
    liquidity: u128,
    zero_for_one: bool,
    fee_pips: u32,
) -> Result<U256, SimulationError> {
    if liquidity == 0 {
        return Ok(U256::ZERO);
    }
    let liquidity = U512::from(liquidity);
    let max_unmoving: U512 = if zero_for_one {
        let scaled_liquidity: U512 = liquidity << 96usize;
        let sqrt_price = U512::from(sqrt_price);
        let per_unit = (sqrt_price - U512::from(1)) * sqrt_price;
        (scaled_liquidity - U512::from(1)) / per_unit
    } else {
        (liquidity - U512::from(1)) >> 96usize
    };
    let denominator = U512::from(FEE_PIPS_DENOMINATOR);
    let kept = U512::from(kept_pips(fee_pips)?);
    // The largest input whose `less` is at most `max_unmoving`.
    let absorbed: U512 = ((max_unmoving + U512::from(1)) * denominator - U512::from(1)) / kept;
    truncate_to_u256(absorbed)
}

impl HookHandler for MontraHookHandler {
    fn address(&self) -> Address {
        self.address
    }

    /// Reports a change when the terms at the new block differ from those before, and also when
    /// either cannot be computed, so the caller re-quotes and meets the error.
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let before = self.quoting_terms();
        self.block_timestamp = block.timestamp();
        !matches!((before, self.quoting_terms()), (Ok(before), Ok(after)) if before == after)
    }

    /// `false` while the pool is paused, whatever its terms.
    fn swaps_enabled(&self) -> bool {
        !self.paused
    }

    /// Returns `true`. Every Montra pool quoted in `currency0` opens on its seed's upper edge
    /// with no active liquidity, and its first buy steps into the seed; [`Self::max_amount_in`]
    /// bounds both directions from the walk, so limits and quotes agree there.
    fn swaps_from_empty_range(&self) -> bool {
        true
    }

    /// The hook gas constants are the hook's call frames alone; this is the PoolManager's side.
    fn pool_manager_call_overhead(&self) -> u64 {
        MONTRA_HOOK_DISPATCH_GAS
    }

    /// Takes a buy's fee out of its input and sets the LP fee in force as the swap's override.
    ///
    /// Refuses with `InvalidInput`, in this order, terms the hook refuses
    /// ([`MontraPoolConfig::check_terms`]), a paused pool, a zero input (the pool manager reverts
    /// `SwapAmountCannotBeZero`), an exact-output swap, which Tycho does not quote, and an input
    /// above `type(int128).max`, which overflows the pool manager's deltas.
    fn before_swap(
        &self,
        params: BeforeSwapParameters,
        _overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        _transient_storage: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<BeforeSwapOutput>, SimulationError> {
        let zero_for_one = params.swap_params.zero_for_one;
        let fees = self.fees(zero_for_one)?;
        if self.paused {
            return Err(self.refuse("the pool is paused"));
        }
        let amount_in = exact_input(params.swap_params.amount_specified)
            .map_err(|reason| self.refuse(reason))?;

        let (fee, gas_estimate) = if self.is_buy(zero_for_one) {
            (hook_fee(amount_in, fees.hook_fee_pips)?, MONTRA_BUY_BEFORE_SWAP_GAS)
        } else {
            (U256::ZERO, MONTRA_SELL_BEFORE_SWAP_GAS)
        };
        // The fee is at most the input, which fits an int128, and a positive specified delta is
        // the hook's claim on the input. It sits in the upper 128 bits of a `BeforeSwapDelta`.
        let amount_delta = BeforeSwapDelta(I256::from_raw(fee) << 128);

        let fee_override = fees
            .lp_fee_override
            .map_or(U24::ZERO, |lp_fee_override| U24::from(lp_fee_override | OVERRIDE_FEE_FLAG));

        Ok(WithGasEstimate {
            gas_estimate,
            result: BeforeSwapOutput {
                amount_delta,
                fee: fee_override,
                overwrites: HashMap::new(),
                transient_storage: HashMap::new(),
            },
        })
    }

    /// Takes a sell's fee out of the pool's output; a buy pays nothing more here.
    ///
    /// Refuses with `InvalidInput` a swap that leaves the swapper an output of 0.
    fn after_swap(
        &self,
        params: AfterSwapParameters,
        _overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        _transient_storage_params: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<AfterSwapDelta>, SimulationError> {
        let zero_for_one = params.swap_params.zero_for_one;
        // An exact-input swap's output is the unspecified leg: currency1 when selling currency0.
        let gross_out = if zero_for_one { params.delta.amount1() } else { params.delta.amount0() };
        let split = self.split_output(zero_for_one, U256::from(gross_out.unsigned_abs()))?;
        if split.swapper_out.is_zero() {
            return Err(self.refuse("the swap's output is 0, which no minimum output accepts"));
        }
        let gas_estimate = if self.is_buy(zero_for_one) {
            MONTRA_BUY_AFTER_SWAP_GAS
        } else {
            MONTRA_SELL_AFTER_SWAP_GAS
        };
        let fee = split.hook_fee;
        let result = u128::try_from(fee)
            .ok()
            .and_then(|fee| I128::try_from(fee).ok())
            .ok_or_else(|| {
                SimulationError::FatalError(format!(
                    "montra hook {}: the fee {fee} is below the swap's int128 output, yet does \
                     not fit int128",
                    self.address
                ))
            })?;
        Ok(WithGasEstimate { gas_estimate, result })
    }

    /// Refuses, as the hook's `afterSwap` reverts, a swap the pool did not fill in full
    /// (`PartialFillRejected`) and a sell whose price ended past the opening (`PastOpening`). A
    /// sell that ends exactly at the opening settles.
    fn check_swap_outcome(
        &self,
        zero_for_one: bool,
        outcome: &SwapOutcome,
    ) -> Result<(), SimulationError> {
        if outcome.amount_unfilled != U256::ZERO {
            return Err(self.refuse(&format!(
                "PartialFillRejected: the pool could not swap {} of the input",
                outcome.amount_unfilled
            )));
        }
        if self.is_buy(zero_for_one) {
            return Ok(());
        }
        let opening = get_sqrt_ratio_at_tick(self.config.opening_tick())?;
        let past_opening = if zero_for_one {
            outcome.sqrt_price_x96 < opening
        } else {
            outcome.sqrt_price_x96 > opening
        };
        if past_opening {
            return Err(self.refuse("PastOpening: the sell takes the price past the opening tick"));
        }
        Ok(())
    }

    /// The hook's fee for the direction, as a fraction of the amount it is charged on.
    fn fee(&self, _context: &UniswapV4State, params: SwapParams) -> Result<f64, SimulationError> {
        let hook_fee_pips = self
            .fees(params.zero_for_one)?
            .hook_fee_pips;
        Ok(f64::from(hook_fee_pips) / f64::from(FEE_PIPS_DENOMINATOR))
    }

    /// The LP fee in force and the hook's fee for the direction. Answers while the pool is
    /// paused too, so a paused pool keeps a price; `before_swap` and `get_limits` stop it from
    /// trading.
    ///
    /// Fails with `InvalidInput` on terms [`MontraPoolConfig::check_terms`] refuses.
    fn spot_fees(&self, zero_for_one: bool) -> Result<Option<HookSpotFees>, SimulationError> {
        Ok(Some(self.fees(zero_for_one)?))
    }

    /// The largest input that settles in the direction, capped at `type(int128).max`. Fails with
    /// `InvalidInput` on terms [`MontraPoolConfig::check_terms`] refuses.
    ///
    /// Both bounds take the pool's own swap steps, with each step's input and LP fee rounded up
    /// as v4's `SwapMath` rounds them:
    /// - a buy may take the price to the far end of the seed, so the pool can swap the input `W`
    ///   that walks there, and the limit is the largest gross input whose net of the hook's fee is
    ///   at most `W`;
    /// - a sell may take the price back to the opening and no further, so the limit is the input
    ///   `W` that walks there, plus the input v4 then swaps at the opening price without moving it,
    ///   which pays nothing out. That extra is none when no liquidity continues past the opening,
    ///   and grows with the liquidity past it, whether or not a swap fee is charged. With no fee
    ///   and liquidity `L` past a `currency0`-quoted opening it is `floor((L - 1) / 2^96)`.
    ///
    /// The output at a bound is the walk's, since input absorbed without moving the price pays
    /// nothing out. Where the swapper's share of that output is 0, `after_swap` refuses every
    /// swap up to the bound, and the limit is 0.
    ///
    /// The walk visits the ticks whose net liquidity is non-zero, while v4's tick bitmap keeps
    /// every tick whose `liquidityGross` is non-zero, net 0 included, and a v4 swap step ends at
    /// each. Only a pool with liquidity outside its seed can have such a tick. For each one the
    /// path crosses, the chain takes one more step than the walk, so the limit can come out up
    /// to 2 raw input units below what the chain settles (one from the step's input rounded up,
    /// one from its LP fee rounded up), and a quote about 1 raw output unit above it (the step's
    /// output rounded down).
    fn max_amount_in(
        &self,
        state: &UniswapV4State,
        zero_for_one: bool,
    ) -> Result<Option<U256>, SimulationError> {
        let fees = self.fees(zero_for_one)?;
        let lp_fee_override = fees.lp_fee_override;
        let bound_tick = if self.is_buy(zero_for_one) {
            self.config.seed_extreme_tick()
        } else {
            self.config.opening_tick()
        };
        let target = swap_bound_sqrt_price(bound_tick, zero_for_one)?;
        let walk = state.amount_in_to_price(zero_for_one, target, lp_fee_override)?;
        if self
            .split_output(zero_for_one, walk.amount_out)?
            .swapper_out
            .is_zero()
        {
            return Ok(Some(U256::ZERO));
        }
        let limit = if self.is_buy(zero_for_one) {
            max_gross_buy(walk.amount_in, fees.hook_fee_pips)?
        } else {
            let swap_fee = state.swap_fee_pips(zero_for_one, lp_fee_override)?;
            let absorbed = input_absorbed_at_price(target, walk.liquidity, zero_for_one, swap_fee)?;
            walk.amount_in
                .checked_add(absorbed)
                .ok_or_else(|| {
                    SimulationError::FatalError(format!(
                        "montra hook {}: sell limit overflows",
                        self.address
                    ))
                })?
        };
        Ok(Some(limit.min(INT128_MAX)))
    }

    /// Applies the updated `montra_*` attributes and the pause flag (set, or deleted to
    /// unpause, read by [`decode_paused`]), and ignores every other attribute. Terms the hook
    /// would refuse apply; quoting refuses them.
    ///
    /// Fails, leaving the handler unchanged, on a `montra_*` value of the wrong width and on a
    /// deleted `montra_*` attribute, which the indexer never deletes.
    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(deleted) = POOL_CONFIG_ATTRIBUTES
            .iter()
            .find(|name| {
                delta
                    .deleted_attributes
                    .contains(**name)
            })
        {
            return Err(TransitionError::DecodeError(format!(
                "montra hook {}: {deleted} was deleted, so the pool's terms are unknown",
                self.address
            )));
        }
        let config = self
            .config
            .with_updates(&delta.updated_attributes)
            .map_err(|e| TransitionError::DecodeError(e.to_string()))?;
        let paused = match delta
            .updated_attributes
            .get(PAUSED_ATTRIBUTE)
        {
            Some(value) => decode_paused(value),
            None if delta
                .deleted_attributes
                .contains(PAUSED_ATTRIBUTE) =>
            {
                false
            }
            None => self.paused,
        };
        self.config = config;
        self.paused = paused;
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn HookHandler> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_equal(&self, other: &dyn HookHandler) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}
