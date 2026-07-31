use std::{any::Any, collections::HashMap, fmt};

use num_bigint::BigUint;

use crate::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        indicatively_priced::IndicativelyPriced,
        swap::{
            self, LimitsParams, MarginalPriceParams, QuoteParams, SwapQuoter, TransitionParams,
        },
    },
    Bytes,
};

#[derive(Default, Debug, Clone)]
pub struct Balances {
    pub component_balances: HashMap<String, HashMap<Bytes, Bytes>>,
    pub account_balances: HashMap<Bytes, HashMap<Bytes, Bytes>>,
}

/// Represents the result of getting the amount out of a trading pair.
#[derive(Debug, Clone)]
pub struct GetAmountOutResult {
    /// The output amount
    pub amount: BigUint,
    /// The gas cost
    pub gas: BigUint,
    /// The new state after the swap
    pub new_state: Box<dyn ProtocolSim>,
}

impl GetAmountOutResult {
    /// Constructs a new GetAmountOutResult struct with the given amount and gas
    pub fn new(amount: BigUint, gas: BigUint, new_state: Box<dyn ProtocolSim>) -> Self {
        GetAmountOutResult { amount, gas, new_state }
    }

    /// Aggregates the given GetAmountOutResult struct to the current one.
    /// It updates the amount with the other's amount and adds the other's gas to the current one's
    /// gas.
    pub fn aggregate(&mut self, other: &Self) {
        self.amount = other.amount.clone();
        self.gas += &other.gas;
    }
}

impl fmt::Display for GetAmountOutResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "amount = {}, gas = {}", self.amount, self.gas)
    }
}

/// Represents a price as a fraction in the token_in -> token_out direction with units
/// `[token_out/token_in]`.
///
/// A fraction struct is used for price to have flexibility in precision independent of the
/// decimal precisions of the numerator and denominator tokens. This allows for:
/// - Exact price representation without floating-point errors
/// - Handling tokens with different decimal places without loss of precision
///
/// # Example
/// If we want to represent that token A is worth 2.5 units of token B:
///
/// ```
/// use num_bigint::BigUint;
/// use tycho_common::simulation::protocol_sim::Price;
///
/// let numerator = BigUint::from(25u32); // Represents 25 units of token B
/// let denominator = BigUint::from(10u32); // Represents 10 units of token A
/// let price = Price::new(numerator, denominator);
/// ```
///
/// If you want to define a limit price for a trade, where you expect to get at least 120 T1 for
/// 50 T2:
/// ```
/// use num_bigint::BigUint;
/// use tycho_common::simulation::protocol_sim::Price;
///
/// let min_amount_out = BigUint::from(120u32); // The minimum amount of T1 you expect
/// let amount_in = BigUint::from(50u32); // The amount of T2 you are selling
/// let limit_price = Price::new(min_amount_out, amount_in);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Price {
    /// The amount of token_out (what you receive), including token decimals
    pub numerator: BigUint,
    /// The amount of token_in (what you pay), including token decimals
    pub denominator: BigUint,
}

impl Price {
    pub fn new(numerator: BigUint, denominator: BigUint) -> Self {
        if denominator == BigUint::ZERO {
            // Division by zero is not possible
            panic!("Price denominator cannot be zero");
        } else if numerator == BigUint::ZERO {
            // Zero pool price is not valid in our context
            panic!("Price numerator cannot be zero");
        }
        Self { numerator, denominator }
    }
}

/// A point on the AMM price curve.
///
/// Collected during iterative numerical search algorithms.
/// These points can be reused as bounds for subsequent searches, improving convergence speed.
#[derive(Debug, Clone)]
pub struct PricePoint {
    /// The amount of token_in in atomic units (wei).
    pub amount_in: BigUint,
    /// The amount of token_out in atomic units (wei).
    pub amount_out: BigUint,
    /// The price in units of `[token_out/token_in]` scaled by decimals.
    ///
    /// Computed as `(amount_out / 10^token_out_decimals) / (amount_in / 10^token_in_decimals)`.
    pub price: f64,
}

impl PricePoint {
    pub fn new(amount_in: BigUint, amount_out: BigUint, price: f64) -> Self {
        Self { amount_in, amount_out, price }
    }
}

/// Represents a pool swap between two tokens at a given price on a pool.
#[derive(Debug, Clone)]
pub struct PoolSwap {
    /// The amount of token_in sold to the pool
    amount_in: BigUint,
    /// The amount of token_out bought from the pool
    amount_out: BigUint,
    /// The new state of the pool after the swap
    new_state: Box<dyn ProtocolSim>,
    /// Optional price points that the pool was transitioned through while computing this swap.
    /// Useful for providing good bounds for repeated calls.
    price_points: Option<Vec<PricePoint>>,
}

