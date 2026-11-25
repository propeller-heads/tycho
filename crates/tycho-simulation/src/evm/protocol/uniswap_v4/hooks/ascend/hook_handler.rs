use std::{any::Any, collections::HashMap};

use alloy::primitives::{address, Address, I128, U256};
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
    math::{
        add, fill_price, mul_div_ceil, mul_div_floor, observe, refuse, size_fill, sub_invariant,
        Fill, FillRequest, PoolPrice, WINDOW_SECONDS,
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
        },
        state::UniswapV4State,
    },
    utils::uniswap::FEE_PIPS_DENOMINATOR,
};

/// The Ascend launchpad hook on Elysium (chain id 99901), deployed at block 423647 and not behind
/// a proxy. The low 14 bits of the address, `0x28ec`, grant `beforeSwap` and `afterSwap` with
/// both return-delta flags among the swap callbacks.
pub const ASCEND_HOOK_ELYSIUM: Address = address!("e0def4221b9840a610d4e7d55812a79a6431a8ec");

// Gas of the hook's own frames: `beforeSwap` or `afterSwap` with the library calls, the token
// calls and the PoolManager calls under it. Each constant was measured with `debug_traceCall` on
// Elysium, one path at a time, as follows. A caller put in place by a state override runs the
// PoolManager's `unlock`, and its callback replays the `PoolManager.swap` call of a real
// transaction at the block before that transaction (with that block's timestamp as a block
// override). The PoolManager, the tokens of the swap and the quote token's proxy implementation
// are in the access list, so they are warm as in a route through the Tycho router, whose balance
// read of the output token warms the last two before the hook runs. The pool's own slots are
// cold. The call tracer's `gasUsed` of the hook's two frames is the value. The replay of the buy
// gives the transaction's own frames exactly. A sell's real `beforeSwap` frame is 5,000 above
// its constant when the quote token and its implementation are first touched there (two cold
// accounts), as in the traced Universal Router transactions. The hook and its library cannot
// change. Re-measure when the shared Uniswap v4 gas model or `pool_manager_call_overhead`
// changes.

