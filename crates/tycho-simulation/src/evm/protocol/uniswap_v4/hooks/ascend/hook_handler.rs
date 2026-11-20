use std::{any::Any, collections::HashMap};

use alloy::primitives::{address, Address, I128, I256, U256};
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
    math::{
        fill_price, mul_div_ceil, mul_div_floor, observe, refuse, size_fill, Curve, Fill,
        FillRequest, INT128_MAX, WINDOW_SECONDS,
    },
    AscendPoolState, Bid, Observation, POOL_ATTRIBUTES,
};
use crate::evm::protocol::{
    uniswap_v4::{
        hooks::{
            hook_handler::HookHandler,
            models::{
                AfterSwapDelta, AfterSwapParameters, BalanceDelta, BeforeSwapDelta,
                BeforeSwapOutput, BeforeSwapParameters, HookSpotFees, StateContext, SwapOutcome,
                SwapParams, WithGasEstimate,
            },
            montra::hook_handler::MONTRA_HOOK_DISPATCH_GAS,
        },
        state::UniswapV4State,
    },
    utils::uniswap::{
        tick_math::{MAX_SQRT_RATIO, MIN_SQRT_RATIO},
        FEE_PIPS_DENOMINATOR,
    },
};

/// The Ascend launchpad hook on Elysium (chain id 99901), deployed at block 423647 and not behind
/// a proxy. The low 14 bits of the address, `0x28ec`, grant `beforeSwap` and `afterSwap` with
/// both return-delta flags among the swap callbacks.
pub const ASCEND_HOOK_ELYSIUM: Address = address!("e0def4221b9840a610d4e7d55812a79a6431a8ec");

// Gas of the hook's frames. PROVISIONAL: the integration decision D16 asks for each constant to
// be measured with `debug_traceCall` on Elysium at the golden transactions' parent blocks (the
// hook's own frames only, with the PoolManager and the tokens warm and the pool's slots cold),
// which this build had no node for. These are estimates from the storage the paths touch; they
// are replaced by the measured values when that trace is run.

/// A buy's `beforeSwap`: the library call (two `extsload`s), the observation, `PM.mint` of the
/// fee and the `accruedFees` write.
pub(crate) const ASCEND_BUY_BEFORE_SWAP_GAS: u64 = 90_000;
/// A buy's `afterSwap`, which takes nothing.
pub(crate) const ASCEND_BUY_AFTER_SWAP_GAS: u64 = 5_000;
/// A sell's `beforeSwap` without a fill: the observation and the eligibility checks.
pub(crate) const ASCEND_SELL_BEFORE_SWAP_GAS: u64 = 45_000;
/// A sell's `beforeSwap` with a fill: the library call, the treasury transfers, `take`, `settle`,
/// the mint of the fill fee and the bid's writes.
pub(crate) const ASCEND_FILL_BEFORE_SWAP_GAS: u64 = 250_000;
/// A sell's `afterSwap`: the mint of the fee on the pool's output and the `accruedFees` write.
pub(crate) const ASCEND_SELL_AFTER_SWAP_GAS: u64 = 40_000;

/// Native model of the Ascend launchpad hook for one pool.
///
/// Every swap pays the pool's constant fee `f` in the quote asset, the LP fee is 0:
/// - a buy (input = quote) pays `ceil(A * f / 1e6)` out of its input in `beforeSwap`, and the pool
///   swaps the rest;
/// - a sell may be partly filled from the pool's bid in `beforeSwap` (see `math.rs`): the fill pays
///   `quoteOut - floor(quoteOut * f / 1e6)`, and the pool swaps the rest of the input; then
///   `afterSwap` takes `ceil(out * f / 1e6)` from the pool's output.
///
/// Every swap that reaches the hook first runs the observation, which writes the EMA, the
/// discount, the observed tick and the timestamp (and a fill writes the bid and the discount).
/// [`HookHandler::handler_after_swap`] carries those writes into the state a quote returns, so a
/// later swap of the same route reads them.
///
/// A sell on a pool with bid inventory depends on `block.timestamp` (the EMA, the decay of the
/// discount, the 600 s window), so [`HookHandler::apply_block`] moves the handler to the block a
/// quote is for. The decoder estimates that block's timestamp as the latest header's plus the
/// chain's block time, 1 s on Elysium. Elysium stamps whole seconds about every 100 ms, so a swap
/// often lands in a block with the latest timestamp, where `dt == 0`: a pool whose price moved
/// since its last observation then rejects the fill, which the quote at +1 s may include. Buys
/// and sells without inventory do not depend on the timestamp.
///
/// The hook refuses what its contract reverts on or what no router settles, with `InvalidInput`:
/// zero input, exact-output swaps, an input above `type(int128).max`, a swap the pool cannot
/// fill in full (`check_swap_outcome`: the router would leave a credit and the PoolManager's
/// `unlock` reverts) and a swap whose swapper output is 0.
///
/// The balance guards of a fill (the hook's quote balance and the PoolManager's token balance
/// against the fill) are not modelled: they decline only on a strict shortfall, which the hook's
/// accounting (balance = summed inventories) rules out on a route whose input is settled first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AscendHookHandler {
    address: Address,
    state: AscendPoolState,
    /// Timestamp of the block quotes are for: the decoded block's at first, then the one
    /// `apply_block` last moved to.
    block_timestamp: u64,
}