impl PoolSwap {
    pub fn new(
        amount_in: BigUint,
        amount_out: BigUint,
        new_state: Box<dyn ProtocolSim>,
        price_points: Option<Vec<PricePoint>>,
    ) -> Self {
        Self { amount_in, amount_out, new_state, price_points }
    }

    pub fn amount_in(&self) -> &BigUint {
        &self.amount_in
    }

    pub fn amount_out(&self) -> &BigUint {
        &self.amount_out
    }

    pub fn new_state(&self) -> &dyn ProtocolSim {
        self.new_state.as_ref()
    }

    pub fn price_points(&self) -> &Option<Vec<PricePoint>> {
        &self.price_points
    }
}

/// Options on how to constrain the pool swap query.
///
/// All prices use units `[token_out/token_in]` with amounts in atomic units (wei). When selling
/// token_in into a pool, prices decrease due to slippage.
#[derive(Debug, Clone, PartialEq)]
pub enum SwapConstraint {
    /// Calculates the maximum trade while respecting a minimum trade price.
    TradeLimitPrice {
        /// The minimum acceptable trade price. The resulting `amount_out / amount_in >= limit`.
        limit: Price,
        /// Fraction to raise the acceptance threshold above `limit`. Loosens the search criteria
        /// but will never allow violating the trade limit price itself.
        tolerance: f64,
        /// The minimum amount of token_in that must be used for this trade.
        min_amount_in: Option<BigUint>,
        /// The maximum amount of token_in that can be used for this trade.
        max_amount_in: Option<BigUint>,
    },

    /// Calculates the swap required to move the pool's marginal price down to a target.
    ///
    /// # Edge Cases and Limitations
    ///
    /// Computing the exact amount to move a pool's marginal price to a target has several
    /// challenges:
    /// - The definition of marginal price varies between protocols. It is usually not an attribute
    ///   of the pool but a consequence of its liquidity distribution and current state.
    /// - For protocols with concentrated liquidity, the marginal price is discrete, meaning we
    ///   can't always find an exact trade amount to reach the target price.
    /// - Not all protocols support analytical solutions for this problem, requiring numerical
    ///   methods.
    PoolTargetPrice {
        /// The target marginal price for the pool after the trade. The pool's price decreases
        /// toward this target as token_in is sold into it.
        target: Price,
        /// Fraction above `target` considered acceptable. After trading, the pool's marginal
        /// price will be in `[target, target * (1 + tolerance)]`.
        tolerance: f64,
        /// The lower bound for searching algorithms.
        min_amount_in: Option<BigUint>,
        /// The upper bound for searching algorithms.
        max_amount_in: Option<BigUint>,
    },
}

/// Represents the parameters for [ProtocolSim::query_pool_swap].
#[derive(Debug, Clone, PartialEq)]
pub struct QueryPoolSwapParams {
    /// The token being sold (swapped into the pool)
    token_in: Token,
    /// The token being bought (swapped out of the pool)
    token_out: Token,
    /// Type of price constraint to be applied. See [SwapConstraint].
    swap_constraint: SwapConstraint,
}

impl QueryPoolSwapParams {
    pub fn new(token_in: Token, token_out: Token, swap_constraint: SwapConstraint) -> Self {
        Self { token_in, token_out, swap_constraint }
    }

    /// Returns a reference to the input token (token being sold into the pool)
    pub fn token_in(&self) -> &Token {
        &self.token_in
    }

    /// Returns a reference to the output token (token being bought out of the pool)
    pub fn token_out(&self) -> &Token {
        &self.token_out
    }

    /// Returns a reference to the price constraint
    pub fn swap_constraint(&self) -> &SwapConstraint {
        &self.swap_constraint
    }
}