/// A buy's `beforeSwap`: the library call, two PoolManager reads, the observation and the mint of
/// the fee. Measured on transaction
/// 0xe607911ed6009338af2013bf355bad1284185a9bc37626801de1d1914a49bb1f, replayed at block 8365604.
const ASCEND_BUY_BEFORE_SWAP_GAS: u64 = 50_203;
/// A buy's `afterSwap`, which takes nothing. Same replay as [`ASCEND_BUY_BEFORE_SWAP_GAS`].
const ASCEND_BUY_AFTER_SWAP_GAS: u64 = 5_402;
/// A sell's `beforeSwap` when the fill is not even considered (the pool has no bid inventory, or
/// its price moved since the last observation): the observation alone. Measured by replaying
/// transaction 0xfc0a9737236a2a953f81bdd369a29b3df83897a86fd74b4510fdc1bf6a9af921 at block 8365305
/// with the pool's bid inventory set to 0 through a storage override.
const ASCEND_SELL_BEFORE_SWAP_GAS: u64 = 17_388;
/// A sell's `beforeSwap` when the fill is considered and the library returns none (a clamped fill
/// price, a spent window, a bid of 1): the observation, a zero-value transfer of the quote token
/// to the hook and the library call. Measured by replaying transaction
/// 0xfc0a9737236a2a953f81bdd369a29b3df83897a86fd74b4510fdc1bf6a9af921 at block 8365305, whose
/// pool held a bid of 1: the exit that runs latest in the library, so this is the upper case of
/// the no-fill exits. Those that decline earlier (a spot outside the band, a fill price at the
/// curve end) cost less and were not measured separately.
const ASCEND_SELL_CONSIDERED_BEFORE_SWAP_GAS: u64 = 47_094;
/// A sell's `beforeSwap` with a fill: the considered path plus both token balance reads, the
/// `take` and the transfers to the treasury, the `settle` and the mint of the fill fee. The larger
/// of the replays of transaction 0xd2cce086b4b757570059c981735f534fcbe85a1dc28d06935400989a2d2f7e5d
/// at block 8351071 (this value) and of transaction
/// 0x22de467d6505e32c4f9554fb8afa7e47d3d5dbbeb3472f9529b46b5bdeb20773 at block 8350325 (154,718).
/// The pools hold different tokens, whose transfers cost differently.
const ASCEND_FILL_BEFORE_SWAP_GAS: u64 = 171_820;
/// A sell's `afterSwap` without a fill: the mint of the fee on the pool's output. Measured by
/// replaying the transaction of [`ASCEND_SELL_CONSIDERED_BEFORE_SWAP_GAS`], with the same value
/// when the bid inventory is set to 0. It costs more than the mint after a fill
/// ([`ASCEND_FILL_AFTER_SWAP_GAS`]), whose claim slot the fill's own mint has written.
const ASCEND_SELL_AFTER_SWAP_GAS: u64 = 20_319;
/// A sell's `afterSwap` after a fill that left the pool an output, whose mint finds the hook's
/// claim slot already written by the fill's own. The larger of the two values traced on 897
/// partial fills to block 10810289 (10,416 and 10,475; 10,475 on transaction 0xff4dc2c1…).
const ASCEND_FILL_AFTER_SWAP_GAS: u64 = 10_475;
/// A sell's `afterSwap` after a fill that took the whole input: the pool's output is 0, so the
/// hook mints no fee. The larger of the two values traced on all 44 such fills to block 10810289
/// (5,669 and 5,728; 5,728 on transaction 0xfdb7885b…).
const ASCEND_WHOLE_INPUT_FILL_AFTER_SWAP_GAS: u64 = 5_728;
/// The route around a swap whose whole input the fill takes, which leaves the pool leg a no-op:
/// the modelled route of a partial fill (`base` 150,000, one bitmap lookup 3,500 and
/// `settlement` 30,000 of the Uniswap v4 costs, the 0 ticks every traced partial fill crossed),
/// less 16,600, by which the gas outside the hook frames of the 44 whole-input fills to block
/// 10810289 is below that of the 897 partial fills on each of the 8 routers that carried both,
/// less the 3,000 of the no-op swap `UniswapV4State::swap` charges. Transaction
/// 0x8af6cf71b4222e56d26e344759bba226738856de7e5c38690e97520436f995bd (block 8523475) used
/// 330,972 in all, against 347,926 modelled; a partial fill on the same router and pool,
/// 0xd2cce086b4b757570059c981735f534fcbe85a1dc28d06935400989a2d2f7e5d, used 362,866.
const ASCEND_WHOLE_INPUT_ROUTE_GAS: u64 = 164_000;
/// Gas the PoolManager spends per hook call outside the hook's own frames: half of what the swap
/// frame holds beyond both hook frames and the median of the hookless swap frames (28,932, the
/// lower median of the 46 hookless `swap` frames among the first 200 transactions with a `Swap`
/// event in blocks 8363000 to 8366000). On swaps traced to block 10810289 the half is about 1,700
/// for sells, 2,100 for fills and 590 for buys; one value serves all paths, so buys are charged
/// about 2,200 more than they use. Quotes assume an external sender and no hook data.
const ASCEND_HOOK_DISPATCH_GAS: u64 = 1_689;

// A swap in the same second as the pool's last observation (`dt == 0`) runs no observation, and a
// second swap of the pool in one transaction also finds the hook's slots warm and its claim slot
// written. Medians and upper values of the frames traced on Elysium to block 10810289, with the
// quote token cold as on the Universal Router, so the declined sell is shown less the documented
// 5,000 of two cold accounts.

