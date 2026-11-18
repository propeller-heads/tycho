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

use super::{decode_paused, MontraPoolConfig, PAUSED_ATTRIBUTE, POOL_CONFIG_ATTRIBUTES};
use crate::evm::protocol::{
    uniswap_v4::{
        hooks::{
            hook_handler::HookHandler,
            models::{
                AfterSwapDelta, AfterSwapParameters, AmountRanges, BeforeSwapDelta,
                BeforeSwapOutput, BeforeSwapParameters, HookSpotFees, SwapOutcome, SwapParams,
                WithGasEstimate,
            },
        },
        state::UniswapV4State,
    },
    utils::{
        solidity_math::{mul_div, truncate_to_u256},
        uniswap::{
            lp_fee::OVERRIDE_FEE_FLAG,
            tick_math::{get_sqrt_ratio_at_tick, MAX_SQRT_RATIO, MIN_SQRT_RATIO},
            FEE_PIPS_DENOMINATOR,
        },
    },
};

/// Montra's `LaunchHook` on Elysium (chain id 99901), deployed at block 588510 and not behind a
/// proxy. The low 14 bits of the address, `0x38ec`, grant `beforeSwap`, `afterSwap` and both
/// return-delta flags among the swap callbacks.
pub const MONTRA_HOOK_ELYSIUM: Address = address!("6b1216a40b65c83e9c9e18a562c446697b8cf8ec");

/// The immutable `lpLineHold()` of [`MONTRA_HOOK_ELYSIUM`]: the seconds the price must hold past
/// the LP line, in total, before the LP fee switches on. 600 at block 6002000.
pub const MONTRA_LP_LINE_HOLD_SECONDS_ELYSIUM: u64 = 600;

/// The largest amount a v4 `BalanceDelta` can carry, `type(int128).max`. A larger input
/// overflows the pool manager's int128 deltas and the swap reverts.
const INT128_MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX >> 1, 0, 0]);

/// Gas of the hook's `beforeSwap` on a buy, the call frame alone: the lower median over the 261
/// buys of `tests/assets/hooks/montra/replay.json` (one window per 500,000 blocks from 588510 to
/// 6521167, each swap the first in its transaction through the hook), read from
/// `debug_traceTransaction` call traces. The range is 10,891 to 56,710; about 8% exceed 30,000,
/// writing an accrual slot from zero (a zero-to-nonzero SSTORE). The swap that switches a pool's
/// LP fee on also calls `updateDynamicLPFee` and costs 33,092 (block 713192), once per pool.
/// Re-measure by re-recording when the hook changes.
pub const MONTRA_BUY_BEFORE_SWAP_GAS: u64 = 22_491;

/// Gas of the hook's `afterSwap` on a buy, measured the same way: range 3,720 to 8,787.
pub const MONTRA_BUY_AFTER_SWAP_GAS: u64 = 5_570;

/// Gas of the hook's `beforeSwap` on a sell, measured the same way over the recording's 132
/// sells: range 1,913 to 3,913.
pub const MONTRA_SELL_BEFORE_SWAP_GAS: u64 = 3_913;

/// Gas of the hook's `afterSwap` on a sell, measured the same way: range 24,533 to 59,069, about
/// 10% above 30,000, writing an accrual slot from zero.
pub const MONTRA_SELL_AFTER_SWAP_GAS: u64 = 24_818;

/// Gas the PoolManager spends per hook call outside the call frames above, an empirical
/// residual that is not broken down further. Half of the difference between the lower
/// median, over the same swaps, of the PoolManager `swap` frame minus both hook frames (32,616)
/// and the lower median `swap` frame of the 58 hookless Elysium v4 swaps in blocks
/// 5990000-5990400 (27,256), as the recording holds them.
pub const MONTRA_HOOK_DISPATCH_GAS: u64 = 2_680;