/// The pool's price and tick before a swap.
#[derive(Debug, Clone, Copy)]
struct PoolView {
    spot: U256,
    tick: i32,
}

/// What the hook does to one swap.
#[derive(Debug, Clone, Copy)]
struct SwapPlan {
    is_buy: bool,
    /// The hook's observation after the swap.
    observation: Observation,
    /// The hook's bid after the swap.
    bid: Bid,
    /// The fill a sell gets, if any.
    fill: Option<Fill>,
}

impl SwapPlan {
    /// What the fill pays the swapper: its quote less the fill fee.
    fn fill_net(&self, fee_pips: u32) -> Result<U256, SimulationError> {
        match self.fill {
            None => Ok(U256::ZERO),
            Some(fill) => Ok(fill.quote_out - fee_floor(fill.quote_out, fee_pips)?),
        }
    }
}

/// `floor(amount * fee_pips / 1e6)`.
pub(super) fn fee_floor(amount: U256, fee_pips: u32) -> Result<U256, SimulationError> {
    mul_div_floor(amount, U256::from(fee_pips), U256::from(FEE_PIPS_DENOMINATOR))
}

/// `ceil(amount * fee_pips / 1e6)`.
pub(super) fn fee_ceil(amount: U256, fee_pips: u32) -> Result<U256, SimulationError> {
    mul_div_ceil(amount, U256::from(fee_pips), U256::from(FEE_PIPS_DENOMINATOR))
}

fn to_i128(value: U256) -> Result<I128, SimulationError> {
    u128::try_from(value)
        .ok()
        .and_then(|v| I128::try_from(v).ok())
        .ok_or_else(|| {
            SimulationError::FatalError(format!("ascend hook: {value} does not fit int128"))
        })
}

/// The largest gross buy input `A` whose net `A - ceil(A * f / 1e6)` is at most `max_net`:
/// `net = floor(A * (1e6 - f) / 1e6)`, which is at most `max_net` exactly when
/// `A * (1e6 - f) < (max_net + 1) * 1e6`.
pub(super) fn max_gross_buy(max_net: U256, fee_pips: u32) -> Result<U256, SimulationError> {
    let kept = U256::from(FEE_PIPS_DENOMINATOR - fee_pips);
    let bound = (max_net + U256::from(1))
        .checked_mul(U256::from(FEE_PIPS_DENOMINATOR))
        .ok_or_else(|| refuse("the buy limit overflows"))?;
    Ok((bound - U256::from(1)) / kept)
}

/// The swap's price bound for a walk: v4's usable extreme next to `price`.
fn swap_bound(price: U256, zero_for_one: bool) -> U256 {
    if zero_for_one {
        price.max(MIN_SQRT_RATIO + U256::from(1))
    } else {
        price.min(MAX_SQRT_RATIO - U256::from(1))
    }
}

impl AscendHookHandler {
    pub fn new(address: Address, state: AscendPoolState, block_timestamp: u64) -> Self {
        Self { address, state, block_timestamp }
    }

    /// The pool's hook state.
    #[cfg(test)]
    pub(super) fn pool_state(&self) -> &AscendPoolState {
        &self.state
    }

    /// The timestamp of the block quotes are for.
    #[cfg(test)]
    pub(super) fn block_timestamp(&self) -> u64 {
        self.block_timestamp
    }