/// A buy's `beforeSwap` in the second of the pool's last observation: the median of 85 single-swap
/// transactions (39,340 to 59,134; e.g.
/// 0x591d6fa3c89e08645035d0019f8ee88ce054fcea4628e2047cc81abfa2738296).
const ASCEND_BUY_SAME_SECOND_BEFORE_SWAP_GAS: u64 = 41_436;
/// A sell's `beforeSwap` in the second of the pool's last observation when the fill is not
/// considered: the larger of the two values on 40 transactions (8,994 and 8,995; e.g.
/// 0x7546fcd5e4665c949148e6c793f37bcfaf71bc2fdeeea458315bb38c989106eb).
const ASCEND_SELL_SAME_SECOND_BEFORE_SWAP_GAS: u64 = 8_995;
/// A sell's `beforeSwap` in the second of the pool's last observation when the fill is considered
/// and declined: the median of 52 transactions, 28,171 (9,170 to 44,294; e.g.
/// 0x6d802ee39057c338d099afc8d99763bb7c7e792f61d1bba0edcfea9afb64b9cb), less 5,000 for the quote
/// token warm.
const ASCEND_SELL_CONSIDERED_SAME_SECOND_BEFORE_SWAP_GAS: u64 = 23_171;
/// A buy's `beforeSwap` when the same pool was already swapped earlier in the route: the median
/// of 112 repeat swaps (20,387 to 50,286; e.g. the third swap of
/// 0xa61dd1b79b714835686c9bb7cfd7b68cf4e672ebe68a048cfa14773d20a82004).
const ASCEND_BUY_REPEAT_BEFORE_SWAP_GAS: u64 = 21_743;
/// A sell's `beforeSwap` when the same pool was already swapped earlier in the route and the fill
/// is not considered: the upper value of 6 repeat swaps (2,994 to 8,994).
const ASCEND_SELL_REPEAT_BEFORE_SWAP_GAS: u64 = 8_994;
/// A sell's `beforeSwap` when the same pool was already swapped earlier in the route and the fill
/// is considered and declined: the median of 8 repeat swaps (9,170 to 45,043; e.g. the second
/// swap of 0x31143a15b807265700ee53497dafbe48aa0a91768f2d75a1cfee7aa92a14363f).
const ASCEND_SELL_CONSIDERED_REPEAT_BEFORE_SWAP_GAS: u64 = 9_171;
/// A sell's `afterSwap` without a fill when the same pool was already swapped earlier in the
/// route, whose mint finds the claim slot written: the median of 14 repeat sells (10,660 to
/// 20,260).
const ASCEND_SELL_REPEAT_AFTER_SWAP_GAS: u64 = 10_660;

/// Native model of the Ascend launchpad hook for one pool.
///
/// Every swap pays the pool's constant fee `f` in the quote asset, the LP fee is 0:
/// - a buy (input = quote) pays `ceil(A * f / 1e6)` out of its input in `beforeSwap`, and the pool
///   swaps the rest;
/// - a sell may be partly filled from the pool's bid in `beforeSwap`: the fill pays `quoteOut -
///   floor(quoteOut * f / 1e6)`, and the pool swaps the rest of the input; then `afterSwap` takes
///   `ceil(out * f / 1e6)` from the pool's output.
///
/// Every swap that reaches the hook first runs the observation, which writes the EMA, the
/// discount, the observed tick and the timestamp (and a fill writes the bid and the discount).
/// [`HookHandler::handler_after_swap`] carries those writes into the state a quote returns, so a
/// later swap of the same route reads them.
///
/// The timestamp a quote uses is the decoder's estimate of the next block's. Elysium makes blocks
/// about every 100 ms with whole-second timestamps, so the next block often carries the
/// timestamp of the latest one, and a quote is then one second ahead of the block the swap lands
/// in: its EMA has moved one second further and its discount decayed one second more. A swap
/// that lands in the second of the pool's last observation (`dt == 0`) on a pool whose price
/// moved since then does not fill, which a quote one second later may.
///
/// Only a sell on a pool with bid inventory has an amount that depends on `block.timestamp`
/// (through the EMA, the decay of the discount and the 600 s window); [`HookHandler::apply_block`]
/// moves the handler to the block a quote is for. The timestamp still matters to every swap in
/// two ways: the observation refuses a timestamp before the pool's last observation, and it
/// stores the timestamp in the state the quote returns.
///
/// The hook refuses what its contract reverts on or what no router settles, with `InvalidInput`:
///
/// - a zero input;
/// - an exact-output swap. The hook prices these with a different fee split (seen by tracing
///   V4Quoter `quoteExactOutputSingle` at block 8365800), but `ProtocolSim::get_amount_out` quotes
///   exact-input only;
/// - an input above `type(int128).max`;
/// - a swap the pool cannot fill in full (`check_swap_outcome`): the router would leave a credit
///   and the PoolManager's `unlock` reverts;
/// - a swap whose swapper output is 0.
///
/// Two checks of the hook are not modelled. Each depends on state that no attribute carries, and
/// neither binds on a route that settles its input first:
///
/// - the hook's quote balance and the PoolManager's token balance against a fill. They decline only
///   on a strict shortfall, which the hook's accounting rules out.
/// - the zero-value `transfer(hook, 0)` of the quote token that a fill needs to succeed. The hook's
///   owner configures the quote tokens, and a token that reverted it would have no fills at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AscendHookHandler {
    address: Address,
    pub(super) state: AscendPoolState,
    /// Timestamp of the block quotes are for: the decoded block's at first, then the one
    /// `apply_block` last moved to.
    pub(super) block_timestamp: u64,
    /// Whether this handler is the one `handler_after_swap` left after a swap of the pool, so the
    /// next swap is a repeat swap of the pool in the same route. `apply_block` and
    /// `delta_transition` clear it.
    follows_swap_in_route: bool,
}

