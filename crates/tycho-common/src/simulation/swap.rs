use std::{collections::HashMap, fmt, fmt::Debug, sync::Arc};

use itertools::Itertools;
use num_bigint::BigUint;

use crate::{
    dto::ProtocolStateDelta,
    models::{protocol::ProtocolComponent, token::Token},
    simulation::{
        errors::{SimulationError, TransitionError},
        indicatively_priced::IndicativelyPriced,
        protocol_sim::{Balances, Price, ProtocolSim},
    },
    Bytes,
};

/// Result type for swap simulation operations that may fail with a `SimulationError`.
pub type SimulationResult<T> = Result<T, SimulationError>;

/// Type alias for token addresses, represented as raw bytes.
pub type TokenAddress = Bytes;

/// Macro that generates parameter structs with embedded blockchain context.
///
/// This macro creates structs that automatically include a `Context` field and provides
/// methods for managing blockchain context (block number and timestamp). All parameter
/// structs used in swap simulations should be created with this macro to ensure consistent
/// context handling.
///
/// # Generated Methods
/// - `with_context(context: Context) -> Self` - Sets the blockchain context
/// - `context() -> &Context` - Gets a reference to the current context
///
/// # Example
/// ```rust
/// params_with_context! {
///     pub struct MyParams {
///         token: TokenAddress,
///         amount: BigUint,
///     }
/// }
/// ```
macro_rules! params_with_context {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident $(<$($gen:tt),*>)? {
            $($field:ident: $ty:ty),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone)]
        $vis struct $name $(<$($gen),*>)? {
            context: Context,
            $($field: $ty,)*
        }

        impl $(<$($gen),*>)? $name $(<$($gen),*>)? {

            pub fn with_context(mut self, context: Context) -> Self {
                self.context = context;
                self
            }

            pub fn context(&self) -> &Context {
                &self.context
            }
        }
    };
}

/// Blockchain context information used in swap simulations.
///
/// Contains optional future block information that can be used for time-sensitive
/// simulations or to simulate swaps at specific future blockchain states.
#[derive(Debug, Clone)]
pub struct Context {}

impl Default for Context {
    /// Creates a new `Context` with no future block information.
    fn default() -> Self {
        Self {}
    }
}

params_with_context! {
/// Parameters for requesting a swap quote from a pool.
///
/// Contains the tokens to swap between, the input amount, and whether the
/// simulation should return a modified state to reflect the swap execution.
pub struct QuoteParams<'a>{
        token_in: &'a TokenAddress,
        token_out: &'a TokenAddress,
        amount: QuoteAmount,
        return_new_state: bool,
    }
}

#[derive(Debug, Clone)]
pub enum QuoteAmount {
    FixedIn(BigUint),
    FixedOut(BigUint),
}

impl<'a> QuoteParams<'a> {
    /// Creates new fixed input parameters with default settings (no state modification).
    ///
    /// # Arguments
    /// * `token_in` - The token to sell
    /// * `token_out` - The token to buy
    /// * `amount` - The amount of input token to sell
    pub fn fixed_in(
        token_in: &'a TokenAddress,
        token_out: &'a TokenAddress,
        amount: BigUint,
    ) -> SimulationResult<Self> {
        if token_out == token_in {
            return Err(SimulationError::InvalidInput(
                "Quote tokens have to differ!".to_string(),
                None,
            ));
        }
        Ok(Self {
            context: Context::default(),
            token_in,
            token_out,
            amount: QuoteAmount::FixedIn(amount),
            return_new_state: false,
        })
    }

    /// Creates new fixed output parameters with default settings (no state modification).
    ///
    /// # Arguments
    /// * `token_in` - The token to sell
    /// * `token_out` - The token to buy
    /// * `amount` - The amount of output token to buy
    pub fn fixed_out(
        token_in: &'a TokenAddress,
        token_out: &'a TokenAddress,
        amount: BigUint,
    ) -> SimulationResult<Self> {
        if token_out == token_in {
            return Err(SimulationError::InvalidInput(
                "Quote tokens have to differ!".to_string(),
                None,
            ));
        }

        Ok(Self {
            context: Context::default(),
            token_in,
            token_out,
            amount: QuoteAmount::FixedOut(amount),
            return_new_state: false,
        })
    }