/// ProtocolSim trait
/// This trait defines the methods that a protocol state must implement in order to be used
/// in the trade simulation.
#[typetag::serde(tag = "protocol", content = "state")]
pub trait ProtocolSim: fmt::Debug + Send + Sync + 'static {
    /// Returns the fee of the protocol as ratio
    ///
    /// E.g. if the fee is 1%, the value returned would be 0.01.
    ///
    /// # Panics
    ///
    /// Currently panic for protocols with asymmetric fees (e.g. Rocketpool, Uniswap V4),
    /// where a single fee value cannot represent the protocol's fee structure.
    fn fee(&self) -> f64;

    /// Returns the protocol's current spot buy price for `base` in units of `quote`.
    ///
    /// The returned price is the amount of `quote` required to buy exactly 1 unit of `base`,
    /// accounting for the protocol fee (i.e. `price = pre_fee_price / (1.0 - fee)`)
    /// and assuming zero slippage (i.e., a negligibly small trade size).
    ///
    /// # Arguments
    /// * `base` - the token being priced (what you buy). For BTC/USDT, BTC is the base token.
    /// * `quote` - the token used to price (pay) for `base`. For BTC/USDT, USDT is the quote token.
    ///
    /// # Examples
    /// If the BTC/USDT is trading at 1000 with a 20% fee, this returns `1000 / (1.0 - 0.20) = 1250`
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError>;

    /// Returns the amount out given an amount in and input/output tokens.
    ///
    /// # Arguments
    ///
    /// * `amount_in` - The amount in of the input token.
    /// * `token_in` - The input token ERC20 token.
    /// * `token_out` - The output token ERC20 token.
    ///
    /// # Returns
    ///
    /// A `Result` containing a `GetAmountOutResult` struct on success or a
    ///  `SimulationError` on failure.
    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError>;

    /// Computes the maximum amount that can be traded between two tokens.
    ///
    /// This function calculates the maximum possible trade amount between two tokens,
    /// taking into account the protocol's specific constraints and mechanics.
    /// The implementation details vary by protocol - for example:
    /// - For constant product AMMs (like Uniswap V2), this is based on available reserves
    /// - For concentrated liquidity AMMs (like Uniswap V3), this considers liquidity across tick
    ///   ranges
    ///
    /// Note: if there are no limits, the returned amount will be a "soft" limit,
    ///       meaning that the actual amount traded could be higher but it's advised to not
    ///       exceed it.
    ///
    /// # Arguments
    /// * `sell_token` - The address of the token being sold
    /// * `buy_token` - The address of the token being bought
    ///
    /// # Returns
    /// * `Ok((BigUint, BigUint))` - A tuple containing:
    ///   - First element: The maximum input amount (sell_token)
    ///   - Second element: The maximum output amount (buy_token)
    ///
    /// For `let res = get_limits(...)`, the valid input domain for `get_amount_out` is `[0,
    /// res.0]`.
    ///
    /// * `Err(SimulationError)` - If any unexpected error occurs
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError>;

    /// Decodes and applies a protocol state delta to the state
    ///
    /// Will error if the provided delta is missing any required attributes or if any of the
    /// attribute values cannot be decoded.
    ///
    /// # Arguments
    ///
    /// * `delta` - A `ProtocolStateDelta` from the tycho indexer
    ///
    /// # Returns
    ///
    /// * `Result<(), TransitionError<String>>` - A `Result` containing `()` on success or a
    ///   `TransitionError` on failure.
    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError>;

    /// Calculates the swap volume required to achieve the provided goal when trading against this
    /// pool.
    ///
    /// This method will branch towards different behaviors based on [SwapConstraint] enum. Please
    /// refer to its documentation for further details on each behavior.
    ///
    /// In short, the current two options are:
    /// - Maximize your trade while respecting a trade limit price:
    ///   [SwapConstraint::TradeLimitPrice]
    /// - Move the pool price to a target price: [SwapConstraint::PoolTargetPrice]
    ///
    /// # Arguments
    ///
    /// * `params` - A [QueryPoolSwapParams] struct containing the inputs for this method.
    ///
    /// # Returns
    ///
    /// * `Ok(PoolSwap)` - A `PoolSwap` struct containing the amounts to be traded and the state of
    ///   the pool after trading.
    /// * `Err(SimulationError)` - If:
    ///   - The calculation encounters numerical issues
    ///   - The method is not implemented for this protocol
    #[allow(unused)]
    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        Err(SimulationError::FatalError("query_pool_swap not implemented".into()))
    }

    /// Clones the protocol state as a trait object.
    /// This allows the state to be cloned when it is being used as a `Box<dyn ProtocolSim>`.
    fn clone_box(&self) -> Box<dyn ProtocolSim>;

    /// Allows downcasting of the trait object to its underlying type.
    fn as_any(&self) -> &dyn Any;

    /// Allows downcasting of the trait object to its mutable underlying type.
    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Compares two protocol states for equality.
    /// This method must be implemented to define how two protocol states are considered equal
    /// (used for tests).
    fn eq(&self, other: &dyn ProtocolSim) -> bool;

    /// Cast as IndicativelyPriced. This is necessary for RFQ protocols
    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Err(SimulationError::FatalError("Pool State does not implement IndicativelyPriced".into()))
    }
}

impl Clone for Box<dyn ProtocolSim> {
    fn clone(&self) -> Box<dyn ProtocolSim> {
        self.clone_box()
    }
}