/// When a swap runs relative to the pool's last observation, which decides the gas of its
/// frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Timing {
    /// Time has passed since the last observation, which the swap writes.
    Elapsed,
    /// The swap is in the second of the last observation, which it leaves as it is.
    SameSecond,
    /// The pool was already swapped earlier in the route, in the same second.
    Repeat,
}

/// The path a swap takes through the hook, which decides its deltas and its gas.
#[derive(Debug, Clone, Copy)]
enum SwapPath {
    /// A swap of the quote asset into the pool.
    Buy,
    /// A sell the hook does not consider filling: the pool has no bid inventory, or its price
    /// moved since the last observation.
    SellNotConsidered,
    /// A sell whose fill the hook considered (it probes the quote token and calls the library),
    /// and the library returned none.
    SellDeclined,
    /// A sell partly filled from the bid.
    SellFilled(Fill),
}

/// What the hook does to one swap.
#[derive(Debug, Clone, Copy)]
struct SwapPlan {
    path: SwapPath,
    timing: Timing,
    /// The hook's observation after the swap.
    observation: Observation,
    /// The hook's bid after the swap.
    bid: Bid,
}

impl SwapPlan {
    /// The tokens the fill takes from the swapper.
    fn fill_tokens(&self) -> U256 {
        match self.path {
            SwapPath::SellFilled(fill) => fill.tokens_in,
            SwapPath::Buy | SwapPath::SellNotConsidered | SwapPath::SellDeclined => U256::ZERO,
        }
    }

    /// Whether the fill takes the whole input of a swap of `amount_in`, leaving the pool leg a
    /// no-op.
    fn fills_whole_input(&self, amount_in: U256) -> bool {
        matches!(self.path, SwapPath::SellFilled(fill) if fill.tokens_in == amount_in)
    }

    /// The gas of the hook's `beforeSwap` frame on this path for a swap of `amount_in`, with the
    /// route around the swap when the fill takes the whole input. A fill is charged as traced
    /// whatever its timing: no fill in the second of an observation was told apart.
    fn before_swap_gas(&self, amount_in: U256) -> u64 {
        match (self.path, self.timing) {
            (SwapPath::Buy, Timing::Elapsed) => ASCEND_BUY_BEFORE_SWAP_GAS,
            (SwapPath::Buy, Timing::SameSecond) => ASCEND_BUY_SAME_SECOND_BEFORE_SWAP_GAS,
            (SwapPath::Buy, Timing::Repeat) => ASCEND_BUY_REPEAT_BEFORE_SWAP_GAS,
            (SwapPath::SellNotConsidered, Timing::Elapsed) => ASCEND_SELL_BEFORE_SWAP_GAS,
            (SwapPath::SellNotConsidered, Timing::SameSecond) => {
                ASCEND_SELL_SAME_SECOND_BEFORE_SWAP_GAS
            }
            (SwapPath::SellNotConsidered, Timing::Repeat) => ASCEND_SELL_REPEAT_BEFORE_SWAP_GAS,
            (SwapPath::SellDeclined, Timing::Elapsed) => ASCEND_SELL_CONSIDERED_BEFORE_SWAP_GAS,
            (SwapPath::SellDeclined, Timing::SameSecond) => {
                ASCEND_SELL_CONSIDERED_SAME_SECOND_BEFORE_SWAP_GAS
            }
            (SwapPath::SellDeclined, Timing::Repeat) => {
                ASCEND_SELL_CONSIDERED_REPEAT_BEFORE_SWAP_GAS
            }
            (SwapPath::SellFilled(_), _) if self.fills_whole_input(amount_in) => {
                ASCEND_FILL_BEFORE_SWAP_GAS + ASCEND_WHOLE_INPUT_ROUTE_GAS
            }
            (SwapPath::SellFilled(_), _) => ASCEND_FILL_BEFORE_SWAP_GAS,
        }
    }