    pub fn amount(&self) -> &QuoteAmount {
        &self.amount
    }

    /// Configures the quote to modify the state during simulation.
    ///
    /// When enabled, the quote simulation will return an updated state
    /// as if the swap was actually executed.
    pub fn with_new_state(mut self) -> Self {
        self.return_new_state = true;
        self
    }

    /// Returns the input token address.
    pub fn token_in(&self) -> &TokenAddress {
        self.token_in
    }

    /// Returns the output token address.
    pub fn token_out(&self) -> &TokenAddress {
        self.token_out
    }

    /// Returns whether the simulation should return the post-swap state.
    pub fn should_return_new_state(&self) -> bool {
        self.return_new_state
    }
}

params_with_context! {
/// Parameters for querying swap limits from a pool.
///
/// Used to determine the minimum and maximum amounts that can be traded
/// between two tokens in the pool.
pub struct LimitsParams<'a> {
    token_in: &'a TokenAddress,
    token_out: &'a TokenAddress,
}
}

impl<'a> LimitsParams<'a> {
    /// Creates new parameters for querying swap limits.
    ///
    /// # Arguments
    /// * `token_in` - The input token address
    /// * `token_out` - The output token address
    pub fn new(token_in: &'a TokenAddress, token_out: &'a TokenAddress) -> Self {
        Self { context: Context::default(), token_in, token_out }
    }

    pub fn token_in(&self) -> &TokenAddress {
        self.token_in
    }

    pub fn token_out(&self) -> &TokenAddress {
        self.token_out
    }
}

params_with_context! {
/// Parameters for querying the spot price between two tokens.
///
/// Used to get the current marginal price for infinitesimally small trades
/// between two tokens in a pool.
pub struct MarginalPriceParams<'a> {
    token_in: &'a TokenAddress,
    token_out: &'a TokenAddress,
}
}

impl<'a> MarginalPriceParams<'a> {
    /// Creates new parameters for querying marginal price.
    ///
    /// # Arguments
    /// * `token_in` - The input token address
    /// * `token_out` - The output token address
    pub fn new(token_in: &'a TokenAddress, token_out: &'a TokenAddress) -> Self {
        Self { context: Context::default(), token_in, token_out }
    }
    pub fn token_in(&self) -> &TokenAddress {
        self.token_in
    }

    pub fn token_out(&self) -> &TokenAddress {
        self.token_out
    }
}

/// Represents a swap fee as a fraction.
///
/// The fee is typically expressed as a decimal (e.g., 0.003 for 0.3%).
pub struct SwapFee {
    fee: f64,
}

impl SwapFee {
    /// Creates a new swap fee.
    ///
    /// # Arguments
    /// * `fee` - The fee as a decimal fraction (e.g., 0.003 for 0.3%)
    pub fn new(fee: f64) -> Self {
        Self { fee }
    }

    /// Returns the fee as a decimal fraction.
    pub fn fee(&self) -> f64 {
        self.fee
    }
}

/// Represents the marginal price at which the next infinitesimal trade would execute.
///
/// This is the instantaneous price for very small trades at the current pool state,
/// often different from the effective price of larger trades due to slippage.
pub struct MarginalPrice {
    price: f64,
}

impl MarginalPrice {
    /// Creates a new marginal price.
    ///
    /// # Arguments
    /// * `price` - The marginal price as token_out/token_in ratio
    pub fn new(price: f64) -> Self {
        Self { price }
    }

    /// Returns the marginal price value.
    pub fn price(&self) -> f64 {
        self.price
    }
}

/// Result of a swap quote calculation.
///
/// Contains the expected output amount, gas cost, and optionally the new pool state
/// if the quote was requested with state modification enabled.
pub struct Quote {
    amount_out: BigUint,
    gas: BigUint,
    new_state: Option<Arc<dyn SwapQuoter>>,
}

impl Quote {
    /// Creates a new quote result.
    ///
    /// # Arguments
    /// * `amount_out` - The amount of output tokens that would be received
    /// * `gas` - The estimated gas cost for executing this swap, excluding token transfers cost
    /// * `new_state` - The new pool state after the swap (if state modification was requested)
    pub fn new(amount_out: BigUint, gas: BigUint, new_state: Option<Arc<dyn SwapQuoter>>) -> Self {
        Self { amount_out, gas, new_state }
    }

