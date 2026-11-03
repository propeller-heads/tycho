use std::{any::Any, collections::HashMap};

use lunarbase_pmm_math::{
    sqrt_price_x96_to_price, try_simulate_successful_swap, Direction, MathError, PoolParams,
    RollbackReason, SimulationStatus, MAX_U112, MAX_U24, U256,
};
use num_bigint::BigUint;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, BlockContext, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
    Bytes,
};

use super::decoder::apply_delta;

pub type Address = [u8; 20];
// Pool-call estimate for routing, not a transaction gas limit.
const DEFAULT_GAS: u64 = 100_000;

/// Native simulation of the linear anchor and immediate punishment mechanism.
///
/// Tycho partner quotes always use fee multiplier one.
/// Active-reserve transitions assume standard token transfers and fully credited
/// fees with sufficient fee-bucket capacity; caller-specific fee accounting and
/// unsynced donations are not indexed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LunarBaseState {
    pub pool: Address,
    pub token_x: Address,
    pub token_y: Address,
    pub anchor_price_x96: U256,
    pub fee_ask_x24: u32,
    pub fee_bid_x24: u32,
    pub latest_update_block: u64,
    pub reserve_x: u128,
    pub reserve_y: u128,
    pub max_punishment_x24: u32,
    pub block_delay: u64,
    pub paused: bool,
    pub head_block: u64,
}

impl LunarBaseState {
    pub fn pool_params(&self) -> PoolParams {
        PoolParams {
            sqrt_price_x96: self.anchor_price_x96,
            fee_ask_x24: self.fee_ask_x24,
            fee_bid_x24: self.fee_bid_x24,
            reserve_x: self.reserve_x,
            reserve_y: self.reserve_y,
            max_punishment_x24: self.max_punishment_x24,
        }
    }

    pub fn is_fresh(&self) -> bool {
        self.head_block <
            self.latest_update_block
                .saturating_add(self.block_delay)
    }

    fn quote_exact_in(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<(U256, Self), QuoteError> {
        if self.paused {
            return Err(QuoteError::Paused);
        }

        if !self.is_fresh() {
            return Err(QuoteError::Stale {
                block_number: self.head_block,
                latest_update_block: self.latest_update_block,
                block_delay: self.block_delay,
            });
        }

        let direction = if token_in == self.token_x && token_out == self.token_y {
            Direction::XToY
        } else if token_in == self.token_y && token_out == self.token_x {
            Direction::YToX
        } else {
            return Err(QuoteError::InvalidTokenPair);
        };
        let simulation = try_simulate_successful_swap(
            &self.pool_params(),
            amount_in,
            direction,
            U256::from(1u64),
        )
        .map_err(QuoteError::Math)?;
        match simulation.status {
            SimulationStatus::Applied => {}
            SimulationStatus::RolledBack(RollbackReason::SwapImpossible) => {
                return Err(QuoteError::Rejected);
            }
            SimulationStatus::RolledBack(RollbackReason::ReserveTransitionOverflow) => {
                return Err(QuoteError::ReserveOverflow);
            }
            SimulationStatus::RolledBack(RollbackReason::LaterRevert) => {
                return Err(QuoteError::Rejected);
            }
        }

        let mut next = self.clone();
        next.reserve_x = simulation.post_swap.reserve_x;
        next.reserve_y = simulation.post_swap.reserve_y;
        next.fee_ask_x24 = simulation.post_swap.fee_ask_x24;
        next.fee_bid_x24 = simulation.post_swap.fee_bid_x24;
        Ok((simulation.quote.amount_out, next))
    }
}

#[typetag::serde]
impl ProtocolSim for LunarBaseState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.pool_params()
            .validate()
            .map_err(|err| map_quote_error(QuoteError::Math(err)))?;
        let token_in = address_from_bytes(base.address.as_ref())?;
        let token_out = address_from_bytes(quote.address.as_ref())?;
        if token_in == self.token_x && token_out == self.token_y {
            return Ok(apply_fee_discount(
                anchor_price(self.anchor_price_x96, base, quote),
                self.fee_bid_x24,
            ));
        }
        if token_in == self.token_y && token_out == self.token_x {
            if self.anchor_price_x96.is_zero() {
                return Ok(0.0);
            }
            return Ok(apply_fee_discount(
                1.0 / anchor_price(self.anchor_price_x96, quote, base),
                self.fee_ask_x24,
            ));
        }
        Err(SimulationError::InvalidInput("invalid LunarBase token pair".to_owned(), None))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        if amount_in == BigUint::ZERO {
            return Ok(GetAmountOutResult::new(
                BigUint::ZERO,
                BigUint::from(DEFAULT_GAS),
                Box::new(self.clone()),
            ));
        }

        let (amount_out, next_state) = self
            .quote_exact_in(
                address_from_bytes(token_in.address.as_ref())?,
                address_from_bytes(token_out.address.as_ref())?,
                biguint_to_u256(&amount_in)?,
            )
            .map_err(map_quote_error)?;