    /// The gas of the hook's `afterSwap` frame on this path for a swap of `amount_in`.
    fn after_swap_gas(&self, amount_in: U256) -> u64 {
        match (self.path, self.timing) {
            (SwapPath::Buy, _) => ASCEND_BUY_AFTER_SWAP_GAS,
            (SwapPath::SellNotConsidered | SwapPath::SellDeclined, Timing::Repeat) => {
                ASCEND_SELL_REPEAT_AFTER_SWAP_GAS
            }
            (SwapPath::SellNotConsidered | SwapPath::SellDeclined, _) => ASCEND_SELL_AFTER_SWAP_GAS,
            (SwapPath::SellFilled(_), _) if self.fills_whole_input(amount_in) => {
                ASCEND_WHOLE_INPUT_FILL_AFTER_SWAP_GAS
            }
            (SwapPath::SellFilled(_), _) => ASCEND_FILL_AFTER_SWAP_GAS,
        }
    }
}

/// What `fill` pays the swapper: its quote less the fill fee.
fn fill_net(fill: &Fill, fee_pips: u32) -> Result<U256, SimulationError> {
    // The fee is a share below 1e6 of `quote_out`.
    sub_invariant(fill.quote_out, fee_floor(fill.quote_out, fee_pips)?, "the fill's net")
}

/// `floor(amount * fee_pips / 1e6)`.
pub(super) fn fee_floor(amount: U256, fee_pips: u32) -> Result<U256, SimulationError> {
    mul_div_floor(amount, U256::from(fee_pips), U256::from(FEE_PIPS_DENOMINATOR))
}

/// `ceil(amount * fee_pips / 1e6)`.
pub(super) fn fee_ceil(amount: U256, fee_pips: u32) -> Result<U256, SimulationError> {
    mul_div_ceil(amount, U256::from(fee_pips), U256::from(FEE_PIPS_DENOMINATOR))
}

/// `value` as an `int128` delta; `what` names the quantity. Fails with a `FatalError`: every
/// caller passes a quantity that a `BalanceDelta` amount or `INT128_MAX` bounds, so a value that
/// does not fit is a fault of the model, not of the swap.
fn to_i128(value: U256, what: &str) -> Result<I128, SimulationError> {
    u128::try_from(value)
        .ok()
        .and_then(|v| I128::try_from(v).ok())
        .ok_or_else(|| {
            SimulationError::FatalError(format!("ascend hook: {what} {value} does not fit int128"))
        })
}

/// The largest gross buy input `A` whose net `A - ceil(A * f / 1e6)` is at most `max_net`:
/// `net = floor(A * (1e6 - f) / 1e6)`, which is at most `max_net` exactly when
/// `A * (1e6 - f) < (max_net + 1) * 1e6`.
pub(super) fn max_gross_buy(max_net: U256, fee_pips: u32) -> Result<U256, SimulationError> {
    let kept = U256::from(FEE_PIPS_DENOMINATOR - fee_pips);
    let bound = add(max_net, U256::from(1), "the buy limit")?
        .checked_mul(U256::from(FEE_PIPS_DENOMINATOR))
        .ok_or_else(|| refuse("the buy limit overflows"))?;
    Ok((bound - U256::from(1)) / kept)
}

/// The pool's price before the swap, as `get_amount_out` passes it to every callback.
fn price_of(context: &StateContext) -> PoolPrice {
    PoolPrice { sqrt_price: context.sqrt_price_x96, tick: context.tick }
}

impl AscendHookHandler {
    pub fn new(address: Address, state: AscendPoolState, block_timestamp: u64) -> Self {
        Self { address, state, block_timestamp, follows_swap_in_route: false }
    }

    /// When a swap at the handler's block timestamp runs relative to the pool's last observation.
    fn timing(&self) -> Timing {
        if self.follows_swap_in_route {
            Timing::Repeat
        } else if self.state.observation.updated_at == self.block_timestamp {
            Timing::SameSecond
        } else {
            Timing::Elapsed
        }
    }

    /// Whether a swap in the direction given by `zero_for_one` sells the quote asset into the
    /// pool, which is what the hook calls a buy.
    fn is_buy(&self, zero_for_one: bool) -> bool {
        zero_for_one != self.state.token_is_currency0()
    }