    /// Returns the amount of output tokens.
    pub fn amount_out(&self) -> &BigUint {
        &self.amount_out
    }

    /// Returns the estimated swap gas cost excluding including token transfer cost.
    pub fn gas(&self) -> &BigUint {
        &self.gas
    }

    /// Returns the new pool state after the swap, if available.
    pub fn new_state(&self) -> Option<Arc<dyn SwapQuoter>> {
        self.new_state.clone()
    }
}

/// Represents a numeric range with lower and upper bounds.
///
/// Used for specifying trading limits and constraints.
pub struct Range {
    lower: BigUint,
    upper: BigUint,
}

impl Range {
    /// Creates a new range with validation.
    ///
    /// # Arguments
    /// * `lower` - The lower bound (must be <= upper)
    /// * `upper` - The upper bound (must be >= lower)
    ///
    /// # Errors
    /// Returns `SimulationError::InvalidInput` if lower > upper.
    pub fn new(lower: BigUint, upper: BigUint) -> SimulationResult<Self> {
        if lower > upper {
            return Err(SimulationError::InvalidInput(
                "Invalid range! Argument lower > upper".to_string(),
                None,
            ));
        }
        Ok(Self { lower, upper })
    }

    /// Returns the lower bound.
    pub fn lower(&self) -> &BigUint {
        &self.lower
    }

    /// Returns the upper bound.
    pub fn upper(&self) -> &BigUint {
        &self.upper
    }
}

/// Defines the trading limits for input and output amounts in a swap.
///
/// Specifies the minimum and maximum amounts that can be traded through a pool
/// for both input and output tokens.
pub struct SwapLimits {
    range_in: Range,
    range_out: Range,
}

impl SwapLimits {
    /// Creates new swap limits.
    ///
    /// # Arguments
    /// * `range_in` - The valid range for input token amounts
    /// * `range_out` - The valid range for output token amounts
    pub fn new(range_in: Range, range_out: Range) -> Self {
        Self { range_in, range_out }
    }

    /// Returns the input amount limits.
    pub fn range_in(&self) -> &Range {
        &self.range_in
    }

    /// Returns the output amount limits.
    pub fn range_out(&self) -> &Range {
        &self.range_out
    }
}

params_with_context! {
/// Parameters for applying protocol state transitions.
///
/// Contains the state delta and associated data needed to transition
/// a pool's state in response to blockchain events.
pub struct TransitionParams<'a> {
        delta: ProtocolStateDelta,
        tokens: &'a HashMap<Bytes, Token>,
        balances: &'a Balances,
    }
}

impl<'a> TransitionParams<'a> {
    /// Creates new parameters for state transition.
    ///
    /// # Arguments
    /// * `delta` - The protocol state change to apply
    /// * `tokens` - Map of token addresses to token metadata
    /// * `balances` - Current token balances in the system
    pub fn new(
        delta: ProtocolStateDelta,
        tokens: &'a HashMap<Bytes, Token>,
        balances: &'a Balances,
    ) -> Self {
        Self { context: Context::default(), delta, tokens, balances }
    }

    pub fn delta(&self) -> &ProtocolStateDelta {
        &self.delta
    }

    pub fn tokens(&self) -> &HashMap<Bytes, Token> {
        self.tokens
    }

    pub fn balances(&self) -> &Balances {
        self.balances
    }
}

/// Result of applying a state transition to a pool.
///
/// Currently, a placeholder struct that may be extended in the future
/// to contain transition metadata or validation results.
pub struct Transition {}

impl Default for Transition {
    /// Creates a new transition result.
    fn default() -> Self {
        Self {}
    }
}