        Ok(GetAmountOutResult::new(
            u256_to_biguint(amount_out),
            BigUint::from(DEFAULT_GAS),
            Box::new(next_state),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let sell = address_from_bytes(sell_token.as_ref())?;
        let buy = address_from_bytes(buy_token.as_ref())?;
        if sell == self.token_x && buy == self.token_y {
            return quote_limit(self, sell, buy, Direction::XToY);
        }
        if sell == self.token_y && buy == self.token_x {
            return quote_limit(self, sell, buy, Direction::YToX);
        }
        Err(SimulationError::InvalidInput("invalid LunarBase token pair".to_owned(), None))
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        if let Some(name) = delta.deleted_attributes.iter().next() {
            return Err(TransitionError::DecodeError(format!(
                "LunarBase does not support deleted attributes: {name}"
            )));
        }

        apply_delta(self, delta.updated_attributes)
            .map_err(|err| TransitionError::DecodeError(format!("{err:?}")))?;
        Ok(())
    }

    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let was_fresh = self.is_fresh();
        self.head_block = block.number();
        was_fresh != self.is_fresh()
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        let (price, tolerance, min_amount, max_amount, trade_price) = match params.swap_constraint()
        {
            SwapConstraint::TradeLimitPrice { limit, tolerance, min_amount_in, max_amount_in } => {
                (limit, *tolerance, min_amount_in, max_amount_in, true)
            }
            SwapConstraint::PoolTargetPrice { target, tolerance, min_amount_in, max_amount_in } => {
                (target, *tolerance, min_amount_in, max_amount_in, false)
            }
        };
        if price.numerator == BigUint::ZERO ||
            price.denominator == BigUint::ZERO ||
            !tolerance.is_finite() ||
            tolerance < 0.0 ||
            matches!((min_amount, max_amount), (Some(min), Some(max)) if min > max)
        {
            return Err(SimulationError::InvalidInput(
                "invalid LunarBase price query bounds".into(),
                None,
            ));
        }
        let token_in = params.token_in();
        let token_out = params.token_out();
        let (mut maximum, _) =
            self.get_limits(token_in.address.clone(), token_out.address.clone())?;
        if let Some(max) = max_amount {
            maximum = maximum.min(max.clone());
        }
        let zero = || PoolSwap::new(BigUint::ZERO, BigUint::ZERO, Box::new(self.clone()), None);
        if maximum == BigUint::ZERO {
            return Ok(zero());
        }
        let direction = if token_in.address.as_ref() == self.token_x {
            Direction::XToY
        } else {
            Direction::YToX
        };
        let (minimum, _) = quote_input_bounds(self, direction);
        if maximum < minimum ||
            min_amount
                .as_ref()
                .is_some_and(|min| maximum < *min)
        {
            return Ok(zero());
        }
        let (amount_out, next) = self
            .quote_exact_in(
                address_from_bytes(token_in.address.as_ref())?,
                address_from_bytes(token_out.address.as_ref())?,
                biguint_to_u256(&maximum)?,
            )
            .map_err(map_quote_error)?;
        let amount_out = u256_to_biguint(amount_out);
        let full_fill_allowed = if trade_price {
            // Compare actual rounded output in raw token units, without f64/decimal loss.
            &amount_out * &price.denominator >= &maximum * &price.numerator
        } else {
            // The post-swap marginal price includes the updated directional punishment.
            let anchor_squared = u256_to_biguint(next.anchor_price_x96).pow(2);
            let q192 = BigUint::from(1u32) << 192;
            let (numerator, denominator, fee) = match direction {
                Direction::XToY => (anchor_squared, q192, next.fee_bid_x24),
                Direction::YToX => (q192, anchor_squared, next.fee_ask_x24),
            };
            let discount = if fee == MAX_U24 { 0 } else { (1u32 << 24) - fee };
            numerator * discount * &price.denominator >=
                denominator * (1u32 << 24) * &price.numerator
        };
        if full_fill_allowed {
            // A liquidity/amount cap can prevent reaching the target even though every
            // tradable amount remains above it. Return the capped fill instead of an error.
            return Ok(PoolSwap::new(maximum, amount_out, Box::new(next), None));
        }
        // Keep the existing numerical search for an interior price crossing. Its search
        // currently ignores optional amount bounds, so never return an out-of-bounds fill.
        let result = crate::evm::query_pool_swap::query_pool_swap(self, params)?;
        if result.amount_in() > &maximum ||
            min_amount
                .as_ref()
                .is_some_and(|min| result.amount_in() < min)
        {
            return Err(SimulationError::InvalidInput(
                "LunarBase price crossing outside amount bounds".into(),
                None,
            ));
        }
        // The numerical search can round distinct rational prices to the same f64.
        if trade_price &&
            result.amount_out() * &price.denominator < result.amount_in() * &price.numerator
        {
            return Err(SimulationError::InvalidInput(
                "LunarBase fill below trade limit price".into(),
                None,
            ));
        }
        Ok(result)
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
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum QuoteError {
    Paused,
    Stale { block_number: u64, latest_update_block: u64, block_delay: u64 },
    InvalidTokenPair,
    Rejected,
    ReserveOverflow,
    Math(MathError),
}

fn anchor_price(anchor_price_x96: U256, token_in: &Token, token_out: &Token) -> f64 {
    let decimals_adjustment = 10f64.powi(token_in.decimals as i32 - token_out.decimals as i32);
    sqrt_price_x96_to_price(anchor_price_x96) * decimals_adjustment
}

fn apply_fee_discount(price: f64, fee_x24: u32) -> f64 {
    if fee_x24 == MAX_U24 {
        return 0.0;
    }
    price * (1.0 - fee_x24 as f64 / (1u64 << 24) as f64)
}

/// Invert both gross-output floors to bound all executable inputs. A positive gross
/// quote stays positive unless the effective fee reaches the MAX_U24 sentinel.
fn quote_input_bounds(state: &LunarBaseState, direction: Direction) -> (BigUint, BigUint) {
    let (reserve_in, reserve_out) = match direction {
        Direction::XToY => (state.reserve_x, state.reserve_y),
        Direction::YToX => (state.reserve_y, state.reserve_x),
    };
    if reserve_out == 0 || state.anchor_price_x96.is_zero() {
        return (BigUint::ZERO, BigUint::ZERO);
    }
    let anchor = u256_to_biguint(state.anchor_price_x96);
    let q96 = BigUint::from(1u64) << 96;
    let (numerator, denominator) = match direction {
        Direction::XToY => (q96, anchor),
        Direction::YToX => (anchor, q96),
    };
    // floor(x * d / n) <= r iff x <= ((r + 1) * n - 1) / d.
    let intermediate = ((BigUint::from(reserve_out) + 1u32) * &numerator - 1u32) / &denominator;
    let maximum = ((intermediate + 1u32) * &numerator - 1u32) / &denominator;
    // Inverse ceilings find the first input with at least one gross output unit.
    let intermediate = (&numerator + &denominator - 1u32) / &denominator;
    let minimum = (intermediate * &numerator + &denominator - 1u32) / denominator;
    (minimum, maximum.min(BigUint::from(MAX_U112 - reserve_in)))
}

fn quote_limit(
    state: &LunarBaseState,
    token_in: Address,
    token_out: Address,
    direction: Direction,
) -> Result<(BigUint, BigUint), SimulationError> {
    if state.paused || !state.is_fresh() {
        return Ok((BigUint::ZERO, BigUint::ZERO));
    }
    state
        .pool_params()
        .validate()
        .map_err(|err| map_quote_error(QuoteError::Math(err)))?;
    let (mut low, mut high) = quote_input_bounds(state, direction);
    if high == BigUint::ZERO || low > high {
        return Ok((BigUint::ZERO, BigUint::ZERO));
    }
    match state.quote_exact_in(token_in, token_out, biguint_to_u256(&high)?) {
        Ok((output, _)) => return Ok((high, u256_to_biguint(output))),
        Err(QuoteError::Rejected | QuoteError::ReserveOverflow) => {}
        Err(err) => return Err(map_quote_error(err)),
    }
    let mut low_output = match state.quote_exact_in(token_in, token_out, biguint_to_u256(&low)?) {
        Ok((output, _)) => u256_to_biguint(output),
        Err(QuoteError::Rejected | QuoteError::ReserveOverflow) => {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        Err(err) => return Err(map_quote_error(err)),
    };
    // Gross output and punishment are nondecreasing. Between the positive-output
    // minimum and the reserve bounds, only fee saturation can stop execution.
    // Keep low executable and high rejected; at most 112 bisections are needed.
    while &high - &low > BigUint::from(1u32) {
        let middle = (&low + &high) >> 1;
        match state.quote_exact_in(token_in, token_out, biguint_to_u256(&middle)?) {
            Ok((output, _)) => {
                low = middle;
                low_output = u256_to_biguint(output);
            }
            Err(QuoteError::Rejected | QuoteError::ReserveOverflow) => high = middle,
            Err(err) => return Err(map_quote_error(err)),
        }
    }
    Ok((low, low_output))
}

fn address_from_bytes(value: &[u8]) -> Result<Address, SimulationError> {
    value.try_into().map_err(|_| {
        SimulationError::InvalidInput(
            format!("expected 20-byte address, got {}", value.len()),
            None,
        )
    })
}

fn biguint_to_u256(value: &BigUint) -> Result<U256, SimulationError> {
    let bytes = value.to_bytes_be();
    if bytes.len() > 32 {
        return Err(SimulationError::InvalidInput("amount_in exceeds uint256".to_owned(), None));
    }
    Ok(U256::from_be_slice(&bytes))
}

fn u256_to_biguint(value: U256) -> BigUint {
    BigUint::from_bytes_be(&value.to_be_bytes::<32>())
}

fn map_quote_error(err: QuoteError) -> SimulationError {
    SimulationError::InvalidInput(format!("LunarBase quote rejected: {err:?}"), None)
}

#[cfg(test)]
mod tests {
    use tycho_common::models::Chain;

    use super::*;

    fn addr(byte: u8) -> [u8; 20] {
        [byte; 20]
    }

    fn token(address: Address, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from(address.to_vec()),
            symbol,
            decimals,
            100,
            &[Some(100_000)],
            Chain::Base,
            100,
        )
    }

    fn state() -> LunarBaseState {
        LunarBaseState {
            pool: addr(9),
            token_x: addr(1),
            token_y: addr(2),
            anchor_price_x96: U256::from(1u128 << 96),
            fee_ask_x24: 0,
            fee_bid_x24: 0,
            latest_update_block: 100,
            reserve_x: 1_000_000,
            reserve_y: 1_000_000,
            max_punishment_x24: 0,
            block_delay: 2,
            paused: false,
            head_block: 100,
        }
    }

    #[test]
    fn limits_use_output_liquidity_for_asymmetric_reserves() {
        for x_to_y in [true, false] {
            let mut state = state();
            let (sell, buy) = if x_to_y {
                state.reserve_x = 1;
                state.reserve_y = 100;
                (state.token_x, state.token_y)
            } else {
                state.reserve_x = 100;
                state.reserve_y = 1;
                (state.token_y, state.token_x)
            };
            assert_eq!(
                state
                    .get_limits(sell.into(), buy.into())
                    .unwrap(),
                (100u32.into(), 100u32.into())
            );
        }
    }

    #[test]
    fn limits_find_the_last_fill_before_punishment_saturates() {
        let mut state = state();
        state.reserve_x = 100;
        state.reserve_y = 100;
        state.fee_bid_x24 = 1 << 23;
        state.fee_ask_x24 = 1 << 23;
        state.max_punishment_x24 = MAX_U24;
        for (sell, buy) in [(state.token_x, state.token_y), (state.token_y, state.token_x)] {
            let expected = (1..=200u64)
                .rev()
                .find_map(|input| {
                    state
                        .quote_exact_in(sell, buy, U256::from(input))
                        .ok()
                        .map(|(output, _)| (BigUint::from(input), u256_to_biguint(output)))
                })
                .unwrap();
            assert_eq!(
                state
                    .get_limits(sell.into(), buy.into())
                    .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn limits_use_remaining_input_reserve_capacity_exactly() {
        let mut state = state();
        state.reserve_x = MAX_U112 - 7;
        state.reserve_y = 100;
        assert_eq!(
            state
                .get_limits(state.token_x.into(), state.token_y.into())
                .unwrap(),
            (7u32.into(), 7u32.into())
        );
    }

    #[test]
    fn price_queries_fill_flat_pools_in_both_directions() {
        use tycho_common::simulation::protocol_sim::{Price, SwapConstraint};
        for x_to_y in [true, false] {
            for trade_price in [true, false] {
                let state = state();
                let (sell, buy) = if x_to_y {
                    (state.token_x, state.token_y)
                } else {
                    (state.token_y, state.token_x)
                };
                for max in [None, Some(BigUint::from(123u32))] {
                    let price = Price::new(999u32.into(), 1000u32.into());
                    let constraint = if trade_price {
                        SwapConstraint::TradeLimitPrice {
                            limit: price,
                            tolerance: 1e-6,
                            min_amount_in: None,
                            max_amount_in: max.clone(),
                        }
                    } else {
                        SwapConstraint::PoolTargetPrice {
                            target: price,
                            tolerance: 1e-6,
                            min_amount_in: None,
                            max_amount_in: max.clone(),
                        }
                    };
                    // Raw-unit prices do not depend on display decimals.
                    let params = QueryPoolSwapParams::new(
                        token(sell, "IN", 18),
                        token(buy, "OUT", 6),
                        constraint,
                    );
                    let swap = state.query_pool_swap(&params).unwrap();
                    let expected = max.unwrap_or(1_000_000u32.into());
                    assert_eq!(swap.amount_in(), &expected);
                    assert_eq!(swap.amount_out(), &expected);
                }
            }
        }
    }

    #[test]
    fn price_queries_distinguish_rounded_trade_and_post_swap_prices() {
        use tycho_common::simulation::protocol_sim::{Price, SwapConstraint};
        let mut state = state();
        state.reserve_y = 2;
        state.fee_bid_x24 = 1 << 22;
        let x = token(state.token_x, "X", 18);
        let y = token(state.token_y, "Y", 18);
        let params = QueryPoolSwapParams::new(
            x.clone(),
            y.clone(),
            SwapConstraint::TradeLimitPrice {
                limit: Price::new(9u32.into(), 10u32.into()),
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );
        let result = state.query_pool_swap(&params).unwrap();
        assert_eq!(result.amount_in(), &BigUint::from(2u32));
        assert_eq!(result.amount_out(), &BigUint::from(2u32));
        let params = QueryPoolSwapParams::new(
            x,
            y,
            SwapConstraint::PoolTargetPrice {
                target: Price::new(9u32.into(), 10u32.into()),
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );
        assert!(state.query_pool_swap(&params).is_err());
    }

    #[test]
    fn price_queries_preserve_bounds_zero_capacity_and_exact_equality() {
        use tycho_common::simulation::protocol_sim::{Price, SwapConstraint};
        for trade in [true, false] {
            for (min, max, expected) in [
                (None, None, 1_000_000u32),
                (Some(12u32), Some(20u32), 20),
                (None, Some(0), 0),
                (Some(1_000_001), None, 0),
            ] {
                let mut state = state();
                let constraint = if trade {
                    SwapConstraint::TradeLimitPrice {
                        limit: Price::new(1u32.into(), 1u32.into()),
                        tolerance: 0.0,
                        min_amount_in: min.map(BigUint::from),
                        max_amount_in: max.map(BigUint::from),
                    }
                } else {
                    SwapConstraint::PoolTargetPrice {
                        target: Price::new(1u32.into(), 1u32.into()),
                        tolerance: 0.0,
                        min_amount_in: min.map(BigUint::from),
                        max_amount_in: max.map(BigUint::from),
                    }
                };
                let params = QueryPoolSwapParams::new(
                    token(state.token_x, "X", 18),
                    token(state.token_y, "Y", 18),
                    constraint,
                );
                assert_eq!(
                    state
                        .query_pool_swap(&params)
                        .unwrap()
                        .amount_in(),
                    &BigUint::from(expected)
                );
                state.paused = true;
                assert_eq!(
                    state
                        .query_pool_swap(&params)
                        .unwrap()
                        .amount_in(),
                    &BigUint::ZERO
                );
            }
        }
        let state = rounding_boundary_state(true, 1);
        let params = QueryPoolSwapParams::new(
            token(state.token_x, "X", 18),
            token(state.token_y, "Y", 18),
            SwapConstraint::TradeLimitPrice {
                limit: Price::new(1u32.into(), 100u32.into()),
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: Some(2u32.into()),
            },
        );
        assert_eq!(
            state
                .query_pool_swap(&params)
                .unwrap()
                .amount_in(),
            &BigUint::ZERO
        );
    }

    #[test]
    fn price_queries_keep_interior_punishment_search() {
        use tycho_common::simulation::protocol_sim::{Price, SwapConstraint};
        let mut state = state();
        state.max_punishment_x24 = 1 << 20;
        let x = token(state.token_x, "X", 18);
        let y = token(state.token_y, "Y", 6);
        for trade in [true, false] {
            let constraint = if trade {
                SwapConstraint::TradeLimitPrice {
                    limit: Price::new(99u32.into(), 100u32.into()),
                    tolerance: 1e-6,
                    min_amount_in: Some(100u32.into()),
                    max_amount_in: Some(900_000u32.into()),
                }
            } else {
                SwapConstraint::PoolTargetPrice {
                    target: Price::new(99u32.into(), 100u32.into()),
                    tolerance: 1e-6,
                    min_amount_in: Some(100u32.into()),
                    max_amount_in: Some(900_000u32.into()),
                }
            };
            let params = QueryPoolSwapParams::new(x.clone(), y.clone(), constraint);
            let result = state.query_pool_swap(&params).unwrap();
            assert!(result.amount_in() > &BigUint::from(100u32));
            assert!(result.amount_in() < &BigUint::from(900_000u32));
            assert_eq!(
                &state
                    .get_amount_out(result.amount_in().clone(), &x, &y)
                    .unwrap()
                    .amount,
                result.amount_out()
            );
            if trade {
                assert!(result.amount_out() * 100u32 >= result.amount_in() * 99u32);
            }
        }
    }

    #[test]
    fn price_queries_reject_fallback_below_exact_trade_limit() {
        use tycho_common::simulation::protocol_sim::{Price, SwapConstraint};
        let mut state = state();
        state.max_punishment_x24 = 1 << 20;
        let scale = BigUint::from(10u32).pow(30);
        let params = QueryPoolSwapParams::new(
            token(state.token_x, "X", 18),
            token(state.token_y, "Y", 18),
            SwapConstraint::TradeLimitPrice {
                limit: Price::new(&scale * 99u32 + 1u32, &scale * 100u32),
                tolerance: 1e-6,
                min_amount_in: None,
                max_amount_in: None,
            },
        );
        // The generic f64 search rounds this limit to 0.99 and returns 320000 -> 316800.
        // That fill is strictly below the requested rational price.
        assert!(matches!(
            state.query_pool_swap(&params),
            Err(SimulationError::InvalidInput(message, _))
                if message == "LunarBase fill below trade limit price"
        ));
    }

    #[test]
    fn randomized_limits_are_executable_and_maximal() {
        // Fixed-seed xorshift keeps property coverage reproducible without a new dependency.
        let mut seed = 0x1445_cafe_7813_u64;
        let mut random = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for case in 0..6000 {
            let mut state = state();
            state.anchor_price_x96 = U256::from(random() % 17) << 94usize;
            state.reserve_x = (random() % 32) as u128;
            state.reserve_y = (random() % 32) as u128;
            state.fee_ask_x24 = (random() % (1 << 24)) as u32;
            state.fee_bid_x24 = (random() % (1 << 24)) as u32;
            state.max_punishment_x24 = (random() % (1 << 24)) as u32;
            if case % 5 == 0 {
                state.max_punishment_x24 = MAX_U24;
            }
            if case % 7 == 0 {
                state.fee_bid_x24 = MAX_U24;
            }
            if case % 11 == 0 {
                state.reserve_x = MAX_U112 - state.reserve_x;
            }
            if case % 13 == 0 {
                state.reserve_y = MAX_U112 - state.reserve_y;
            }
            if case % 17 == 0 {
                state.paused = true;
            }
            if case % 19 == 0 {
                state.head_block = 102;
            }
            for (sell, buy) in [(state.token_x, state.token_y), (state.token_y, state.token_x)] {
                let token_in = token(sell, "IN", 18);
                let token_out = token(buy, "OUT", 18);
                let (input, output) = state
                    .get_limits(sell.into(), buy.into())
                    .unwrap();
                if input != BigUint::ZERO {
                    let quoted = state
                        .get_amount_out(input.clone(), &token_in, &token_out)
                        .unwrap();
                    assert_eq!(quoted.amount, output, "case={case}");
                    assert_ne!(output, BigUint::ZERO, "case={case}");
                    assert!(
                        state
                            .get_amount_out(&input + 1u32, &token_in, &token_out)
                            .is_err(),
                        "case={case}"
                    );
                } else {
                    assert_eq!(output, BigUint::ZERO);
                }
                if state.reserve_x < 32 && state.reserve_y < 32 {
                    // At these anchors all possible fills fit in 512 input units.
                    // Exhaustive search independently checks zero limits as well as maxima.
                    let expected = (1..=512u64)
                        .rev()
                        .find_map(|amount| {
                            state
                                .get_amount_out(BigUint::from(amount), &token_in, &token_out)
                                .ok()
                                .map(|quote| (BigUint::from(amount), quote.amount))
                        })
                        .unwrap_or((BigUint::ZERO, BigUint::ZERO));
                    assert_eq!(
                        (input, output),
                        expected,
                        "case={case}, sell={sell:?}, state={state:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn quotes_x_to_y_and_transitions_reserves() {
        let state = state();
        let (amount_out, next_state) = state
            .quote_exact_in(state.token_x, state.token_y, U256::from(1_000u64))
            .unwrap();

        assert_eq!(amount_out, U256::from(1_000u64));
        assert_eq!(next_state.reserve_x, 1_001_000);
        assert_eq!(next_state.reserve_y, 999_000);
        assert_eq!(next_state.anchor_price_x96, state.anchor_price_x96);
        assert_eq!(next_state.head_block, state.head_block);
    }

    #[test]
    fn zero_amount_out_returns_zero_without_transition() {
        let state = state();
        let token_x = token(state.token_x, "ETH", 18);
        let token_y = token(state.token_y, "USDC", 6);

        let quote = state
            .get_amount_out(BigUint::ZERO, &token_x, &token_y)
            .unwrap();
        let next_state = quote
            .new_state
            .as_any()
            .downcast_ref::<LunarBaseState>()
            .unwrap();

        assert_eq!(quote.amount, BigUint::ZERO);
        assert_eq!(next_state, &state);
    }

    #[test]
    fn spot_price_uses_anchor_price() {
        let mut state = state();
        state.anchor_price_x96 = lunarbase_pmm_math::price_to_sqrt_price_x96(2_000.0 / 1e12);
        state.reserve_x = 1_000_000_000_000_000_000;
        state.reserve_y = 1_000_000;

        let token_x = token(state.token_x, "ETH", 18);
        let token_y = token(state.token_y, "USDC", 6);

        let price = state
            .spot_price(&token_x, &token_y)
            .unwrap();
        let inverse = state
            .spot_price(&token_y, &token_x)
            .unwrap();

        assert!((price - 2_000.0).abs() < 1e-6);
        assert!((inverse - 0.0005).abs() < 1e-12);
    }

    #[test]
    fn spot_price_applies_directional_base_fee() {
        let mut state = state();
        state.anchor_price_x96 = lunarbase_pmm_math::price_to_sqrt_price_x96(2_000.0 / 1e12);
        state.fee_ask_x24 = (1u32 << 24) / 100;
        state.fee_bid_x24 = (2 * (1u32 << 24)) / 100;

        let token_x = token(state.token_x, "ETH", 18);
        let token_y = token(state.token_y, "USDC", 6);

        let price = state
            .spot_price(&token_x, &token_y)
            .unwrap();
        let inverse = state
            .spot_price(&token_y, &token_x)
            .unwrap();

        assert!((price - (2_000.0 * 0.98)).abs() < 1e-3);
        assert!((inverse - (0.0005 * 0.99)).abs() < 1e-9);
    }

    #[test]
    fn rejects_stale_state() {
        let mut state = state();
        state.head_block = 102;

        let err = state
            .quote_exact_in(state.token_x, state.token_y, U256::from(1_000u64))
            .unwrap_err();

        assert_eq!(
            err,
            QuoteError::Stale { block_number: 102, latest_update_block: 100, block_delay: 2 }
        );
    }

    #[test]
    fn unavailable_pools_have_zero_limits_but_still_reject_swaps() {
        for (paused, head_block) in [(true, 100), (false, 102), (true, 103)] {
            let mut state = state();
            state.paused = paused;
            state.head_block = head_block;
            for (sell, buy) in [(state.token_x, state.token_y), (state.token_y, state.token_x)] {
                assert_eq!(
                    state
                        .get_limits(Bytes::from(sell), Bytes::from(buy))
                        .unwrap(),
                    (BigUint::ZERO, BigUint::ZERO)
                );
                assert!(state
                    .get_amount_out(
                        BigUint::from(1_000u64),
                        &token(sell, "IN", 18),
                        &token(buy, "OUT", 18),
                    )
                    .is_err());
            }
            assert!(state
                .get_limits(Bytes::from(addr(3)), Bytes::from(state.token_y))
                .is_err());
        }
    }

    #[test]
    fn limits_recover_after_unpause_or_fresh_operator_update() {
        let mut state = state();
        let sell = Bytes::from(state.token_x);
        let buy = Bytes::from(state.token_y);
        let available_limits = state
            .get_limits(sell.clone(), buy.clone())
            .unwrap();
        assert!(available_limits.0 > BigUint::ZERO);
        assert!(available_limits.1 > BigUint::ZERO);

        state.paused = true;
        assert_eq!(
            state
                .get_limits(sell.clone(), buy.clone())
                .unwrap()
                .0,
            BigUint::ZERO
        );
        apply_delta(&mut state, HashMap::from([("paused".to_owned(), Bytes::from([0u8]))]))
            .unwrap();
        assert_eq!(
            state
                .get_limits(sell.clone(), buy.clone())
                .unwrap(),
            available_limits
        );

        state.apply_block(&BlockContext::new(102, 0));
        assert_eq!(
            state
                .get_limits(sell.clone(), buy.clone())
                .unwrap()
                .0,
            BigUint::ZERO
        );
        apply_delta(
            &mut state,
            HashMap::from([("latest_update_block".to_owned(), Bytes::from(102u64))]),
        )
        .unwrap();
        assert_eq!(state.get_limits(sell, buy).unwrap(), available_limits);
    }

    #[test]
    fn charges_immediate_punishment_and_accumulates_only_the_traded_direction() {
        let mut state = state();
        state.max_punishment_x24 = 16_778;
        let amount = U256::from(100_000u64);

        let (first_out, first) = state
            .quote_exact_in(state.token_x, state.token_y, amount)
            .unwrap();
        assert_eq!(first_out, U256::from(99_995u64));
        assert_eq!((first.reserve_x, first.reserve_y), (1_100_000, 900_000));
        assert_eq!((first.fee_ask_x24, first.fee_bid_x24), (0, 839));
        assert_eq!(first.anchor_price_x96, state.anchor_price_x96);
        assert_eq!(first.latest_update_block, state.latest_update_block);

        let (second_out, second) = first
            .quote_exact_in(state.token_x, state.token_y, amount)
            .unwrap();
        assert_eq!(second_out, U256::from(99_990u64));
        assert_eq!((second.fee_ask_x24, second.fee_bid_x24), (0, 1_678));

        let (reverse_out, reverse) = second
            .quote_exact_in(state.token_y, state.token_x, amount)
            .unwrap();
        assert_eq!(reverse_out, U256::from(99_995u64));
        assert_eq!((reverse.fee_ask_x24, reverse.fee_bid_x24), (839, 1_678));
        assert_eq!((state.fee_ask_x24, state.fee_bid_x24), (0, 0));
    }

    #[test]
    fn operator_update_replaces_accumulated_fees_and_restores_freshness() {
        let mut initial = state();
        initial.max_punishment_x24 = 16_778;
        let (_, mut state) = initial
            .quote_exact_in(initial.token_x, initial.token_y, U256::from(100_000u64))
            .unwrap();
        state.apply_block(&BlockContext::new(102, 0));
        let reserves = (state.reserve_x, state.reserve_y);
        state
            .delta_transition(
                ProtocolStateDelta {
                    updated_attributes: HashMap::from([
                        (
                            "anchor_price_x96".to_owned(),
                            Bytes::from(
                                (U256::from(2u64) << 96usize).to_be_bytes::<32>()[12..].to_vec(),
                            ),
                        ),
                        ("fee_ask_x24".to_owned(), Bytes::from(11u32)),
                        ("fee_bid_x24".to_owned(), Bytes::from(22u32)),
                        ("latest_update_block".to_owned(), Bytes::from(102u64)),
                        ("block_number".to_owned(), Bytes::from(102u64)),
                    ]),
                    ..Default::default()
                },
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();

        assert!(state.is_fresh());
        assert_eq!((state.fee_ask_x24, state.fee_bid_x24), (11, 22));
        assert_eq!(state.max_punishment_x24, 16_778);
        assert_eq!((state.reserve_x, state.reserve_y), reserves);
    }

    #[test]
    fn full_fee_and_punishment_saturation_reject_without_changing_state() {
        let mut state = state();
        state.fee_bid_x24 = MAX_U24;
        let before = state.clone();
        assert_eq!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1u64)),
            Err(QuoteError::Rejected)
        );
        assert_eq!(state, before);
        assert_eq!(
            state
                .spot_price(&token(state.token_x, "X", 18), &token(state.token_y, "Y", 18))
                .unwrap(),
            0.0
        );

        state.fee_bid_x24 = MAX_U24 - 1;
        state.max_punishment_x24 = 1;
        let before = state.clone();
        assert_eq!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1u64)),
            Err(QuoteError::Rejected)
        );
        assert_eq!(state, before);
    }

    #[test]
    fn max_punishment_sentinel_uses_conceptual_one_hundred_percent() {
        let mut state = state();
        state.reserve_x = 0;
        state.reserve_y = 1_000_000;
        state.max_punishment_x24 = MAX_U24;
        assert_eq!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1_000_000u64)),
            Err(QuoteError::Rejected)
        );
    }

    #[test]
    fn rejects_uint112_reserve_overflow_and_invalid_math_state() {
        let mut state = state();
        state.reserve_x = lunarbase_pmm_math::MAX_U112;
        let before = state.clone();
        assert_eq!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1u64)),
            Err(QuoteError::ReserveOverflow)
        );
        assert_eq!(state, before);

        state.max_punishment_x24 = 1 << 24;
        assert_eq!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1u64)),
            Err(QuoteError::Math(MathError::MaxPunishmentExceedsUint24))
        );
    }

    #[test]
    fn quotes_anchor_above_uint128_and_checks_gross_liquidity() {
        let mut state = state();
        state.anchor_price_x96 = U256::from(1u64) << 129usize;
        state.reserve_y = 1u128 << 80;
        let expected_gross = U256::from(1u64) << 66usize;
        let (out, next) = state
            .quote_exact_in(state.token_x, state.token_y, U256::from(1u64))
            .unwrap();
        assert_eq!(out, expected_gross);
        assert_eq!(next.reserve_y, state.reserve_y - (1u128 << 66));
        state.reserve_y = (1u128 << 66) - 1;
        state.fee_bid_x24 = (1 << 24) / 2;
        assert_eq!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1u64)),
            Err(QuoteError::Rejected)
        );
    }

    #[test]
    fn execution_block_hook_expires_idle_pools_and_handles_reorgs() {
        let mut state = state();
        assert!(!state.apply_block(&BlockContext::new(101, 0)));
        assert_eq!(state.head_block, 101);
        assert!(state.is_fresh());

        assert!(state.apply_block(&BlockContext::new(102, 0)));
        assert!(!state.is_fresh());
        assert!(!state.apply_block(&BlockContext::new(102, 0)));
        assert!(!state.apply_block(&BlockContext::new(103, 0)));
        assert!(matches!(
            state.quote_exact_in(state.token_x, state.token_y, U256::from(1u64)),
            Err(QuoteError::Stale { block_number: 103, .. })
        ));

        assert!(state.apply_block(&BlockContext::new(101, 0)));
        assert!(state.is_fresh());
    }

    #[test]
    fn zero_anchor_never_returns_infinite_spot_price() {
        let mut state = state();
        state.anchor_price_x96 = U256::ZERO;
        let x = token(state.token_x, "X", 18);
        let y = token(state.token_y, "Y", 18);
        assert_eq!(state.spot_price(&x, &y).unwrap(), 0.0);
        assert_eq!(state.spot_price(&y, &x).unwrap(), 0.0);
        assert_eq!(
            state.quote_exact_in(state.token_y, state.token_x, U256::from(1u64)),
            Err(QuoteError::Rejected)
        );
    }

    #[test]
    fn limits_allow_both_directions_with_an_empty_input_reserve() {
        let mut state = state();
        state.anchor_price_x96 = U256::from(2u64) << 96usize;
        state.max_punishment_x24 = 16_778;

        state.reserve_x = 0;
        state.reserve_y = 4_000_000;
        let (input, output) = state
            .get_limits(Bytes::from(state.token_x), Bytes::from(state.token_y))
            .unwrap();
        assert_eq!(input, BigUint::from(1_000_000u64));
        assert_eq!(output, BigUint::from(3_996_000u64));

        state.reserve_x = 1_000_000;
        state.reserve_y = 0;
        let (input, output) = state
            .get_limits(Bytes::from(state.token_y), Bytes::from(state.token_x))
            .unwrap();
        // The last three input units still round down to the same gross output.
        assert_eq!(input, BigUint::from(4_000_003u64));
        assert_eq!(output, BigUint::from(999_000u64));
    }

    #[test]
    fn limits_recover_minimum_trade_for_empty_and_small_input_reserves() {
        for x_to_y in [true, false] {
            // Reserve 1 starts below the minimum; reserve 4 halves 8 -> 4 -> 2,
            // skipping the only valid input (3). Reserve 3 already finds it.
            for reserve_in in [0, 1, 3, 4] {
                let state = rounding_boundary_state(x_to_y, reserve_in);
                let (sell, buy) = if x_to_y {
                    (state.token_x, state.token_y)
                } else {
                    (state.token_y, state.token_x)
                };
                assert_eq!(
                    state
                        .quote_exact_in(sell, buy, U256::from(3u64))
                        .unwrap()
                        .0,
                    U256::from(1u64)
                );
                assert_eq!(
                    state
                        .get_limits(Bytes::from(sell), Bytes::from(buy))
                        .unwrap(),
                    (BigUint::from(3u64), BigUint::from(1u64)),
                    "x_to_y={x_to_y}, reserve_in={reserve_in}"
                );
            }
        }
    }

    fn rounding_boundary_state(x_to_y: bool, reserve_in: u128) -> LunarBaseState {
        let mut state = state();
        let q96 = U256::from(1u64) << 96usize;
        if x_to_y {
            state.anchor_price_x96 = q96 * U256::from(3u64) / U256::from(4u64);
            state.reserve_x = reserve_in;
            state.reserve_y = 1;
        } else {
            state.anchor_price_x96 = q96 * U256::from(4u64) / U256::from(3u64);
            state.reserve_x = 1;
            state.reserve_y = reserve_in;
        }
        state
    }

    #[test]
    fn minimum_trade_limits_apply_fees_and_punishment() {
        for x_to_y in [true, false] {
            for (fee, punishment, available) in
                [(1 << 22, 1 << 22, true), (MAX_U24, 0, false), (MAX_U24 - 1, MAX_U24, false)]
            {
                let mut state = rounding_boundary_state(x_to_y, 1);
                state.fee_ask_x24 = fee;
                state.fee_bid_x24 = fee;
                state.max_punishment_x24 = punishment;
                let (sell, buy) = if x_to_y {
                    (state.token_x, state.token_y)
                } else {
                    (state.token_y, state.token_x)
                };
                let expected = if available {
                    (BigUint::from(3u64), BigUint::from(1u64))
                } else {
                    (BigUint::ZERO, BigUint::ZERO)
                };
                assert_eq!(
                    state
                        .get_limits(Bytes::from(sell), Bytes::from(buy))
                        .unwrap(),
                    expected
                );
            }
        }
    }

    #[test]
    fn minimum_trade_limits_preserve_unavailable_states_and_math_errors() {
        for x_to_y in [true, false] {
            for case in [
                "paused",
                "stale",
                "no liquidity",
                "reserve overflow",
                "zero anchor",
                "minimum exceeds capacity",
                "output quantum exceeds liquidity",
                "invalid math",
            ] {
                let mut state = rounding_boundary_state(x_to_y, 1);
                let (sell, buy) = if x_to_y {
                    (state.token_x, state.token_y)
                } else {
                    (state.token_y, state.token_x)
                };
                match case {
                    "paused" => state.paused = true,
                    "stale" => state.head_block = 102,
                    "no liquidity" if x_to_y => state.reserve_y = 0,
                    "no liquidity" => state.reserve_x = 0,
                    "reserve overflow" if x_to_y => state.reserve_x = MAX_U112 - 2,
                    "reserve overflow" => state.reserve_y = MAX_U112 - 2,
                    "zero anchor" => state.anchor_price_x96 = U256::ZERO,
                    "minimum exceeds capacity" => {
                        state.anchor_price_x96 =
                            if x_to_y { U256::from(1u64) } else { U256::from(1u64) << 159usize };
                    }
                    "output quantum exceeds liquidity" => {
                        state.anchor_price_x96 =
                            U256::from(1u64) << if x_to_y { 97usize } else { 95usize };
                    }
                    "invalid math" => state.max_punishment_x24 = 1 << 24,
                    _ => unreachable!(),
                }
                let limits = state.get_limits(Bytes::from(sell), Bytes::from(buy));
                if case == "invalid math" {
                    assert!(limits.is_err(), "x_to_y={x_to_y}, {case}");
                } else {
                    assert_eq!(
                        limits.unwrap(),
                        (BigUint::ZERO, BigUint::ZERO),
                        "x_to_y={x_to_y}, {case}"
                    );
                }
            }
        }
    }

    #[test]
    fn empty_input_reserve_limit_inverts_both_rounding_steps() {
        for x_to_y in [true, false] {
            let mut state = state();
            let (sell, buy) = if x_to_y {
                state.anchor_price_x96 = (U256::from(3u64) << 96usize) / U256::from(4u64);
                state.reserve_x = 0;
                state.reserve_y = 1;
                (state.token_x, state.token_y)
            } else {
                state.anchor_price_x96 = (U256::from(3u64) << 96usize) / U256::from(2u64);
                state.reserve_x = 1;
                state.reserve_y = 0;
                (state.token_y, state.token_x)
            };
            assert_eq!(
                state
                    .quote_exact_in(sell, buy, U256::from(3u64))
                    .unwrap()
                    .0,
                U256::from(1u64)
            );
            let (input, output) = state
                .get_limits(Bytes::from(sell), Bytes::from(buy))
                .unwrap();
            assert_eq!(input, BigUint::from(if x_to_y { 3u64 } else { 4u64 }));
            assert_eq!(output, BigUint::from(1u64));
            assert_eq!(
                state.quote_exact_in(sell, buy, biguint_to_u256(&(input + 1u32)).unwrap()),
                Err(QuoteError::Rejected)
            );
        }
    }

    #[test]
    fn empty_input_reserve_limits_match_exhaustive_small_quotes() {
        for anchor_quarters in 1..=16u64 {
            for reserve_out in 1..=8u128 {
                for x_to_y in [true, false] {
                    let mut state = state();
                    state.anchor_price_x96 = U256::from(anchor_quarters) << 94usize;
                    let (sell, buy) = if x_to_y {
                        state.reserve_x = 0;
                        state.reserve_y = reserve_out;
                        (state.token_x, state.token_y)
                    } else {
                        state.reserve_x = reserve_out;
                        state.reserve_y = 0;
                        (state.token_y, state.token_x)
                    };
                    // These prices/reserves exhaust output liquidity below 256 input units.
                    let expected = (1..=256u64)
                        .rev()
                        .find_map(|input| {
                            state
                                .quote_exact_in(sell, buy, U256::from(input))
                                .ok()
                                .map(|(output, _)| (BigUint::from(input), u256_to_biguint(output)))
                        })
                        .unwrap_or((BigUint::ZERO, BigUint::ZERO));
                    assert_eq!(
                        state.get_limits(Bytes::from(sell), Bytes::from(buy)).unwrap(),
                        expected,
                        "anchor_quarters={anchor_quarters}, reserve_out={reserve_out}, x_to_y={x_to_y}"
                    );
                }
            }
        }
    }

    #[test]
    fn empty_input_reserve_limit_respects_uint112_capacity() {
        let mut state = state();
        state.anchor_price_x96 = U256::from(2u64) << 96usize;
        state.reserve_x = MAX_U112;
        state.reserve_y = 0;
        let (input, output) = state
            .get_limits(Bytes::from(state.token_y), Bytes::from(state.token_x))
            .unwrap();
        assert_eq!(input, BigUint::from(MAX_U112));
        assert_eq!(output, BigUint::from(MAX_U112 / 4));
    }

    #[test]
    fn legacy_caller_policy_does_not_change_partner_quotes() {
        let mut state = state();
        state.max_punishment_x24 = 16_778;
        state.fee_bid_x24 = (1 << 24) / 4;
        let x = token(state.token_x, "X", 18);
        let y = token(state.token_y, "Y", 18);
        let amount = U256::from(100_000u64);
        let expected = state
            .quote_exact_in(state.token_x, state.token_y, amount)
            .unwrap();
        assert_eq!(expected.0, U256::from(74_995u64));
        assert_eq!(state.spot_price(&x, &y).unwrap(), 0.75);

        apply_delta(
            &mut state,
            HashMap::from([
                ("quote_caller_whitelisted".to_owned(), Bytes::from([0u8])),
                ("blacklist_fee_multiplier".to_owned(), Bytes::from(U256::MAX.to_be_bytes::<32>())),
            ]),
        )
        .unwrap();
        assert_eq!(
            state
                .quote_exact_in(state.token_x, state.token_y, amount)
                .unwrap(),
            expected
        );
        assert_eq!(state.spot_price(&x, &y).unwrap(), 0.75);
    }

    #[test]
    fn matches_base_partner_quotes_from_execution_fixture() {
        let capture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../protocols/substreams/base-lunarbase/bootstrap/base-execution.json"
        )))
        .unwrap();
        let mut state = state();
        let address = |name: &str| {
            address_from_bytes(
                &hex::decode(
                    capture[name]
                        .as_str()
                        .unwrap()
                        .trim_start_matches("0x"),
                )
                .unwrap(),
            )
            .unwrap()
        };
        state.pool = address("pool");
        state.token_x = address("token_x");
        state.token_y = address("token_y");
        let snapshot = &capture["execution_state"];
        apply_delta(
            &mut state,
            snapshot["attributes"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, value)| {
                    (
                        name.clone(),
                        value
                            .as_str()
                            .unwrap()
                            .parse::<Bytes>()
                            .unwrap(),
                    )
                })
                .collect(),
        )
        .unwrap();
        state.head_block = snapshot["block_number"]
            .as_u64()
            .unwrap();
        let mut directions = [0; 2];
        for quote in capture["observed_quotes"]
            .as_array()
            .unwrap()
        {
            let forward = match quote["signature"].as_str().unwrap() {
                "quoteXToY(uint256)" => true,
                "quoteYToX(uint256)" => false,
                other => panic!("unexpected quote: {other}"),
            };
            directions[usize::from(forward)] += 1;
            let amount = U256::from_str_radix(quote["arguments"][0].as_str().unwrap(), 10).unwrap();
            let expected: Vec<U256> = quote["result"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| U256::from_str_radix(value.as_str().unwrap(), 10).unwrap())
                .collect();
            let (token_in, token_out) = if forward {
                (state.token_x, state.token_y)
            } else {
                (state.token_y, state.token_x)
            };
            let (output, next) = state
                .quote_exact_in(token_in, token_out, amount)
                .unwrap();
            assert!(output > U256::ZERO);
            assert_eq!(output, expected[0]);
            assert_eq!(next.anchor_price_x96, expected[1]);
            let gross = if forward {
                state.reserve_y - next.reserve_y
            } else {
                state.reserve_x - next.reserve_x
            };
            assert_eq!(U256::from(gross) - output, expected[2]);
        }
        assert_eq!(directions, [3, 3]);
    }

    #[test]
    fn derives_partner_quotes_from_base_capture_at_block_51297469() {
        // Recorded eth_call results at Base block 51,297,469, pool
        // 0x0000efc4ec03a7c47d3a38a9be7ff1d52dd01b99, with `from` set to
        // TychoRouter 0xAbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67.
        // The historical caller paid multiplier 100 at this block.
        // Normalize the recorded fee (floor(gross * fee_x24 / Q24) * 100) to
        // multiplier one. This checks partner math, not historical caller parity.
        let mut state = state();
        state.pool =
            address_from_bytes(&hex::decode("0000efc4ec03a7c47d3a38a9be7ff1d52dd01b99").unwrap())
                .unwrap();
        state.token_x = [0; 20];
        state.token_y =
            address_from_bytes(&hex::decode("833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap())
                .unwrap();
        state.anchor_price_x96 = U256::from(3_971_029_714_873_270_134_636_544u128);
        state.fee_ask_x24 = 1_835;
        state.fee_bid_x24 = 8_475;
        state.latest_update_block = 51_297_469;
        state.head_block = 51_297_469;
        state.max_punishment_x24 = 160_000;
        state.block_delay = 3;
        state.reserve_x = 9_683_337_552_179_635_497;
        state.reserve_y = 23_231_346_797;

        // Direction, input, observed net output, observed output-token fee.
        let quotes = [
            (true, 1_000_000_000_000_000u128, 2_385_158u128, 127_000u128),
            (true, 100_000_000_000_000_000, 237_258_990, 13_956_900),
            (true, 1_000_000_000_000_000_000, 2_258_700_200, 253_458_700),
            (false, 1_000_000, 393_700_695_234_017, 4_363_296_513_600),
            (false, 100_000_000, 39_291_060_402_299_481, 515_338_772_582_000),
            (false, 1_000_000_000, 385_726_231_079_417_225, 12_337_760_669_557_200),
        ];
        for (x_to_y, input, observed_out, observed_fee) in quotes {
            assert_eq!(observed_fee % 100, 0);
            let expected_fee = observed_fee / 100;
            let expected_out = observed_out + observed_fee - expected_fee;
            let (token_in, token_out) = if x_to_y {
                (state.token_x, state.token_y)
            } else {
                (state.token_y, state.token_x)
            };
            let (out, next) = state
                .quote_exact_in(token_in, token_out, U256::from(input))
                .unwrap();
            assert_eq!(out, U256::from(expected_out), "direction={x_to_y}, input={input}");
            let gross = if x_to_y {
                state.reserve_y - next.reserve_y
            } else {
                state.reserve_x - next.reserve_x
            };
            assert_eq!(gross - expected_out, expected_fee);
            assert_eq!(next.anchor_price_x96, state.anchor_price_x96);
        }
    }

    #[test]
    fn matches_bsc_router_quotes_observed_at_block_121835898() {
        // Recorded eth_call results at BSC block 121,835,898, pool
        // 0x00007904d186680c709519e71f4dc3e2df8f1b99, with `from` set to
        // 0x7f3d12bbafb8955e51b3ab9588b34c8ad95bda4e. Both tokens use 18
        // decimals; the observed caller multiplier was 1.
        let mut state = state();
        state.pool =
            address_from_bytes(&hex::decode("00007904d186680c709519e71f4dc3e2df8f1b99").unwrap())
                .unwrap();
        state.token_x = [0; 20];
        state.token_y =
            address_from_bytes(&hex::decode("55d398326f99059ff775485246999027b3197955").unwrap())
                .unwrap();
        state.anchor_price_x96 = U256::from(2_129_209_909_442_387_799_622_216_056_832u128);
        state.fee_ask_x24 = 3_092;
        state.fee_bid_x24 = 552;
        state.latest_update_block = 121_835_890;
        state.head_block = 121_835_898;
        state.max_punishment_x24 = 125_000;
        state.block_delay = 25;
        state.reserve_x = 40_982_882_360_761_506_608;
        state.reserve_y = 37_489_098_599_819_531_485_143;

        // Direction, input, observed net output, observed output-token fee.
        let quotes: [(bool, u128, u128, u128); 6] = [
            (true, 1_000_000_000_000_000, 722_209_919_138_098_675, 23_848_861_901_283),
            (true, 100_000_000_000_000_000, 72_220_419_368_930_290_024, 2_957_431_069_707_870),
            (true, 1_000_000_000_000_000_000, 722_152_061_971_320_131_445, 81_706_028_679_847_714),
            (false, 1_000_000_000_000_000_000, 1_384_337_907_652_450, 255_342_216_199),
            (false, 100_000_000_000_000_000_000, 138_432_263_993_751_885, 27_060_993_113_036),
            (false, 1_000_000_000_000_000_000_000, 1_384_184_240_164_873_034, 409_009_703_776_182),
        ];
        for (x_to_y, input, expected_out, expected_fee) in quotes {
            let (token_in, token_out) = if x_to_y {
                (state.token_x, state.token_y)
            } else {
                (state.token_y, state.token_x)
            };
            let (out, next) = state
                .quote_exact_in(token_in, token_out, U256::from(input))
                .unwrap();
            assert_eq!(out, U256::from(expected_out), "direction={x_to_y}, input={input}");
            let gross = if x_to_y {
                state.reserve_y - next.reserve_y
            } else {
                state.reserve_x - next.reserve_x
            };
            assert_eq!(gross - expected_out, expected_fee);
            assert_eq!(next.anchor_price_x96, state.anchor_price_x96);
        }
    }
}