/// Native model of Montra's `LaunchHook` for one pool, from the integration spec's verified
/// behaviour of the deployed hook (no verified source is published).
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
/// The returned states keep the hook's terms as they were: the stage machine (graduation, the LP
/// line crossing and its hold) advances from the `PoolConfig` write each swap makes, which the
/// indexer publishes, and at a fixed block timestamp no stage change alters a later quote.
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
    lp_fee_pips: u32,
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

    /// The LP fee in force and the hook's fee `h = rate - lpInForce`, both in pips, of a swap in
    /// the direction given by `zero_for_one`.
    ///
    /// Refuses with `InvalidInput` the terms [`MontraPoolConfig::check_terms`] refuses. Terms it
    /// accepts keep `lp <= total <= rate`, so the subtraction cannot fail then; an error names
    /// that broken invariant.
    fn fees_pips(&self, zero_for_one: bool) -> Result<(u32, u32), SimulationError> {
        let terms = self.quoting_terms()?;
        let rate = if self.is_buy(zero_for_one) {
            terms.buy_fee_pips
        } else {
            self.config.total_fee_pips()
        };
        let hook_fee_pips = rate
            .checked_sub(terms.lp_fee_pips)
            .ok_or_else(|| {
                SimulationError::FatalError(format!(
                    "montra hook {}: LP fee {} exceeds the rate {rate}",
                    self.address, terms.lp_fee_pips
                ))
            })?;
        Ok((terms.lp_fee_pips, hook_fee_pips))
    }

    fn refuse(&self, reason: &str) -> SimulationError {
        SimulationError::InvalidInput(format!("montra hook {}: {reason}", self.address), None)
    }

    /// What the hook does to a swap whose pool output is `gross_out`, in the direction given by
    /// `zero_for_one`, at the hook's fee `hook_fee_pips` for that direction
    /// ([`Self::fees_pips`]): the fee it takes from that output (a sell's
    /// `floor(gross_out * h / 1e6)`, nothing on a buy) and what the swapper receives after it.
    ///
    /// Expects a `hook_fee_pips` from terms [`MontraPoolConfig::check_terms`] accepts, which
    /// keep a sell's fee below the output. Fails as [`hook_fee`] does, and with `FatalError` when
    /// the fee exceeds `gross_out`, which only a rate outside those terms causes.
    fn split_output(
        &self,
        zero_for_one: bool,
        gross_out: U256,
        hook_fee_pips: u32,
    ) -> Result<OutputSplit, SimulationError> {
        let fee = if self.is_buy(zero_for_one) {
            U256::ZERO
        } else {
            hook_fee(gross_out, hook_fee_pips)?
        };
        let swapper_out = gross_out
            .checked_sub(fee)
            .ok_or_else(|| {
                SimulationError::FatalError(format!(
                    "montra hook {}: the fee {fee} exceeds the output {gross_out}",
                    self.address
                ))
            })?;
        Ok(OutputSplit { hook_fee: fee, swapper_out })
    }
}

/// A swap's pool output split between the hook's fee and the swapper.
struct OutputSplit {
    /// What the hook takes from the output.
    hook_fee: U256,
    /// What the swapper receives.
    swapper_out: U256,
}