/// Defines constraints for advanced quote calculations.
///
/// These constraints allow sophisticated trading strategies by limiting swaps
/// based on price thresholds or targeting specific pool states.
#[derive(Debug, Clone, PartialEq)]
pub enum SwapConstraint {
    /// This mode will calculate the maximum trade that this pool can execute while respecting a
    /// trade limit price.
    #[non_exhaustive]
    TradeLimitPrice {
        /// The minimum acceptable price for the resulting trade, as a [Price] struct. The
        /// resulting amount_out / amount_in must be >= trade_limit_price
        limit: Price,
        /// The tolerance as a fraction to be applied on top of (increasing) the trade
        /// limit price, raising the acceptance threshold. This is used to loosen the acceptance
        /// criteria for implementations of this method, but will never allow violating the trade
        /// limit price itself.
        tolerance: f64,
        /// The minimum amount of token_in that must be used for this trade.
        min_amount_in: Option<BigUint>,
        /// The maximum amount of token_in that can be used for this trade.
        max_amount_in: Option<BigUint>,
    },

    /// This mode will calculate the amount of token_in required to move the pool's marginal price
    /// down to a target price, and the amount of token_out received.
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
    #[non_exhaustive]
    PoolTargetPrice {
        /// The marginal price we want the pool to be after the trade, as a [Price] struct. The
        /// pool's price will move down to this level as token_in is sold into it
        target: Price,
        /// The tolerance as a fraction of the resulting pool marginal price. After trading, the
        /// pool's price will decrease to the interval `[target, target * (1 +
        /// tolerance)]`.
        tolerance: f64,
        /// The lower bound for searching algorithms.
        min_amount_in: Option<BigUint>,
        /// The upper bound for searching algorithms.
        max_amount_in: Option<BigUint>,
    },
}

impl SwapConstraint {
    /// Creates a trade limit price constraint.
    ///
    /// This constraint finds the maximum trade size while respecting a minimum price
    /// threshold. See `SwapConstraint::TradeLimitPrice` for details.
    ///
    /// # Arguments
    /// * `limit` - The minimum acceptable price for the trade
    /// * `tolerance` - Additional tolerance as a fraction to loosen the constraint
    pub fn trade_limit_price(limit: Price, tolerance: f64) -> Self {
        SwapConstraint::TradeLimitPrice {
            limit,
            tolerance,
            min_amount_in: None,
            max_amount_in: None,
        }
    }

    /// Creates a pool target price constraint.
    ///
    /// This constraint calculates the trade needed to move the pool's price to a
    /// target level. See `SwapConstraint::PoolTargetPrice` for details.`
    ///
    /// # Arguments
    /// * `target` - The desired final marginal price of the pool
    /// * `tolerance` - Acceptable variance from the target price as a fraction
    pub fn pool_target_price(target: Price, tolerance: f64) -> Self {
        SwapConstraint::PoolTargetPrice {
            target,
            tolerance,
            min_amount_in: None,
            max_amount_in: None,
        }
    }

    /// Adds a lower bound to the constraint's search range.
    ///
    /// # Arguments
    /// * `lower` - The minimum amount_in to consider
    ///
    /// # Returns
    /// The modified constraint with the lower bound applied.
    pub fn with_lower_bound(mut self, lower: BigUint) -> SimulationResult<Self> {
        match &mut self {
            SwapConstraint::PoolTargetPrice { min_amount_in, .. } => {
                *min_amount_in = Some(lower);
                Ok(self)
            }
            SwapConstraint::TradeLimitPrice { min_amount_in, .. } => {
                *min_amount_in = Some(lower);
                Ok(self)
            }
        }
    }

    /// Adds an upper bound to the constraint's search range.
    ///
    /// # Arguments
    /// * `upper` - The maximum amount_in to consider
    ///
    /// # Returns
    /// The modified constraint with the upper bound applied.
    pub fn with_upper_bound(mut self, upper: BigUint) -> SimulationResult<Self> {
        match &mut self {
            SwapConstraint::PoolTargetPrice { max_amount_in, .. } => {
                *max_amount_in = Some(upper);
                Ok(self)
            }
            SwapConstraint::TradeLimitPrice { max_amount_in, .. } => {
                *max_amount_in = Some(upper);
                Ok(self)
            }
        }
    }
}

params_with_context! {
/// Parameters for advanced swap queries with constraints.
///
/// Used for sophisticated swap calculations that respect price limits or target
/// prices instead of given amount values.
pub struct QuerySwapParams<'a> {
    token_in: &'a TokenAddress,
    token_out: &'a TokenAddress,
    swap_constraint: SwapConstraint,
}
}