impl<T> ProtocolSim for T
where
    T: SwapQuoter + Clone + Send + Sync + Eq + 'static,
{
    fn fee(&self) -> f64 {
        self.quotable_pairs()
            .iter()
            .map(|(t0, t1)| {
                let amount = BigUint::from(10u32).pow(t0.decimals);
                if let Ok(params) = QuoteParams::fixed_in(&t0.address, &t1.address, amount) {
                    self.fee(params)
                        .map(|f| f.fee())
                        .unwrap_or(f64::MAX)
                } else {
                    f64::MAX
                }
            })
            .reduce(f64::min)
            .unwrap_or(f64::MAX)
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.marginal_price(MarginalPriceParams::new(&base.address, &quote.address))
            .map(|r| r.price())
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        #[allow(deprecated)]
        self.quote(
            QuoteParams::fixed_in(&token_in.address, &token_out.address, amount_in)?
                .with_new_state(),
        )
        .map(|r| {
            GetAmountOutResult::new(
                r.amount_out().clone(),
                r.gas().clone(),
                r.new_state()
                    .expect("quote includes new state")
                    .to_protocol_sim(),
            )
        })
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.swap_limits(LimitsParams::new(&sell_token, &buy_token))
            .map(|r| (r.range_in().upper().clone(), r.range_out().upper().clone()))
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.delta_transition(TransitionParams::new(delta, tokens, balances))
            .map(|_| ())
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        let constraint = match params.swap_constraint.clone() {
            SwapConstraint::TradeLimitPrice { limit, tolerance, min_amount_in, max_amount_in } => {
                swap::SwapConstraint::TradeLimitPrice {
                    limit,
                    tolerance,
                    min_amount_in,
                    max_amount_in,
                }
            }
            SwapConstraint::PoolTargetPrice { target, tolerance, min_amount_in, max_amount_in } => {
                swap::SwapConstraint::PoolTargetPrice {
                    target,
                    tolerance,
                    min_amount_in,
                    max_amount_in,
                }
            }
        };
        #[allow(deprecated)]
        self.query_swap(swap::QuerySwapParams::new(
            &params.token_in.address,
            &params.token_out.address,
            constraint,
        ))
        .map(|r| {
            PoolSwap::new(
                r.amount_in().clone(),
                r.amount_out().clone(),
                r.new_state().unwrap().to_protocol_sim(),
                r.price_points().as_ref().map(|points| {
                    points
                        .iter()
                        .map(|p| {
                            PricePoint::new(
                                p.amount_in().clone(),
                                p.amount_out().clone(),
                                p.price(),
                            )
                        })
                        .collect()
                }),
            )
        })
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        #[allow(deprecated)]
        self.to_protocol_sim()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        if let Some(other) = other.as_any().downcast_ref::<T>() {
            self == other
        } else {
            false
        }
    }

    fn typetag_name(&self) -> &'static str {
        self.typetag_name()
    }

    fn typetag_deserialize(&self) {
        self.typetag_deserialize()
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn serde() {
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
        struct DummyProtocol {
            reserve_0: u64,
            reserve_1: u64,
        }

        #[typetag::serde]
        impl ProtocolSim for DummyProtocol {
            fn clone_box(&self) -> Box<dyn ProtocolSim> {
                todo!()
            }

            fn as_any(&self) -> &dyn Any {
                self
            }

            fn as_any_mut(&mut self) -> &mut dyn Any {
                todo!()
            }

            fn eq(&self, other: &dyn ProtocolSim) -> bool {
                if let Some(other) = other.as_any().downcast_ref::<Self>() {
                    self.reserve_0 == other.reserve_0 && self.reserve_1 == other.reserve_1
                } else {
                    false
                }
            }

            fn fee(&self) -> f64 {
                todo!()
            }
            fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
                todo!()
            }
            fn get_amount_out(
                &self,
                _amount_in: BigUint,
                _token_in: &Token,
                _token_out: &Token,
            ) -> Result<GetAmountOutResult, SimulationError> {
                todo!()
            }
            fn get_limits(
                &self,
                _sell_token: Bytes,
                _buy_token: Bytes,
            ) -> Result<(BigUint, BigUint), SimulationError> {
                todo!()
            }
            fn delta_transition(
                &mut self,
                _delta: ProtocolStateDelta,
                _tokens: &HashMap<Bytes, Token>,
                _balances: &Balances,
            ) -> Result<(), TransitionError> {
                todo!()
            }
        }

        let state = DummyProtocol { reserve_0: 1, reserve_1: 2 };

        assert_eq!(serde_json::to_string(&state).unwrap(), r#"{"reserve_0":1,"reserve_1":2}"#);
        assert_eq!(
            serde_json::to_string(&state as &dyn ProtocolSim).unwrap(),
            r#"{"protocol":"DummyProtocol","state":{"reserve_0":1,"reserve_1":2}}"#
        );

        let deserialized: Box<dyn ProtocolSim> = serde_json::from_str(
            r#"{"protocol":"DummyProtocol","state":{"reserve_0":1,"reserve_1":2}}"#,
        )
        .unwrap();

        assert!(deserialized.eq(&state));
    }
}