/// The LP fee override a swap runs at, given the LP fee in force: none while the LP fee is
/// off, as the hook's `beforeSwap` returns none then. The pool's own LP fee is then its
/// slot0 fee, 0 for a Montra pool, whose key carries the dynamic-fee flag (which
/// `UniswapV4Fees` maps to 0). Every path that sets a swap's LP fee goes through here.
fn swap_lp_fee_override(lp_fee_pips: u32) -> Option<u32> {
    (lp_fee_pips != 0).then_some(lp_fee_pips)
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
/// The net is `ceil(A * (1e6 - h) / 1e6)`, so the largest such `A` is
/// `floor(max_net * 1e6 / (1e6 - h))`. The result is checked against the forward formula: it
/// fits and one more unit does not.
fn max_gross_buy(max_net: U256, hook_fee_pips: u32) -> Result<U256, SimulationError> {
    let kept_pips = kept_pips(hook_fee_pips)?;
    let gross =
        mul_div(max_net, U256::from(FEE_PIPS_DENOMINATOR), U256::from(kept_pips)).map_err(|e| {
            SimulationError::FatalError(format!(
                "montra hook: the gross buy for a net of {max_net} at {hook_fee_pips} pips: {e}"
            ))
        })?;
    let net = |gross: U256| -> Result<U256, SimulationError> {
        Ok(gross - hook_fee(gross, hook_fee_pips)?)
    };
    let next = gross
        .checked_add(U256::from(1))
        .ok_or_else(|| {
            SimulationError::FatalError(format!("montra hook: buy limit of {max_net} overflows"))
        })?;
    if net(gross)? > max_net || net(next)? <= max_net {
        return Err(SimulationError::FatalError(format!(
            "montra hook: {gross} is not the largest buy whose net fits {max_net} at \
             {hook_fee_pips} pips"
        )));
    }
    Ok(gross)
}

/// The square-root price at `tick`, inside the range a swap's price limit allows: the
/// executor and V4Quoter pass `MIN_SQRT_PRICE + 1` or `MAX_SQRT_PRICE - 1`, which a walk to
/// TickMath's own extreme (at tick spacings 1, 2, 4 and 8) stops at.
fn swap_bound_sqrt_price(tick: i32, zero_for_one: bool) -> Result<U256, SimulationError> {
    let sqrt_price = get_sqrt_ratio_at_tick(tick)?;
    Ok(if zero_for_one {
        sqrt_price.max(MIN_SQRT_RATIO + U256::from(1))
    } else {
        sqrt_price.min(MAX_SQRT_RATIO - U256::from(1))
    })
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
        let (lp_fee, hook_fee_pips) = self.fees_pips(zero_for_one)?;
        if self.paused {
            return Err(self.refuse("the pool is paused"));
        }
        let amount_specified = params.swap_params.amount_specified;
        if amount_specified == I256::ZERO {
            return Err(self.refuse("a swap of zero reverts SwapAmountCannotBeZero"));
        }
        if amount_specified > I256::ZERO {
            return Err(self.refuse("exact-output swaps are not quoted"));
        }
        let amount_in = amount_specified.unsigned_abs();
        if amount_in > INT128_MAX {
            return Err(self.refuse("an input above type(int128).max overflows the swap delta"));
        }

        let (fee, gas_estimate) = if self.is_buy(zero_for_one) {
            (hook_fee(amount_in, hook_fee_pips)?, MONTRA_BUY_BEFORE_SWAP_GAS)
        } else {
            (U256::ZERO, MONTRA_SELL_BEFORE_SWAP_GAS)
        };
        // The fee is at most the input, which fits an int128, and a positive specified delta is
        // the hook's claim on the input. It sits in the upper 128 bits of a `BeforeSwapDelta`.
        let amount_delta = BeforeSwapDelta(I256::from_raw(fee) << 128);

        let fee_override = swap_lp_fee_override(lp_fee)
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
        let (_, hook_fee_pips) = self.fees_pips(zero_for_one)?;
        let split =
            self.split_output(zero_for_one, U256::from(gross_out.unsigned_abs()), hook_fee_pips)?;
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
        let (_, hook_fee_pips) = self.fees_pips(params.zero_for_one)?;
        Ok(f64::from(hook_fee_pips) / f64::from(FEE_PIPS_DENOMINATOR))
    }

    /// Never consulted: [`Self::spot_fees`] always answers.
    fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
        Err(SimulationError::RecoverableError(
            "spot_price is not implemented for MontraHookHandler".into(),
        ))
    }

    /// The LP fee in force and the hook's fee for the direction. Answers while the pool is
    /// paused too, so a paused pool keeps a price; `before_swap` and `get_limits` stop it from
    /// trading.
    ///
    /// Fails with `InvalidInput` on terms [`MontraPoolConfig::check_terms`] refuses.
    fn spot_fees(&self, zero_for_one: bool) -> Result<Option<HookSpotFees>, SimulationError> {
        let (lp_fee, hook_fee_pips) = self.fees_pips(zero_for_one)?;
        Ok(Some(HookSpotFees { lp_fee_override: swap_lp_fee_override(lp_fee), hook_fee_pips }))
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
        let (lp_fee, hook_fee_pips) = self.fees_pips(zero_for_one)?;
        let lp_fee_override = swap_lp_fee_override(lp_fee);
        let bound_tick = if self.is_buy(zero_for_one) {
            self.config.seed_extreme_tick()
        } else {
            self.config.opening_tick()
        };
        let target = swap_bound_sqrt_price(bound_tick, zero_for_one)?;
        let walk = state.amount_in_to_price(zero_for_one, target, lp_fee_override)?;
        if self
            .split_output(zero_for_one, walk.amount_out, hook_fee_pips)?
            .swapper_out
            .is_zero()
        {
            return Ok(Some(U256::ZERO));
        }
        let limit = if self.is_buy(zero_for_one) {
            max_gross_buy(walk.amount_in, hook_fee_pips)?
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

    /// Reports "not implemented": [`Self::max_amount_in`] answers for every direction, so
    /// `get_limits` never asks for amount ranges.
    fn get_amount_ranges(
        &self,
        _token_in: Bytes,
        _token_out: Bytes,
    ) -> Result<AmountRanges, SimulationError> {
        Err(SimulationError::RecoverableError(
            "get_amount_ranges is not implemented for MontraHookHandler".into(),
        ))
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
        if let Some(deleted) = delta
            .deleted_attributes
            .iter()
            .find(|name| POOL_CONFIG_ATTRIBUTES.contains(&name.as_str()))
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
