//! `PendleState` — one `ProtocolSim` covering both component types.
//!
//! Tycho binds a single state type per protocol system, but the Substreams package emits two kinds
//! of component: `pendle_market` (the SY↔PT AMM) and `pendle_sy` (the ERC-5115 wrapper). So the
//! state is an enum that dispatches on `protocol_type_name` at decode time and forwards every
//! `ProtocolSim` method.
//!
//! The six edges the brief asks for split across the two:
//!
//! | Edge | Component | Shape |
//! |---|---|---|
//! | PT → SY | market | exact, closed form |
//! | SY → PT | market | inverted by the router's search |
//! | YT → SY | market | exact, via the flash-swap identity |
//! | SY → YT | market | inverted by the router's search |
//! | underlying → SY | sy | constant rate, per token class |
//! | SY → underlying | sy | constant rate, per token class |
//!
//! The YT legs are flash-swaps against the **same reserves** as the PT legs. They are quoted on the
//! market component, and `get_limits` reports each direction's own bound rather than sharing one.

use std::{any::Any, collections::HashMap, fmt::Debug};

use alloy::primitives::{I256, U256};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use super::{
    clock,
    math::{
        approx,
        errors::PendleError,
        market::{self, MarketState},
        pmath, sy_utils,
    },
};
use crate::evm::protocol::u256_num::{biguint_to_u256, u256_to_biguint, u256_to_f64};

/// Gas for a swap routed through Router V4.
///
/// Measured per edge on a mainnet fork at block 25 800 000 against the wstETH market, each edge in
/// its own test on a fresh fork, then rounded up by roughly a tenth. The headroom is deliberate and
/// asymmetric in its consequences: under-estimating makes a router pick a route that costs more
/// than it was quoted, while over-estimating only makes Pendle look slightly worse than it is.
///
/// The two directions of a leg are **not** one number. Buying PT costs 40% more than selling it,
/// and buying YT 27% more than selling it, because the exact-SY-in directions run the router's
/// approximation loop on chain. Charging the dearer figure to both would misprice the cheaper
/// direction badly enough to change routing decisions.
///
/// | Edge | Measured | Charged |
/// |---|---|---|
/// | SY to PT | 299,736 | 330,000 |
/// | PT to SY | 214,123 | 235,000 |
/// | SY to YT | 385,413 | 425,000 |
/// | YT to SY | 304,325 | 335,000 |
/// | wrap, `one_to_one` | 80,206 / 77,603 | 90,000 |
const GAS_SY_FOR_PT: u64 = 330_000;
const GAS_PT_FOR_SY: u64 = 235_000;
const GAS_SY_FOR_YT: u64 = 425_000;
const GAS_YT_FOR_SY: u64 = 335_000;

/// A `one_to_one` wrap is the cheapest case there is: the SY takes the token and mints, and does
/// nothing else.
const GAS_WRAP_ONE_TO_ONE: u64 = 90_000;

/// An `index_rate` wrap reads through to whatever protocol the SY wraps — an ERC-4626 vault, a
/// staking contract — and costs whatever that costs. **No `index_rate` SY has been measured**, so
/// this keeps the original conservative figure rather than extrapolating from the wrapper case.
/// The class is known at quote time, so the two are charged separately instead of averaging a
/// measured number with a guess.
const GAS_WRAP_INDEX_RATE: u64 = 180_000;

/// How an SY converts a given token, as classified at component creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenClass {
    /// One token unit is one SY unit, modulo decimals. A real wrapper.
    OneToOne,
    /// Converted at the SY's exchange rate, like an ERC-4626 share.
    IndexRate,
}

impl TokenClass {
    pub fn parse(raw: &[u8]) -> Option<Self> {
        match raw {
            b"one_to_one" => Some(TokenClass::OneToOne),
            b"index_rate" => Some(TokenClass::IndexRate),
            _ => None,
        }
    }
}

/// The AMM. Everything here comes from attributes the Substreams package emits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendleMarketState {
    pub market: MarketState,
    /// `max(pyIndexStored, SY.exchangeRate())` as of [`Self::rate_sampled_at`].
    pub py_index: U256,
    /// The block timestamp at which `py_index` was read. Also the clock a quote runs on: it is
    /// the one moment for which the rate and the curve describe the same state.
    pub rate_sampled_at: u64,
    /// The chain head as the stream last reported it, injected by the decoder rather than emitted
    /// by the Substreams package. Only ever compared against `rate_sampled_at`, to tell whether
    /// this state still answers for the current block.
    pub head_timestamp: u64,
    pub sy_address: Bytes,
    pub pt_address: Bytes,
    pub yt_address: Bytes,
}