    fn curve(&self) -> Result<Curve, SimulationError> {
        Curve::new(self.state.token_is_currency0, self.state.start_tick)
    }

    /// Whether a swap in the direction given by `zero_for_one` sells the quote asset into the
    /// pool, which is what the hook calls a buy.
    fn is_buy(&self, zero_for_one: bool) -> bool {
        zero_for_one != self.state.token_is_currency0
    }

    /// Validates `amount_specified` as the hook's contract and the pool manager would: the
    /// exact input in raw units.
    fn exact_input(amount_specified: I256) -> Result<U256, SimulationError> {
        if amount_specified == I256::ZERO {
            return Err(refuse("a swap of zero reverts SwapAmountCannotBeZero"));
        }
        if amount_specified > I256::ZERO {
            return Err(refuse("exact-output swaps are not quoted"));
        }
        let amount_in = amount_specified.unsigned_abs();
        if amount_in > INT128_MAX {
            return Err(refuse("an input above type(int128).max overflows the swap delta"));
        }
        Ok(amount_in)
    }

    /// What the hook does to a swap of `amount_in` in the direction `zero_for_one`, from a pool
    /// at `view`, at the handler's block timestamp.
    fn plan(
        &self,
        curve: &Curve,
        view: PoolView,
        zero_for_one: bool,
        amount_in: U256,
    ) -> Result<SwapPlan, SimulationError> {
        let now = self.block_timestamp;
        let mut observation = observe(curve, &self.state.observation, view.spot, view.tick, now)?;
        let is_buy = self.is_buy(zero_for_one);
        let mut bid = self.state.bid;
        let mut fill = None;
        // Fill eligibility (S15): a sell with inventory, the price unmoved since the last
        // observation.
        if !is_buy && !bid.inventory.is_zero() && observation.tick == view.tick {
            let window_reset = bid
                .window_start
                .saturating_add(WINDOW_SECONDS) <=
                now;
            let filled = if window_reset { U256::ZERO } else { bid.filled };
            let x = fill_price(curve, observation.ema, observation.discount)?;
            fill = size_fill(
                curve,
                &FillRequest {
                    spot: view.spot,
                    fill_price: x,
                    ema: observation.ema,
                    amount_in,
                    inventory: bid.inventory,
                    max_clip: U256::from(bid.max_clip),
                    filled,
                },
            )?;
            if let Some(fill) = fill {
                observation.discount = fill.discount;
                bid.inventory = bid
                    .inventory
                    .checked_sub(fill.quote_out)
                    .ok_or_else(|| refuse("a fill larger than the inventory"))?;
                bid.filled = filled + fill.quote_out;
                if window_reset {
                    bid.window_start = now;
                }
            }
        }
        Ok(SwapPlan { is_buy, observation, bid, fill })
    }

    fn view(context: &StateContext) -> PoolView {
        PoolView { spot: context.sqrt_price_x96, tick: context.tick }
    }

    /// The swapper's output of a swap whose pool output is `amm_out`.
    fn swapper_output(&self, plan: &SwapPlan, amm_out: U256) -> Result<U256, SimulationError> {
        let fee = self.state.fee_pips;
        if plan.is_buy {
            return Ok(amm_out);
        }
        Ok(plan.fill_net(fee)? + amm_out - fee_ceil(amm_out, fee)?)
    }
}

impl HookHandler for AscendHookHandler {
    fn address(&self) -> Address {
        self.address
    }