    /// Whether every swap refuses: the observation every swap runs first refuses a block
    /// timestamp before the pool's last observation, which a delta can carry ahead of the block
    /// the handler is at.
    fn refuses_every_swap(&self) -> bool {
        self.state
            .observation
            .is_ahead_of(self.block_timestamp)
    }

    /// The validated input of a swap and what the hook does to it, from the pool's price and tick
    /// before the swap. The trait carries nothing between its callbacks, so each one plans again.
    fn plan_swap(
        &self,
        context: &StateContext,
        swap_params: &SwapParams,
    ) -> Result<(U256, SwapPlan), SimulationError> {
        let amount_in = exact_input(swap_params.amount_specified).map_err(refuse)?;
        let plan = self.plan(price_of(context), swap_params.zero_for_one, amount_in)?;
        Ok((amount_in, plan))
    }

    /// What the hook does to a swap of `amount_in` in the direction `zero_for_one`, from a pool
    /// at `price`, at the handler's block timestamp.
    fn plan(
        &self,
        price: PoolPrice,
        zero_for_one: bool,
        amount_in: U256,
    ) -> Result<SwapPlan, SimulationError> {
        let curve = &self.state.curve;
        let now = self.block_timestamp;
        let PoolPrice { sqrt_price: spot, tick } = price;
        let mut observation = observe(curve, &self.state.observation, price, now)?;
        let is_buy = self.is_buy(zero_for_one);
        let mut bid = self.state.bid;
        let path = if is_buy {
            SwapPath::Buy
        } else if bid.inventory.is_zero() || observation.tick != tick {
            // The hook considers a fill only for a sell with inventory, the price unmoved since
            // the last observation.
            SwapPath::SellNotConsidered
        } else {
            let window_reset = bid
                .window_start
                .saturating_add(WINDOW_SECONDS) <=
                now;
            let filled = if window_reset { U256::ZERO } else { bid.filled };
            let x = fill_price(curve, observation.ema, observation.discount)?;
            let fill = size_fill(
                curve,
                &FillRequest {
                    spot,
                    fill_price: x,
                    ema: observation.ema,
                    amount_in,
                    inventory: bid.inventory,
                    max_clip: U256::from(bid.max_clip),
                    filled,
                },
            )?;
            match fill {
                Some(fill) => {
                    observation.discount = fill.discount;
                    // `size_fill` caps a fill's quote at the inventory.
                    bid.inventory =
                        sub_invariant(bid.inventory, fill.quote_out, "the bid inventory")?;
                    bid.filled = add(filled, fill.quote_out, "the bid's filled amount")?;
                    if window_reset {
                        bid.window_start = now;
                    }
                    SwapPath::SellFilled(fill)
                }
                None => SwapPath::SellDeclined,
            }
        };
        Ok(SwapPlan { path, timing: self.timing(), observation, bid })
    }

    /// What `afterSwap` takes from a swap whose pool output is `amm_out`, and what the swapper
    /// is left with.
    fn settle_output(
        &self,
        plan: &SwapPlan,
        amm_out: U256,
    ) -> Result<SettledOutput, SimulationError> {
        let fee_pips = self.state.fee_pips;
        let filled_net = match plan.path {
            SwapPath::Buy => {
                return Ok(SettledOutput { fee: U256::ZERO, swapper_output: amm_out });
            }
            SwapPath::SellFilled(fill) => fill_net(&fill, fee_pips)?,
            SwapPath::SellNotConsidered | SwapPath::SellDeclined => U256::ZERO,
        };
        let fee = fee_ceil(amm_out, fee_pips)?;
        let amm_net = sub_invariant(amm_out, fee, "the pool's output net of the fee")?;
        let swapper_output = add(filled_net, amm_net, "the swapper's output")?;
        Ok(SettledOutput { fee, swapper_output })
    }
}

/// What a swap's pool output becomes once `afterSwap` has taken the hook's fee.
#[derive(Debug, Clone, Copy)]
struct SettledOutput {
    /// The fee `afterSwap` takes from the pool's output: 0 for a buy.
    fee: U256,
    /// The swapper's output: the fill's net quote plus the pool's output less the fee.
    swapper_output: U256,
}

impl HookHandler for AscendHookHandler {
    fn address(&self) -> Address {
        self.address
    }