/// The ERC-5115 wrapper, and the conversions it supports per token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendleSyState {
    pub sy_address: Bytes,
    /// Accounting-asset units per SY, as of `rate_sampled_at`.
    pub exchange_rate: U256,
    /// The block timestamp at which `exchange_rate` was read.
    pub rate_sampled_at: u64,
    /// The chain head as the stream last reported it. See [`PendleMarketState::head_timestamp`].
    pub head_timestamp: u64,
    pub sy_decimals: u32,
    pub asset_decimals: u32,
    /// Entry tokens and how each converts. A token absent here is one the indexer could not
    /// classify, and is not quotable.
    pub tokens_in: HashMap<Bytes, TokenClass>,
    /// Exit tokens and how each converts.
    pub tokens_out: HashMap<Bytes, TokenClass>,
    /// What the SY actually holds, per token, as of the indexed block. Redeeming cannot pay out
    /// more than this; depositing is not bounded by it, since the SY mints new shares.
    pub token_balances: HashMap<Bytes, U256>,
    /// This component's id, used to find its own balances in a delta.
    pub component_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendleState {
    Market(PendleMarketState),
    Sy(PendleSyState),
}

impl PendleMarketState {
    /// Which of the market's three tokens an address names.
    fn role(&self, token: &Bytes) -> Option<MarketToken> {
        if *token == self.sy_address {
            Some(MarketToken::Sy)
        } else if *token == self.pt_address {
            Some(MarketToken::Pt)
        } else if *token == self.yt_address {
            Some(MarketToken::Yt)
        } else {
            None
        }
    }

    fn expired(&self) -> bool {
        market::is_expired(self.market.expiry, self.quote_time())
    }

    fn guard_live(&self) -> Result<(), SimulationError> {
        if self.expired() {
            return Err(PendleError::MarketExpired {
                expiry: self.market.expiry,
                block_time: self.quote_time(),
            }
            .into());
        }
        Ok(())
    }

    /// PT in → SY out. Exact: the market has this primitive.
    fn pt_for_sy(&self, pt_in: U256) -> Result<U256, PendleError> {
        let trade = market::execute_trade(
            &self.market,
            self.py_index,
            -pmath::to_i256(pt_in)?,
            self.quote_time(),
        )?;
        pmath::to_u256(trade.net_sy_to_account)
    }

    /// YT in → SY out. A flash-swap: borrow PT, redeem PT+YT to SY, repay the market.
    ///
    /// Redeeming `n` YT needs `n` PT, so the borrowed leg is exactly the YT amount. What the
    /// trader keeps is the redemption proceeds less what repaying the borrow costs.
    fn yt_for_sy(&self, yt_in: U256) -> Result<U256, PendleError> {
        let comp = market::get_market_pre_compute(&self.market, self.py_index, self.quote_time())?;
        let trade = market::calc_trade(&self.market, &comp, self.py_index, pmath::to_i256(yt_in)?)?;
        // Buying the PT back costs this much SY.
        let sy_to_repay = pmath::to_u256(-trade.net_sy_to_account)?;
        // Redeeming PT+YT returns the asset value of the position, converted at the index.
        let redeemed = sy_utils::asset_to_sy(self.py_index, yt_in)?;
        if redeemed < sy_to_repay {
            return Err(PendleError::NegativeResult {
                a: redeemed.to_string(),
                b: sy_to_repay.to_string(),
            });
        }
        Ok(redeemed - sy_to_repay)
    }