    /// Moves the handler to the block's timestamp. Reports a change when the pool has bid
    /// inventory and the timestamp moved, since only a fill depends on the time.
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let moved = self.block_timestamp != block.timestamp();
        self.block_timestamp = block.timestamp();
        moved && !self.state.bid.inventory.is_zero()
    }

    /// Returns `true`. Every Ascend pool opens one tick spacing outside its single position,
    /// with no active liquidity; [`Self::max_amount_in`] bounds both directions, so limits and
    /// quotes agree there.
    fn swaps_from_empty_range(&self) -> bool {
        true
    }

    /// The hook gas constants are the hook's call frames alone; this is the PoolManager's side,
    /// which Montra's measurement of the same `Hooks.sol` path covers.
    fn pool_manager_call_overhead(&self) -> u64 {
        MONTRA_HOOK_DISPATCH_GAS
    }

    /// Takes a buy's fee out of its input, or fills part of a sell from the bid.
    ///
    /// Refuses with `InvalidInput` a zero input, an exact-output swap and an input above
    /// `type(int128).max`.
    fn before_swap(
        &self,
        params: BeforeSwapParameters,
        _overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        _transient_storage: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<BeforeSwapOutput>, SimulationError> {
        let zero_for_one = params.swap_params.zero_for_one;
        let amount_in = Self::exact_input(params.swap_params.amount_specified)?;
        let curve = self.curve()?;
        let plan = self.plan(&curve, Self::view(&params.context), zero_for_one, amount_in)?;

        let (delta, gas_estimate) = if plan.is_buy {
            let fee = fee_ceil(amount_in, self.state.fee_pips)?;
            (BalanceDelta::new(to_i128(fee)?, I128::ZERO), ASCEND_BUY_BEFORE_SWAP_GAS)
        } else if let Some(fill) = plan.fill {
            // The hook takes the tokens (a positive specified delta) and pays the quote, net of
            // its fee (a negative unspecified delta).
            let net = plan.fill_net(self.state.fee_pips)?;
            (
                BalanceDelta::new(to_i128(fill.tokens_in)?, -to_i128(net)?),
                ASCEND_FILL_BEFORE_SWAP_GAS,
            )
        } else {
            (BalanceDelta::new(I128::ZERO, I128::ZERO), ASCEND_SELL_BEFORE_SWAP_GAS)
        };
        Ok(WithGasEstimate {
            gas_estimate,
            result: BeforeSwapOutput {
                amount_delta: BeforeSwapDelta(delta.as_i256()),
                fee: Default::default(),
                overwrites: HashMap::new(),
                transient_storage: HashMap::new(),
            },
        })
    }

    /// Takes a sell's fee `ceil(out * f / 1e6)` from the pool's output; a buy pays nothing more
    /// here. Refuses with `InvalidInput` a swap that leaves the swapper an output of 0.
    fn after_swap(
        &self,
        params: AfterSwapParameters,
        _overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        _transient_storage_params: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<AfterSwapDelta>, SimulationError> {
        let zero_for_one = params.swap_params.zero_for_one;
        let amount_in = Self::exact_input(params.swap_params.amount_specified)?;
        let curve = self.curve()?;
        let plan = self.plan(&curve, Self::view(&params.context), zero_for_one, amount_in)?;
        // An exact-input swap's pool output is the unspecified leg.
        let amm_out = U256::from(
            if zero_for_one { params.delta.amount1() } else { params.delta.amount0() }
                .unsigned_abs(),
        );
        if self
            .swapper_output(&plan, amm_out)?
            .is_zero()
        {
            return Err(refuse("the swap's output is 0, which no minimum output accepts"));
        }
        if plan.is_buy {
            return Ok(WithGasEstimate {
                gas_estimate: ASCEND_BUY_AFTER_SWAP_GAS,
                result: I128::ZERO,
            });
        }
        let fee = fee_ceil(amm_out, self.state.fee_pips)?;
        Ok(WithGasEstimate { gas_estimate: ASCEND_SELL_AFTER_SWAP_GAS, result: to_i128(fee)? })
    }

    /// Refuses a swap the pool did not fill in full: the router settles the whole input, and
    /// the pool manager's `unlock` reverts on the credit the unconsumed part leaves.
    fn check_swap_outcome(
        &self,
        _zero_for_one: bool,
        outcome: &SwapOutcome,
    ) -> Result<(), SimulationError> {
        if outcome.amount_unfilled != U256::ZERO {
            return Err(refuse(format!(
                "the pool could not swap {} of the input, and unlock reverts CurrencyNotSettled \
                 on the credit",
                outcome.amount_unfilled
            )));
        }
        Ok(())
    }

    /// The hook's fee as a fraction of the amount it is charged on.
    fn fee(&self, _context: &UniswapV4State, _params: SwapParams) -> Result<f64, SimulationError> {
        Ok(f64::from(self.state.fee_pips) / f64::from(FEE_PIPS_DENOMINATOR))
    }

    /// The pool's constant fee in both directions, with no LP fee override. A marginal sell
    /// fills at no other price (a marginal fill is no fill), so the AMM price marked up by the fee
    /// is the margin.
    fn spot_fees(&self, _zero_for_one: bool) -> Result<Option<HookSpotFees>, SimulationError> {
        Ok(Some(HookSpotFees { lp_fee_override: None, hook_fee_pips: self.state.fee_pips }))
    }

    /// The largest input that settles in the direction, capped at `type(int128).max`.
    ///
    /// - A buy: the largest gross `A` whose net `A - ceil(A * f / 1e6)` is at most the walk input
    ///   `W` to the far end of the position (`max_gross_buy`); past it the pool cannot swap the
    ///   net.
    /// - A sell: the pool takes the walk input `W_sell` back to the curve end and swaps nothing
    ///   beyond, while a fill takes at most `E_sat` tokens (what an unbounded input fills), so the
    ///   limit is `W_sell + E_sat`. It is `W_sell` where the state admits no fill.
    ///
    /// A bound at which the swapper's output is 0 gives 0, as `after_swap` refuses every swap
    /// up to it.
    fn max_amount_in(
        &self,
        state: &UniswapV4State,
        zero_for_one: bool,
    ) -> Result<Option<U256>, SimulationError> {
        let curve = self.curve()?;
        let (spot, tick, _liquidity) = state.price_tick_liquidity();
        let view = PoolView { spot, tick };
        let fee_pips = self.state.fee_pips;
        if self.is_buy(zero_for_one) {
            let target = swap_bound(curve.far_end, zero_for_one);
            let walk = state.amount_in_to_price(zero_for_one, target, None)?;
            if walk.amount_in.is_zero() || walk.amount_out.is_zero() {
                return Ok(Some(U256::ZERO));
            }
            return Ok(Some(max_gross_buy(walk.amount_in, fee_pips)?.min(INT128_MAX)));
        }
        let target = swap_bound(curve.curve_end, zero_for_one);
        let walk = state.amount_in_to_price(zero_for_one, target, None)?;
        let fill_tokens = |amount: U256| -> Result<(U256, U256), SimulationError> {
            let plan = self.plan(&curve, view, zero_for_one, amount)?;
            Ok((
                plan.fill
                    .map_or(U256::ZERO, |f| f.tokens_in),
                plan.fill_net(fee_pips)?,
            ))
        };
        let (saturated, _) = fill_tokens(INT128_MAX)?;
        let mut limit = walk.amount_in + saturated;
        let (fill_in, mut fill_net) = fill_tokens(limit.min(INT128_MAX))?;
        if fill_in != saturated {
            // The fill is not saturated at `W + E_sat`: the pool would need more than the walk to
            // reach the fill price, so no fill settles within the walk.
            limit = walk.amount_in;
            fill_net = fill_tokens(limit.min(INT128_MAX))?.1;
        }
        let amm_out = walk.amount_out;
        let swapper_out = fill_net + amm_out - fee_ceil(amm_out, fee_pips)?;
        if limit.is_zero() || swapper_out.is_zero() {
            return Ok(Some(U256::ZERO));
        }
        Ok(Some(limit.min(INT128_MAX)))
    }

    /// The hook after the swap: the observation and the bid as the swap leaves them.
    fn handler_after_swap(
        &self,
        context: &StateContext,
        swap_params: &SwapParams,
    ) -> Result<Option<Box<dyn HookHandler>>, SimulationError> {
        let amount_in = Self::exact_input(swap_params.amount_specified)?;
        let curve = self.curve()?;
        let plan = self.plan(&curve, Self::view(context), swap_params.zero_for_one, amount_in)?;
        let mut next = self.clone();
        next.state.observation = plan.observation;
        next.state.bid = plan.bid;
        Ok(Some(Box::new(next)))
    }

    /// Applies the updated `ascend_*` attributes and ignores every other one. Fails, leaving the
    /// handler unchanged, on a value of the wrong width or one the hook could not have stored,
    /// and on a deleted `ascend_*` attribute, which the indexer never deletes.
    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(deleted) = POOL_ATTRIBUTES.iter().find(|name| {
            delta
                .deleted_attributes
                .contains(**name)
        }) {
            return Err(TransitionError::DecodeError(format!(
                "ascend hook {}: {deleted} was deleted, so the pool's terms are unknown",
                self.address
            )));
        }
        self.state = self
            .state
            .with_updates(&delta.updated_attributes)
            .map_err(|e| TransitionError::DecodeError(e.to_string()))?;
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