    /// Moves the handler to the block's timestamp. Reports a change when the timestamp moved and
    /// either the pool has bid inventory (only a fill's amount depends on the time) or the move
    /// started or ended the refusal of every swap, which a pool observed after the block's
    /// timestamp makes.
    /// A new block also ends the route a carried handler came from (`handler_after_swap`).
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        self.follows_swap_in_route = false;
        let moved = self.block_timestamp != block.timestamp();
        let was_refusing = self.refuses_every_swap();
        self.block_timestamp = block.timestamp();
        let amounts_depend_on_time = !self.state.bid.inventory.is_zero();
        let refusal_changed = was_refusing != self.refuses_every_swap();
        moved && (amounts_depend_on_time || refusal_changed)
    }

    /// Returns `true`. Every Ascend pool opens one tick spacing outside its single position,
    /// with no active liquidity; [`Self::max_amount_in`] bounds both directions, so limits and
    /// quotes agree there.
    fn swaps_from_empty_range(&self) -> bool {
        true
    }

    /// The hook gas constants are the hook's call frames alone; this is the PoolManager's side.
    /// A repeat swap of the pool in the route finds the PoolManager's slots warm: on the 126
    /// repeat swaps traced to block 10810289 the swap frame holds about 9,500 less than a first
    /// swap's beyond the hook frames, more than the dispatch charge, so it is 0 there.
    fn pool_manager_call_overhead(&self) -> u64 {
        if self.follows_swap_in_route {
            0
        } else {
            ASCEND_HOOK_DISPATCH_GAS
        }
    }

    /// Takes a buy's fee out of its input, or fills part of a sell from the bid.
    ///
    /// Refuses with `InvalidInput` a zero input, an exact-output swap, an input above
    /// `type(int128).max` and a block timestamp before the pool's last observation.
    fn before_swap(
        &self,
        params: BeforeSwapParameters,
        _overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        _transient_storage: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<BeforeSwapOutput>, SimulationError> {
        let (amount_in, plan) = self.plan_swap(&params.context, &params.swap_params)?;

        let delta = match plan.path {
            SwapPath::Buy => {
                let fee = fee_ceil(amount_in, self.state.fee_pips)?;
                BalanceDelta::new(to_i128(fee, "the buy's fee")?, I128::ZERO)
            }
            SwapPath::SellFilled(fill) => {
                // The hook takes the tokens (a positive specified delta) and pays the quote, net
                // of its fee (a negative unspecified delta).
                let net = fill_net(&fill, self.state.fee_pips)?;
                BalanceDelta::new(
                    to_i128(fill.tokens_in, "the fill's tokens")?,
                    -to_i128(net, "the fill's net quote")?,
                )
            }
            SwapPath::SellNotConsidered | SwapPath::SellDeclined => {
                BalanceDelta::new(I128::ZERO, I128::ZERO)
            }
        };
        Ok(WithGasEstimate {
            gas_estimate: plan.before_swap_gas(amount_in),
            result: BeforeSwapOutput {
                amount_delta: BeforeSwapDelta(delta.as_i256()),
                fee: Default::default(),
                overwrites: HashMap::new(),
                transient_storage: HashMap::new(),
            },
        })
    }

    /// Takes a sell's fee `ceil(out * f / 1e6)` from the pool's output; a buy pays nothing more
    /// here. Refuses with `InvalidInput` a swap that leaves the swapper an output of 0, and the
    /// swaps `before_swap` refuses.
    fn after_swap(
        &self,
        params: AfterSwapParameters,
        _overwrites: Option<HashMap<Address, HashMap<U256, U256>>>,
        _transient_storage_params: Option<HashMap<Address, HashMap<U256, U256>>>,
    ) -> Result<WithGasEstimate<AfterSwapDelta>, SimulationError> {
        let (amount_in, plan) = self.plan_swap(&params.context, &params.swap_params)?;
        // An exact-input swap's pool output is the unspecified leg.
        let amm_out = U256::from(
            if params.swap_params.zero_for_one {
                params.delta.amount1()
            } else {
                params.delta.amount0()
            }
            .unsigned_abs(),
        );
        let settled = self.settle_output(&plan, amm_out)?;
        if settled.swapper_output.is_zero() {
            return Err(refuse("the swap's output is 0, which no minimum output accepts"));
        }
        Ok(WithGasEstimate {
            gas_estimate: plan.after_swap_gas(amount_in),
            result: to_i128(settled.fee, "the sell's fee")?,
        })
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
    ///
    /// Refuses with `InvalidInput` while every swap refuses (a pool observed after the block's
    /// timestamp), as the quotes and the limits do.
    fn spot_fees(&self, _zero_for_one: bool) -> Result<Option<HookSpotFees>, SimulationError> {
        if self.refuses_every_swap() {
            return Err(refuse(format!(
                "the block timestamp {} is before the last observation {}",
                self.block_timestamp, self.state.observation.updated_at
            )));
        }
        Ok(Some(HookSpotFees { lp_fee_override: None, hook_fee_pips: self.state.fee_pips }))
    }

    /// The largest input that settles in the direction, capped at `type(int128).max`.
    ///
    /// - A buy: the largest gross input whose net of the fee, `gross - ceil(gross * fee / 1e6)`, is
    ///   at most the walk's input to the far end of the position; past it the pool cannot swap the
    ///   net.
    /// - A sell: the pool takes the walk's input back to the curve end and swaps nothing beyond,
    ///   while a fill takes at most the tokens an unbounded input fills, so the limit is the walk's
    ///   input plus those tokens. It is the walk's input alone where the state admits no fill, or
    ///   where no fill settles within the walk.
    ///
    /// A bound at which the swapper's output is 0 gives 0, as `after_swap` refuses every swap
    /// up to it.
    ///
    /// The limit is hard, though the hook does not revert on the size of an input. Two reads show
    /// it: the hook's library `0xe08461e7` with an input of 1e27 at block 8365604 returns the
    /// capped net, and a V4Quoter sell past the range's capacity succeeds on the PoolManager. So on
    /// chain a larger input settles partially: a buy's net is capped at the pool's capacity to the
    /// curve end, and a sell consumes what the range absorbs. A router settles the whole input,
    /// and the pool manager's `unlock` reverts on the credit the rest leaves
    /// (`check_swap_outcome`), so an input above the limit is refused rather than quoted as a
    /// partial swap.
    ///
    /// A pool whose every swap refuses fails here in both directions with `InvalidInput`, as its
    /// quotes and its spot price do.
    fn max_amount_in(
        &self,
        state: &UniswapV4State,
        zero_for_one: bool,
    ) -> Result<Option<U256>, SimulationError> {
        let curve = &self.state.curve;
        let (sqrt_price, tick) = state.price_and_tick();
        let price = PoolPrice { sqrt_price, tick };
        if self.is_buy(zero_for_one) {
            // The refusal of a quote: the observation every swap runs first.
            observe(curve, &self.state.observation, price, self.block_timestamp)?;
            let target = swap_bound(curve.far_end, zero_for_one);
            let walk = state.amount_in_to_price(zero_for_one, target, None)?;
            if walk.amount_in.is_zero() || walk.amount_out.is_zero() {
                return Ok(Some(U256::ZERO));
            }
            return Ok(Some(max_gross_buy(walk.amount_in, self.state.fee_pips)?.min(INT128_MAX)));
        }
        let target = swap_bound(curve.curve_end, zero_for_one);
        let walk = state.amount_in_to_price(zero_for_one, target, None)?;
        let saturated = self
            .plan(price, zero_for_one, INT128_MAX)?
            .fill_tokens();
        let mut limit = add(walk.amount_in, saturated, "the sell limit")?.min(INT128_MAX);
        let mut plan = self.plan(price, zero_for_one, limit)?;
        if plan.fill_tokens() != saturated {
            // The fill is not saturated at the walk plus `saturated`: the pool would need more
            // than the walk to reach the fill price, so no fill settles within the walk.
            limit = walk.amount_in.min(INT128_MAX);
            plan = self.plan(price, zero_for_one, limit)?;
        }
        if limit.is_zero() ||
            self.settle_output(&plan, walk.amount_out)?
                .swapper_output
                .is_zero()
        {
            return Ok(Some(U256::ZERO));
        }
        Ok(Some(limit))
    }

    /// The hook after the swap: the observation and the bid as the swap leaves them, and a next
    /// swap of the pool in the same route priced as a repeat swap.
    fn handler_after_swap(
        &self,
        context: &StateContext,
        swap_params: &SwapParams,
    ) -> Result<Option<Box<dyn HookHandler>>, SimulationError> {
        let (_, plan) = self.plan_swap(context, swap_params)?;
        let mut next = self.clone();
        next.state.observation = plan.observation;
        next.state.bid = plan.bid;
        next.follows_swap_in_route = true;
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
        self.follows_swap_in_route = false;
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