impl<'a> QuerySwapParams<'a> {
    /// Creates new parameters for constrained swap queries.
    ///
    /// # Arguments
    /// * `token_in` - The input token address
    /// * `token_out` - The output token metadata
    /// * `swap_constraint` - The constraint to apply to the swap calculation
    pub fn new(
        token_in: &'a TokenAddress,
        token_out: &'a TokenAddress,
        swap_constraint: SwapConstraint,
    ) -> Self {
        Self { context: Context::default(), token_in, token_out, swap_constraint }
    }

    pub fn token_in(&self) -> &'a TokenAddress {
        self.token_in
    }

    pub fn token_out(&self) -> &'a TokenAddress {
        self.token_out
    }

    pub fn swap_constraint(&self) -> &SwapConstraint {
        &self.swap_constraint
    }
}

/// Result of an advanced swap calculation with constraints.
///
/// Contains the calculated swap amounts, optionally the new pool state,
/// and price points traversed during calculation for optimization purposes.
pub struct Swap {
    /// The amount of token_in sold to the component
    amount_in: BigUint,
    /// The amount of token_out bought from the component
    amount_out: BigUint,
    /// The new state of the component after the swap
    new_state: Option<Arc<dyn SwapQuoter>>,
    /// Optional price points that the pool was transitioned through while computing this swap.
    /// The values are tuples of (amount_in, amount_out, price). This is useful for repeated calls
    /// by providing good bounds for the next call.
    price_points: Option<Vec<PricePoint>>,
}

/// A point on the AMM price curve.
///
/// Collected during iterative numerical search algorithms.
/// These points can be reused as bounds for subsequent searches, improving convergence speed.
#[derive(Debug, Clone)]
pub struct PricePoint {
    /// The amount of token_in in atomic units (wei).
    amount_in: BigUint,
    /// The amount of token_out in atomic units (wei).
    amount_out: BigUint,
    /// The price in units of `[token_out/token_in]` scaled by decimals.
    ///
    /// Computed as `(amount_out / 10^token_out_decimals) / (amount_in / 10^token_in_decimals)`.
    price: f64,
}

impl PricePoint {
    pub fn amount_in(&self) -> &BigUint {
        &self.amount_in
    }

    pub fn amount_out(&self) -> &BigUint {
        &self.amount_out
    }

    pub fn price(&self) -> f64 {
        self.price
    }
}

impl Swap {
    /// Creates a new swap result.
    ///
    /// # Arguments
    /// * `amount_in` - The amount of input tokens used
    /// * `amount_out` - The amount of output tokens received
    /// * `new_state` - The new pool state after the swap (if calculated)
    /// * `price_points` - Optional price trajectory data for optimization
    pub fn new(
        amount_in: BigUint,
        amount_out: BigUint,
        new_state: Option<Arc<dyn SwapQuoter>>,
        price_points: Option<Vec<PricePoint>>,
    ) -> Self {
        Self { amount_in, amount_out, new_state, price_points }
    }

    /// Returns the amount of input tokens used.
    pub fn amount_in(&self) -> &BigUint {
        &self.amount_in
    }

    /// Returns the amount of output tokens received.
    pub fn amount_out(&self) -> &BigUint {
        &self.amount_out
    }

    /// Returns the new pool state after the swap, if calculated.
    pub fn new_state(&self) -> Option<Arc<dyn SwapQuoter>> {
        self.new_state.clone()
    }

    /// Returns the price points traversed during calculation.
    ///
    /// Each tuple contains (amount_in, amount_out, price) at various points
    /// during the swap calculation, useful for optimizing subsequent calls.
    pub fn price_points(&self) -> &Option<Vec<PricePoint>> {
        &self.price_points
    }
}