    fn amount_out(
        &self,
        amount_in: U256,
        from: MarketToken,
        to: MarketToken,
    ) -> Result<(U256, u64), PendleError> {
        match (from, to) {
            (MarketToken::Pt, MarketToken::Sy) => Ok((self.pt_for_sy(amount_in)?, GAS_PT_FOR_SY)),
            (MarketToken::Yt, MarketToken::Sy) => Ok((self.yt_for_sy(amount_in)?, GAS_YT_FOR_SY)),
            (MarketToken::Sy, MarketToken::Pt) => Ok((
                approx::approx_swap_exact_sy_for_pt(
                    &self.market,
                    self.py_index,
                    amount_in,
                    self.quote_time(),
                )?
                .amount_out,
                GAS_SY_FOR_PT,
            )),
            (MarketToken::Sy, MarketToken::Yt) => Ok((
                approx::approx_swap_exact_sy_for_yt(
                    &self.market,
                    self.py_index,
                    amount_in,
                    self.quote_time(),
                )?
                .amount_out,
                GAS_SY_FOR_YT,
            )),
            _ => unreachable!("unsupported pair is rejected before reaching here"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarketToken {
    Sy,
    Pt,
    Yt,
}

impl PendleSyState {
    fn class(&self, token_in: &Bytes, token_out: &Bytes) -> Option<(TokenClass, Direction)> {
        if *token_in == self.sy_address {
            return self
                .tokens_out
                .get(token_out)
                .map(|class| (*class, Direction::Redeem));
        }
        if *token_out == self.sy_address {
            return self
                .tokens_in
                .get(token_in)
                .map(|class| (*class, Direction::Deposit));
        }
        None
    }

    /// Underlying → SY, or SY → underlying, at the class's conversion.
    ///
    /// `OneToOne` still rescales for decimals: one wstETH is one SY-wstETH, but only because both
    /// carry 18. A 6-decimal token against an 18-decimal SY is one-to-one in *units*, not in wei.
    fn convert(
        &self,
        amount: U256,
        class: TokenClass,
        direction: Direction,
        token_decimals: u32,
    ) -> Result<U256, PendleError> {
        match (class, direction) {
            (TokenClass::OneToOne, Direction::Deposit) => {
                rescale(amount, token_decimals, self.sy_decimals)
            }
            (TokenClass::OneToOne, Direction::Redeem) => {
                rescale(amount, self.sy_decimals, token_decimals)
            }
            (TokenClass::IndexRate, Direction::Deposit) => {
                let as_asset = rescale(amount, token_decimals, self.asset_decimals)?;
                sy_utils::asset_to_sy(self.exchange_rate, as_asset)
            }
            (TokenClass::IndexRate, Direction::Redeem) => {
                let as_asset = sy_utils::sy_to_asset(self.exchange_rate, amount)?;
                rescale(as_asset, self.asset_decimals, token_decimals)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Deposit,
    Redeem,
}

fn rescale(amount: U256, from_decimals: u32, to_decimals: u32) -> Result<U256, PendleError> {
    if from_decimals == to_decimals {
        return Ok(amount);
    }
    if from_decimals < to_decimals {
        let factor = U256::from(10)
            .checked_pow(U256::from(to_decimals - from_decimals))
            .ok_or(PendleError::Overflow { operation: "rescale" })?;
        amount
            .checked_mul(factor)
            .ok_or(PendleError::Overflow { operation: "rescale" })
    } else {
        let factor = U256::from(10)
            .checked_pow(U256::from(from_decimals - to_decimals))
            .ok_or(PendleError::Overflow { operation: "rescale" })?;
        Ok(amount / factor)
    }
}

#[typetag::serde]
impl ProtocolSim for PendleState {
    /// The market's fee, as a ratio. It is a rate-space multiplier on chain, so this converts it
    /// to the amount-space number the trait documents, and it decays to zero at expiry.
    fn fee(&self) -> f64 {
        match self {
            PendleState::Market(state) => {
                let Ok(comp) = market::get_market_pre_compute(
                    &state.market,
                    state.py_index,
                    state.quote_time(),
                ) else {
                    return 0.0;
                };
                let excess = comp.fee_rate - pmath::i_one();
                let Ok(excess) = pmath::to_u256(excess) else { return 0.0 };
                u256_to_f64(excess).unwrap_or(0.0) / 1e18
            }
            // A wrapper takes no fee of its own for the classes quoted here.
            PendleState::Sy(_) => 0.0,
        }
    }

    /// The amount of `quote` needed to buy one unit of `base`.
    ///
    /// Quoted by **spending `quote` to buy `base`**, which is what the trait asks for. That
    /// direction matters here beyond arithmetic: an SY's entry and exit token lists are not the
    /// same set. SY-wstETH accepts WETH as a deposit but will not redeem to it, so pricing SY in
    /// WETH is answerable only from the buying side — asking what selling SY yields in WETH has no
    /// answer at all.
    ///
    /// Priced off a real trade rather than a closed form, so the number agrees with what a caller
    /// would actually get, fee included. One whole unit of `quote` is small against every live
    /// market's depth.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let one_quote = BigUint::from(10u32).pow(quote.decimals);
        let bought = self.get_amount_out(one_quote, quote, base)?;
        let base_out = biguint_to_f64(&bought.amount) / 10f64.powi(base.decimals as i32);
        if base_out == 0.0 {
            return Err(SimulationError::FatalError(format!(
                "one unit of {} buys no {}, so there is no spot price",
                quote.address, base.address
            )));
        }
        // One unit of quote bought `base_out` base, so one base costs `1 / base_out` quote.
        Ok(1.0 / base_out)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        if amount_in == BigUint::from(0u32) {
            return Err(SimulationError::InvalidInput("amount_in is zero".to_string(), None));
        }
        let amount = biguint_to_u256(&amount_in);

        let (out, gas) = match self {
            PendleState::Market(state) => {
                state.guard_live()?;
                state.guard_current()?;
                let from = state
                    .role(&token_in.address)
                    .ok_or_else(|| unknown_pair(token_in, token_out))?;
                let to = state
                    .role(&token_out.address)
                    .ok_or_else(|| unknown_pair(token_in, token_out))?;
                // PT↔YT never touches the market: minting and redeeming PY is the YT's business,
                // and SY is one side of every market edge.
                if from == to || (from != MarketToken::Sy && to != MarketToken::Sy) {
                    return Err(unknown_pair(token_in, token_out));
                }
                state.amount_out(amount, from, to)?
            }
            PendleState::Sy(state) => {
                state.guard_current()?;
                let (class, direction) = state
                    .class(&token_in.address, &token_out.address)
                    .ok_or_else(|| unquotable_pair(token_in, token_out))?;
                let token_decimals = match direction {
                    Direction::Deposit => token_in.decimals,
                    Direction::Redeem => token_out.decimals,
                };
                let gas = match class {
                    TokenClass::OneToOne => GAS_WRAP_ONE_TO_ONE,
                    TokenClass::IndexRate => GAS_WRAP_INDEX_RATE,
                };
                (state.convert(amount, class, direction, token_decimals)?, gas)
            }
        };

        Ok(GetAmountOutResult {
            amount: u256_to_biguint(out),
            gas: BigUint::from(gas),
            new_state: self.clone_box(),
        })
    }

    /// The largest trade that will not revert, per direction.
    ///
    /// Each market direction has its own binding constraint and they do not follow from one
    /// another: SY→PT is bounded by the rate floor, PT→SY by the 96% proportion cap, and the YT
    /// legs by the same cap mapped through the flash-swap identity. The SY→PT and SY→YT bounds are
    /// obtained by inverting the router's own search bound, so the reported input is one the search
    /// actually fills.
    ///
    /// A pair this component does not trade — PT against YT, or a token that is not one of its
    /// three — reports **zero depth rather than an error**. A caller enumerating the pairs of a
    /// component is asking a question, and "none" is the answer; only an actual swap request is
    /// wrong enough to fail. The integration test enumerates every pair, PT↔YT included.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        match self {
            PendleState::Market(state) => {
                if state.expired() {
                    // Dead, not merely empty. Zero depth in both directions.
                    return Ok(no_depth());
                }
                // A bound this state cannot quote against is not a bound. Reported as no depth
                // rather than an error, so a router skips the market the way it skips an expired
                // one instead of treating a routine gap as a failure.
                if !state.is_current() {
                    return Ok(no_depth());
                }
                let (Some(from), Some(to)) = (state.role(&sell_token), state.role(&buy_token))
                else {
                    return Ok(no_depth());
                };

                let (max_in, max_out) = match (from, to) {
                    (MarketToken::Sy, MarketToken::Pt) => {
                        approx::max_sy_in_for_pt(&state.market, state.py_index, state.quote_time())?
                    }
                    (MarketToken::Sy, MarketToken::Yt) => {
                        approx::max_sy_in_for_yt(&state.market, state.py_index, state.quote_time())?
                    }
                    // Selling PT hands PT *to* the market, so the 96% proportion cap binds.
                    (MarketToken::Pt, MarketToken::Sy) => {
                        let max_pt_in = state.max_pt_in()?;
                        (max_pt_in, state.pt_for_sy(max_pt_in)?)
                    }
                    // Selling YT flash-*borrows* PT from the market to redeem against, so the
                    // market sends PT out and the rate floor binds — the same bound as SY→PT, not
                    // the proportion cap. The two YT legs therefore have different constraints
                    // from each other, despite sharing the PT legs' reserves.
                    (MarketToken::Yt, MarketToken::Sy) => {
                        let max_yt_in = state.max_pt_out()?;
                        (max_yt_in, state.yt_for_sy(max_yt_in)?)
                    }
                    // PT against YT, or either against itself: not an edge of this market.
                    _ => return Ok(no_depth()),
                };
                Ok((u256_to_biguint(max_in), u256_to_biguint(max_out)))
            }
            PendleState::Sy(state) => {
                let Some((class, direction)) = state.class(&sell_token, &buy_token) else {
                    return Ok(no_depth());
                };
                // Redeeming is capped by what the SY holds of the token being paid out, so that
                // bound lives on the output side and is converted back into an input amount.
                // Depositing has no reserve behind it at all.
                match direction {
                    Direction::Deposit => {
                        let max_in = unbounded_soft_limit();
                        let max_out = state.convert(max_in, class, direction, state.sy_decimals)?;
                        Ok((u256_to_biguint(max_in), u256_to_biguint(max_out)))
                    }
                    Direction::Redeem => {
                        let max_out = state.held(&buy_token);
                        let max_in = state.sy_for_exit(max_out, class, state.sy_decimals)?;
                        Ok((u256_to_biguint(max_in), u256_to_biguint(max_out)))
                    }
                }
            }
        }
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        match self {
            PendleState::Market(state) => state.apply(&delta),
            PendleState::Sy(state) => {
                // The SY's redeem depth is its own holdings, so balance updates matter as much as
                // attribute ones here.
                if let Some(held) = balances
                    .component_balances
                    .get(&state.component_id)
                {
                    for (token, balance) in held {
                        state
                            .token_balances
                            .insert(token.clone(), U256::from_be_slice(balance));
                    }
                }
                state.apply(&delta)
            }
        }
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other
            .as_any()
            .downcast_ref::<PendleState>()
            .is_some_and(|other| self == other)
    }

    fn query_pool_swap(
        &self,
        params: &tycho_common::simulation::protocol_sim::QueryPoolSwapParams,
    ) -> Result<tycho_common::simulation::protocol_sim::PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }
}

impl PendleMarketState {
    /// The timestamp this market's quotes are evaluated at: the block its rate was read at.
    ///
    /// Not the chain head. Evaluating the curve at the head while holding a rate from an earlier
    /// block would pair two different moments — see [`super::clock`].
    pub fn quote_time(&self) -> u64 {
        self.rate_sampled_at
    }

    /// Whether this state still answers for the current block.
    pub fn is_current(&self) -> bool {
        clock::is_current(self.rate_sampled_at, self.head_timestamp)
    }

    fn guard_current(&self) -> Result<(), SimulationError> {
        if !self.is_current() {
            return Err(SimulationError::RecoverableError(format!(
                "Pendle market rate was read at {} but the chain head is {}; only a quote at the \
                 sampled block is exact",
                self.rate_sampled_at, self.head_timestamp
            )));
        }
        Ok(())
    }

    /// The most PT the market will absorb, from the 96% proportion cap.
    fn max_pt_in(&self) -> Result<U256, PendleError> {
        let comp = market::get_market_pre_compute(&self.market, self.py_index, self.quote_time())?;
        approx::calc_soft_max_pt_in(&self.market, &comp)
    }

    /// The most PT the market will send out, from the exchange-rate floor.
    fn max_pt_out(&self) -> Result<U256, PendleError> {
        let comp = market::get_market_pre_compute(&self.market, self.py_index, self.quote_time())?;
        approx::calc_max_pt_out(&comp, self.market.total_pt)
    }
}

impl PendleSyState {
    /// Whether this wrapper still answers for the current block.
    pub fn is_current(&self) -> bool {
        clock::is_current(self.rate_sampled_at, self.head_timestamp)
    }

    fn guard_current(&self) -> Result<(), SimulationError> {
        if !self.is_current() {
            return Err(SimulationError::RecoverableError(format!(
                "Pendle SY rate was read at {} but the chain head is {}; only a quote at the \
                 sampled block is exact",
                self.rate_sampled_at, self.head_timestamp
            )));
        }
        Ok(())
    }

    /// What the SY holds of `token`, which is all it can pay out when redeeming.
    ///
    /// An unknown balance falls back to the soft limit rather than to zero: reporting no depth for
    /// a token the SY genuinely wraps would hide a real edge, whereas an optimistic bound only
    /// costs a failed quote at the top of the range.
    fn held(&self, token: &Bytes) -> U256 {
        self.token_balances
            .get(token)
            .copied()
            .unwrap_or_else(unbounded_soft_limit)
    }

    /// How much SY it takes to redeem `exit_amount` of an exit token — the inverse of `convert`.
    fn sy_for_exit(
        &self,
        exit_amount: U256,
        class: TokenClass,
        token_decimals: u32,
    ) -> Result<U256, PendleError> {
        match class {
            TokenClass::OneToOne => rescale(exit_amount, token_decimals, self.sy_decimals),
            TokenClass::IndexRate => {
                let as_asset = rescale(exit_amount, token_decimals, self.asset_decimals)?;
                sy_utils::asset_to_sy(self.exchange_rate, as_asset)
            }
        }
    }
}

fn biguint_to_f64(value: &BigUint) -> f64 {
    value
        .to_string()
        .parse::<f64>()
        .unwrap_or(f64::INFINITY)
}

fn unknown_pair(token_in: &Token, token_out: &Token) -> SimulationError {
    SimulationError::FatalError(format!(
        "{} -> {} is not an edge of this Pendle component",
        token_in.address, token_out.address
    ))
}

fn unquotable_pair(token_in: &Token, token_out: &Token) -> SimulationError {
    SimulationError::FatalError(format!(
        "{} -> {} is not a quotable conversion for this SY: the indexer could not classify it, so \
         no closed form is known",
        token_in.address, token_out.address
    ))
}

/// The soft limit this repo uses for an edge with no reserve behind it, matching
/// `native_wrapper`. Large enough not to constrain any real trade, small enough that a caller can
/// multiply by it without overflowing.
fn unbounded_soft_limit() -> U256 {
    U256::from(u128::MAX)
}

/// What `get_limits` reports for a pair this component cannot trade.
fn no_depth() -> (BigUint, BigUint) {
    (BigUint::from(0u32), BigUint::from(0u32))
}

/// Reads an attribute as a big-endian unsigned integer.
pub(super) fn attribute_u256(attributes: &HashMap<String, Bytes>, name: &str) -> Option<U256> {
    attributes
        .get(name)
        .map(|raw| U256::from_be_slice(raw))
}

/// Reads an attribute as a big-endian signed integer.
pub(super) fn attribute_i256(attributes: &HashMap<String, Bytes>, name: &str) -> Option<I256> {
    attributes.get(name).map(|raw| {
        let mut padded = [0u8; 32];
        let start = 32 - raw.len().min(32);
        padded[start..].copy_from_slice(&raw[raw.len().saturating_sub(32)..]);
        // Sign-extend, since the indexer writes minimal two's-complement.
        if raw
            .first()
            .is_some_and(|b| b & 0x80 != 0)
        {
            for byte in padded.iter_mut().take(start) {
                *byte = 0xff;
            }
        }
        I256::from_be_bytes(padded)
    })
}

impl PendleMarketState {
    fn apply(&mut self, delta: &ProtocolStateDelta) -> Result<(), TransitionError> {
        let attributes = &delta.updated_attributes;
        if let Some(value) = attribute_i256(attributes, "total_pt") {
            self.market.total_pt = value;
        }
        if let Some(value) = attribute_i256(attributes, "total_sy") {
            self.market.total_sy = value;
        }
        if let Some(value) = attribute_u256(attributes, "last_ln_implied_rate") {
            self.market.last_ln_implied_rate = value;
        }
        if let Some(value) = attribute_u256(attributes, "ln_fee_rate_root") {
            self.market.ln_fee_rate_root = value;
        }
        if let Some(value) = attribute_u256(attributes, "reserve_fee_percent") {
            self.market.reserve_fee_percent = value;
        }
        if let Some(value) = attribute_u256(attributes, "py_index_current") {
            self.py_index = value;
        }
        if let Some(value) = attribute_u256(attributes, "rate_sampled_at") {
            self.rate_sampled_at = value.saturating_to();
        }
        // Injected by the decoder from the block header, not emitted by the Substreams package.
        if let Some(value) = attribute_u256(attributes, "block_timestamp") {
            self.head_timestamp = value.saturating_to();
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;

impl PendleSyState {
    fn apply(&mut self, delta: &ProtocolStateDelta) -> Result<(), TransitionError> {
        let attributes = &delta.updated_attributes;
        if let Some(value) = attribute_u256(attributes, "sy_exchange_rate") {
            self.exchange_rate = value;
        }
        if let Some(value) = attribute_u256(attributes, "rate_sampled_at") {
            self.rate_sampled_at = value.saturating_to();
        }
        if let Some(value) = attribute_u256(attributes, "block_timestamp") {
            self.head_timestamp = value.saturating_to();
        }
        Ok(())
    }
}