/// Core trait for implementing swap quote functionality.
///
/// This trait defines the interface that all liquidity sources must implement
/// to participate in swap quotes. It provides methods for price discovery,
/// quote calculation, state transitions, and advanced swap queries.
///
/// Implementations should be thread-safe and support cloning for parallel simulations.
#[typetag::serde(tag = "protocol", content = "state")]
pub trait SwapQuoter: fmt::Debug + Send + Sync + 'static {
    /// Returns the [`ProtocolComponent`] describing the protocol instance this quoter
    /// is associated with.
    ///
    /// The component provides **structural and descriptive metadata** about the protocol,
    /// such as the set of involved tokens and protocol-specific configuration, but does not
    /// represent a mutable simulation state.
    ///
    /// # Semantics
    ///
    /// - The returned component is expected to be **stable for the lifetime of the quoter**.
    /// - Multiple quoter instances may share the same component instance; callers should not assume
    ///   unique ownership (e.g. quoter instances may represent the state at different points in
    ///   time)
    /// - The component is used for discovery and introspection (e.g. determining supported tokens
    ///   or deriving default quotable pairs), not for executing swaps.
    ///
    /// # Ownership
    ///
    /// This method returns an `Arc` to allow cheap cloning and shared access without
    /// constraining the internal storage strategy of the implementation.
    ///
    /// # Intended use
    ///
    /// Typical uses include:
    /// - Inspecting the tokens and configuration exposed by the protocol
    /// - Deriving default [`quotable_pairs`](Self::quotable_pairs)
    /// - Identifying or grouping quoters by protocol metadata
    fn component(&self) -> Arc<ProtocolComponent<Arc<Token>>>;

    /// Returns the set of **directed token pairs** for which this quoter can produce swap quotes.
    ///
    /// Each `(base, quote)` pair indicates that a swap from `base` to `quote` is supported.
    /// Direction matters: `(A, B)` and `(B, A)` are considered distinct pairs and might not
    /// both be present.
    ///
    /// # Semantics
    ///
    /// - The returned set represents **capability**, not liquidity or pricing guarantees. A pair
    ///   being present does not imply that a quote will be favorable or even currently executable,
    ///   only that the quoter understands how to price it.
    /// - The set may be **computed dynamically** and is not required to be stable across calls,
    ///   though most implementations are expected to return the same result unless the underlying
    ///   protocol configuration changes.
    ///
    /// # Default behavior
    ///
    /// The default implementation derives the pairs from the tokens exposed by
    /// [`component()`](Self::component), returning all ordered pairs `(a, b)` where `a != b`.
    /// Protocols with restricted or asymmetric support (e.g. RFQ-based or single-sided
    /// designs) should override this method.
    ///
    ///
    /// # Intended use
    ///
    /// This method is primarily intended for routing, discovery, and validation logic,
    /// allowing callers to determine whether a quote request is meaningful before invoking
    /// [`quote()`](Self::quote).
    fn quotable_pairs(&self) -> Vec<(Arc<Token>, Arc<Token>)> {
        let component = self.component();
        component
            .tokens
            .iter()
            .permutations(2)
            .map(|token| (token[0].clone(), token[1].clone()))
            .collect()
    }

    /// Computes the protocol fee applicable to a prospective swap described by `params`.
    ///
    /// This method evaluates the fee that would be charged by the protocol for the given
    /// swap parameters, without performing the swap or mutating any internal state.
    ///
    /// # Semantics
    ///
    /// - The returned [`SwapFee`] represents the **protocol-defined fee component** of the swap
    ///   (e.g. LP fee, protocol fee, or RFQ spread), as understood by this quoter.
    /// - Fee computation is **pure and side-effect free**; calling this method must not modify the
    ///   internal state of the quoter.
    /// - The fee may depend on the full set of quote parameters (including direction, amount, or
    ///   other protocol-specific inputs).
    ///
    /// # Relation to quoting
    ///
    /// Implementations may internally reuse logic from [`quote()`](Self::quote), but this method
    /// exists to allow callers to:
    /// - Inspect or decompose pricing components
    /// - Perform fee-aware routing or optimization
    /// - Estimate costs without requesting a full quote
    ///
    /// # Errors
    ///
    /// Returns an error if the fee cannot be determined for the given parameters (e.g. the
    /// pair is not quotable or required inputs are missing)
    fn fee(&self, params: QuoteParams) -> SimulationResult<SwapFee>;

    /// Computes the **marginal (infinitesimal) price** for a swap described by `params`,
    /// with **all protocol fees included**.
    ///
    /// The marginal price represents the instantaneous exchange rate at the current
    /// protocol state, evaluated at an infinitesimally small trade size. It reflects the
    /// derivative of output amount with respect to input amount for the specified swap
    /// direction, inclusive of any protocol-defined fees or spreads.
    ///
    /// # Semantics
    ///
    /// - Fees are **always included** in the returned [`MarginalPrice`].
    /// - The price is evaluated **at the margin** and does not represent an executable price for a
    ///   finite trade.
    /// - This method is **pure and side-effect free**; it must not mutate internal state.
    /// - For sufficiently small trade sizes, the marginal price should be consistent with
    ///   [`quote()`](Self::quote), up to numerical precision.
    ///
    /// # Use cases
    ///
    /// Typical uses include:
    /// - Price display and monitoring
    /// - Slippage estimation and sensitivity analysis
    /// - Routing heuristics and initial path selection
    ///
    /// # Errors
    ///
    /// Returns an error if the marginal price is undefined or cannot be computed for the
    /// given parameters (e.g. unsupported pair, zero liquidity, or missing inputs).
    fn marginal_price(&self, params: MarginalPriceParams) -> SimulationResult<MarginalPrice>;

    /// Produces a swap quote for the trade described by `params`, with **all protocol fees
    /// included**.
    ///
    /// The returned [`Quote`] represents the effective execution terms of the swap as
    /// understood by this quoter, including any protocol-defined fees, spreads, or
    /// adjustments. Calling this method does not perform the swap and does not mutate
    /// internal state.
    ///
    /// # Semantics
    ///
    /// - Fees are **always included** in the quoted price and amounts. Callers should not apply
    ///   additional protocol fees on top of the returned quote.
    /// - Fees mentioned above **do not include** chain fees such as gas costs.
    /// - The quote reflects a **finite-size trade** and therefore accounts for price impact where
    ///   applicable.
    /// - This method is **pure and side-effect free**; it must not mutate internal state.
    /// - For sufficiently small trade sizes, the quote should be consistent with
    ///   [`marginal_price()`](Self::marginal_price), up to numerical precision.
    ///
    /// # Intended use
    ///
    /// This method is the primary entry point for consumers of [`SwapQuoter`] and is
    /// intended for:
    /// - User-facing price discovery
    /// - Routing and optimization across multiple quoters
    /// - Simulation and what-if analysis
    ///
    /// # Errors
    ///
    /// Returns an error if a quote cannot be produced for the given parameters (e.g. the
    /// pair is not quotable, required inputs are missing, or the quote is undefined).
    fn quote(&self, params: QuoteParams) -> SimulationResult<Quote>;

    /// Returns the valid execution limits for a prospective quote
    ///
    /// The returned [`SwapLimits`] describes the bounds within which a swap can be quoted or
    /// simulated, such as minimum and maximum input or output amounts, given the current
    /// protocol state.
    ///
    /// # Semantics
    ///
    /// - Limits are evaluated **at the current state** of the quoter and do not imply that a quote
    ///   will succeed outside the returned bounds.
    /// - The limits may depend on swap direction, fees, liquidity constraints, or protocol-specific
    ///   rules.
    /// - This method is **pure and side-effect free**; it must not mutate internal state.
    /// - Limits are expressed in **fee-inclusive terms**, consistent with [`quote()`](Self::quote)
    ///   and [`marginal_price()`](Self::marginal_price).
    ///
    /// # Intended use
    ///
    /// Typical uses include:
    /// - Pre-validating quote requests before quoting
    /// - Bounding search spaces for routing and optimization
    /// - UI validation and input clamping
    ///
    /// # Errors
    ///
    /// Returns an error if limits cannot be determined for the given parameters (e.g. the
    /// pair is not quotable or required inputs are missing).
    fn swap_limits(&self, params: LimitsParams) -> SimulationResult<SwapLimits>;

    /// Produces an **advanced, price-constraint-based quote**
    ///
    /// Unlike [`quote()`](Self::quote), which prices a swap for a fixed input or output amount,
    /// `query_swap` solves for a swap that satisfies a higher-level [`SwapConstraint`],
    /// such as a minimum execution price or a target post-trade pool price.
    ///
    /// The returned [`Swap`] describes the swap that best satisfies the given constraint
    /// under the current protocol state, with **all protocol fees included**.
    ///
    /// # Semantics
    ///
    /// - The method is **read-only** and does not mutate internal state.
    /// - All amounts and prices in the returned [`Swap`] are **fee-inclusive**, consistent with
    ///   [`quote()`](Self::quote) and [`marginal_price()`](Self::marginal_price).
    /// - The constraint defines *what is solved for* (e.g. maximum trade size, target price),
    ///   rather than supplying an explicit trade amount.
    /// - Implementations may use analytical or numerical methods to satisfy the constraint, subject
    ///   to the provided tolerances and bounds.
    ///
    /// # Supported constraints
    ///
    /// The behavior of this method is defined by the [`SwapConstraint`] provided in
    /// `params`, including:
    ///
    /// - **Trade limit price**: computes the maximum executable trade size whose effective price
    ///   (amount_out / amount_in) meets or exceeds a specified limit.
    /// - **Pool target price**: computes the trade required to move the pool’s marginal price down
    ///   to a target level, within a specified tolerance.
    ///
    /// Bounds on the search space (minimum and maximum `amount_in`) are respected when
    /// provided.
    ///
    /// # Intended use
    ///
    /// Typical uses include:
    /// - Price-impact-aware execution planning
    /// - Strategy-driven routing (e.g. price-capped or price-targeting trades)
    /// - Liquidity probing and pool sensitivity analysis
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The constraint cannot be satisfied within the provided bounds or tolerances
    /// - The token pair is not quotable
    /// - The swap is undefined under the current protocol state
    fn query_swap(&self, params: QuerySwapParams) -> SimulationResult<Swap>;

    /// Applies a **protocol state delta**
    ///
    /// This method updates the internal protocol state of the quoter by applying the
    /// incremental changes described by `params`, typically derived from an external
    /// indexer or on-chain data source. It is used to keep the quoter’s local view of the
    /// protocol state in sync with the latest block.
    ///
    /// # Semantics
    ///
    /// - The provided delta is assumed to represent a **valid, externally observed state
    ///   transition** (e.g. from an indexer service) and is not re-validated as a swap.
    /// - Calling this method **mutates internal state**; all subsequent quotes, prices, and limits
    ///   will reflect the updated state.
    /// - The transition is applied **incrementally** and is expected to be composable with previous
    ///   deltas.
    ///
    /// # Intended use
    ///
    /// Typical uses include:
    /// - Applying per-block protocol updates received from an indexer
    /// - Advancing local state during historical replay or backfilling
    /// - Keeping multiple quoters synchronized with chain state
    ///
    /// # Errors
    ///
    /// Returns an error if the delta cannot be applied to the current state (e.g. it is
    /// incompatible, malformed, or violates protocol invariants).
    fn delta_transition(&mut self, params: TransitionParams)
        -> Result<Transition, TransitionError>;

    /// Clones the protocol state as a trait object.
    ///
    /// This method enables cloning when the pool is used as a boxed trait object,
    /// which is necessary for parallel simulations and state management.
    ///
    /// # Returns
    /// A new boxed instance with the same state as this pool.
    fn clone_box(&self) -> Box<dyn SwapQuoter>;

    /// Attempts to cast this pool to an indicatively priced pool.
    ///
    /// This is used for RFQ (Request for Quote) protocols that provide
    /// indicative pricing rather than deterministic calculations.
    ///
    /// # Returns
    /// A reference to the `IndicativelyPriced` implementation, or an error
    /// if this pool type doesn't support indicative pricing.
    ///
    /// # Default Implementation
    /// Returns an error indicating that indicative pricing is not supported.
    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Err(SimulationError::FatalError("Pool State does not implement IndicativelyPriced".into()))
    }

    #[deprecated(note = "ProtocolSim is deprecated. This method will be removed in v1.0.0")]
    fn to_protocol_sim(&self) -> Box<dyn ProtocolSim>;
}

/// Testing extension trait for SwapQuoter implementations.
///
/// Provides additional methods needed for testing and validation
/// that are not part of the main SwapQuoter interface.
#[cfg(test)]
pub trait SwapQuoterTestExt {
    /// Compares this pool state with another for equality.
    ///
    /// This method is used in tests to verify that pool states
    /// are equivalent after various operations.
    ///
    /// # Arguments
    /// * `other` - Another SwapQuoter to compare against
    ///
    /// # Returns
    /// `true` if the pool states are equivalent, `false` otherwise.
    fn eq(&self, other: &dyn SwapQuoter) -> bool;
}
