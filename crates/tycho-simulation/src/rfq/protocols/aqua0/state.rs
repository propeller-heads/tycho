use std::{
    any::Any,
    collections::HashMap,
    str::FromStr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{protocol::GetAmountOutParams, token::Token},
    simulation::{
        errors::{SimulationError, TransitionError},
        indicatively_priced::{IndicativelyPriced, SignedQuote},
        protocol_sim::{
            Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
    Bytes,
};

use crate::{
    book::sim,
    rfq::protocols::aqua0::{
        client::{Aqua0Client, Aqua0Pool},
        models::Aqua0Level,
    },
};

/// Gas for one Aqua0 fill when the backend sends no measured estimate: `PoolManager.swap` while
/// the hook adds and removes each JIT range, fitted to Aqua0's Arbitrum fills since October 2026.
const SWAP_GAS_BASE: u64 = 500_000;
const SWAP_GAS_PER_RANGE: u64 = 200_000;

fn swap_gas(ranges: usize) -> BigUint {
    BigUint::from(SWAP_GAS_BASE) + BigUint::from(SWAP_GAS_PER_RANGE) * BigUint::from(ranges.max(1))
}

/// One fully backed sample: what the binding quote for `amount_in` signs, and how many JIT
/// ranges it draws on.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Point {
    amount_in: BigUint,
    amount_out: BigUint,
    gas: BigUint,
}

/// The fully backed levels of one direction, in rising input, checked to be monotonic.
fn points(levels: &[Aqua0Level]) -> Result<Vec<Point>, String> {
    let mut points = levels
        .iter()
        .filter(|level| level.fully_supported)
        .map(|level| {
            Ok(Point {
                amount_in: BigUint::from_str(&level.amount_in)
                    .map_err(|error| format!("invalid amountIn {}: {error}", level.amount_in))?,
                amount_out: BigUint::from_str(&level.amount_out)
                    .map_err(|error| format!("invalid amountOut {}: {error}", level.amount_out))?,
                gas: match &level.gas_estimate {
                    Some(gas) => BigUint::from_str(gas)
                        .map_err(|error| format!("invalid gasEstimate {gas}: {error}"))?,
                    None => swap_gas(level.ranges.len()),
                },
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    points.sort_by(|left, right| left.amount_in.cmp(&right.amount_in));
    points.dedup_by(|left, right| left.amount_in == right.amount_in);
    let mut previous = (BigUint::default(), BigUint::default());
    for point in &points {
        if point.amount_in == BigUint::default() || point.amount_out < previous.1 {
            return Err("levels are not monotonic".into());
        }
        previous = (point.amount_in.clone(), point.amount_out.clone());
    }
    Ok(points)
}

/// One Aqua0 market: a Uniswap V4 pool with Aqua0's hook, priced from the backend's sampled
/// exact-input fills. A binding quote reserves LP backing for one fill and signs its hook data.
#[derive(Clone, derive_more::Debug, Serialize, Deserialize)]
pub struct Aqua0State {
    pub(super) token0: Token,
    pub(super) token1: Token,
    pub(super) fee_units: u32,
    pub(super) pool: Aqua0Pool,
    pub(super) state_version: String,
    pub(super) expires_at: u64,
    #[debug(skip)]
    zero_for_one: Vec<Aqua0Level>,
    #[debug(skip)]
    one_for_zero: Vec<Aqua0Level>,
    #[serde(skip)]
    #[debug(skip)]
    client: Option<Arc<Aqua0Client>>,
}

impl Aqua0State {
    /// Refuses levels that cannot be read, so a state that exists always prices.
    #[expect(clippy::too_many_arguments, reason = "one field each, all from one answer")]
    pub(super) fn new(
        token0: Token,
        token1: Token,
        fee_units: u32,
        pool: Aqua0Pool,
        state_version: String,
        expires_at: u64,
        zero_for_one: Vec<Aqua0Level>,
        one_for_zero: Vec<Aqua0Level>,
        client: Arc<Aqua0Client>,
    ) -> Result<Self, String> {
        points(&zero_for_one)?;
        points(&one_for_zero)?;
        Ok(Self {
            token0,
            token1,
            fee_units,
            pool,
            state_version,
            expires_at,
            zero_for_one,
            one_for_zero,
            client: Some(client),
        })
    }

    /// The `(token sold, token bought)` pairs this market has a backed level for.
    pub(super) fn directions(&self) -> Vec<(Bytes, Bytes)> {
        sim::directions(
            &self.token0,
            &self.token1,
            self.zero_for_one
                .iter()
                .any(|level| level.fully_supported),
            self.one_for_zero
                .iter()
                .any(|level| level.fully_supported),
        )
    }

    fn ensure_fresh(&self) -> Result<(), SimulationError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| SimulationError::FatalError(error.to_string()))?
            .as_secs();
        if self.expires_at <= now {
            return Err(SimulationError::RecoverableError("Aqua0 RFQ snapshot is expired".into()));
        }
        Ok(())
    }

    fn direction(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<(&[Aqua0Level], &Token, &Token), SimulationError> {
        if token_in == &self.token0.address && token_out == &self.token1.address {
            Ok((&self.zero_for_one, &self.token0, &self.token1))
        } else if token_in == &self.token1.address && token_out == &self.token0.address {
            Ok((&self.one_for_zero, &self.token1, &self.token0))
        } else {
            Err(SimulationError::InvalidInput(
                format!("Aqua0 component does not quote {token_in} -> {token_out}"),
                None,
            ))
        }
    }

    fn backed_points(levels: &[Aqua0Level]) -> Result<Vec<Point>, SimulationError> {
        let points = points(levels).map_err(SimulationError::FatalError)?;
        if points.is_empty() {
            return Err(SimulationError::RecoverableError(
                "Aqua0 has no fully backed levels".into(),
            ));
        }
        Ok(points)
    }

    /// Linear between backed samples, and the gas of the sample at or above `amount_in`.
    fn interpolate(
        &self,
        amount_in: &BigUint,
        levels: &[Aqua0Level],
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let points = Self::backed_points(levels)?;
        if amount_in == &BigUint::default() {
            return Ok((BigUint::default(), points[0].gas.clone()));
        }
        let mut previous_in = BigUint::default();
        let mut previous_out = BigUint::default();
        let mut previous_gas = swap_gas(1);
        for point in &points {
            if amount_in <= &point.amount_in {
                let input_span = &point.amount_in - &previous_in;
                let output_span = &point.amount_out - &previous_out;
                let out = previous_out + ((amount_in - &previous_in) * output_span / input_span);
                return Ok((out, point.gas.clone()));
            }
            previous_in = point.amount_in.clone();
            previous_out = point.amount_out.clone();
            previous_gas = point.gas.clone();
        }
        let partial = GetAmountOutResult {
            amount: previous_out,
            gas: previous_gas,
            new_state: self.clone_box(),
        };
        Err(SimulationError::InvalidInput(
            format!(
                "Aqua0 amount {amount_in} exceeds the largest fully backed level {previous_in}"
            ),
            Some(partial),
        ))
    }
}

#[typetag::serde]
impl ProtocolSim for Aqua0State {
    fn fee(&self) -> f64 {
        self.fee_units as f64 / 1_000_000.0
    }

    /// The price of the smallest backed sample, fee included.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.ensure_fresh()?;
        let (levels, input, output) = self.direction(&base.address, &quote.address)?;
        let first = &Self::backed_points(levels)?[0];
        let human = |amount: &BigUint, decimals: u32| -> Result<f64, SimulationError> {
            let decimals = i32::try_from(decimals)
                .map_err(|error| SimulationError::FatalError(error.to_string()))?;
            Ok(amount.to_f64().ok_or_else(|| {
                SimulationError::FatalError("Aqua0 amount does not fit f64".into())
            })? / 10f64.powi(decimals))
        };
        Ok(human(&first.amount_out, output.decimals)? / human(&first.amount_in, input.decimals)?)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        self.ensure_fresh()?;
        let (levels, _, _) = self.direction(&token_in.address, &token_out.address)?;
        let (amount, gas) = self.interpolate(&amount_in, levels)?;
        Ok(GetAmountOutResult { amount, gas, new_state: self.clone_box() })
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.ensure_fresh()?;
        let (levels, _, _) = self.direction(&sell_token, &buy_token)?;
        let last = Self::backed_points(levels)?
            .pop()
            .ok_or_else(|| SimulationError::RecoverableError("No Aqua0 liquidity".into()))?;
        Ok((last.amount_in, last.amount_out))
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        self.ensure_fresh()?;
        let SwapConstraint::TradeLimitPrice { limit, min_amount_in, max_amount_in, .. } =
            params.swap_constraint()
        else {
            // Independent RFQ samples do not describe a post-trade marginal pool price.
            return Err(SimulationError::InvalidInput(
                "Aqua0 supports trade-price limits, not pool-price targets".into(),
                None,
            ));
        };
        let (capacity, _) =
            self.get_limits(params.token_in().address.clone(), params.token_out().address.clone())?;
        let maximum = max_amount_in
            .as_ref()
            .map_or_else(|| capacity.clone(), |bound| capacity.clone().min(bound.clone()));
        let minimum = min_amount_in
            .clone()
            .unwrap_or_default();
        if minimum > maximum || limit.denominator == BigUint::default() {
            return Err(SimulationError::InvalidInput("Invalid Aqua0 trade bounds".into(), None));
        }
        let quote =
            |amount: BigUint| self.get_amount_out(amount, params.token_in(), params.token_out());
        let clears = |amount: &BigUint, output: &BigUint| {
            output * &limit.denominator >= amount * &limit.numerator
        };
        let result = quote(maximum.clone())?;
        // RFQ liquidity ends at the last backed sample, which need not reach the price limit.
        if clears(&maximum, &result.amount) {
            return Ok(PoolSwap::new(maximum, result.amount, result.new_state, None));
        }
        let candidate = crate::evm::query_pool_swap::query_pool_swap(self, params)?;
        let amount = candidate
            .amount_in()
            .clone()
            .min(maximum);
        let result = quote(amount.clone())?;
        // The floating-point search has a tolerance; never return a quote below the exact
        // integer price limit or outside the caller's bounds.
        if amount < minimum || !clears(&amount, &result.amount) {
            return Err(SimulationError::InvalidInput(
                "No Aqua0 trade satisfies the requested bounds".into(),
                None,
            ));
        }
        Ok(PoolSwap::new(amount, result.amount, result.new_state, None))
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        Err(TransitionError::DecodeError(
            "Aqua0 RFQ state is snapshot-based and does not accept deltas".into(),
        ))
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
            .downcast_ref::<Aqua0State>()
            .is_some_and(|other| {
                self.token0 == other.token0 &&
                    self.token1 == other.token1 &&
                    self.pool == other.pool &&
                    self.state_version == other.state_version
            })
    }

    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Ok(self)
    }
}

#[async_trait]
impl IndicativelyPriced for Aqua0State {
    async fn request_signed_quote(
        &self,
        params: GetAmountOutParams,
    ) -> Result<SignedQuote, SimulationError> {
        self.ensure_fresh()?;
        Ok(sim::quoting_client(self.client.as_ref(), "Aqua0")?
            .request_binding_quote(&params, &self.pool)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tycho_common::{models::Chain, simulation::protocol_sim::Price};

    use super::*;
    use crate::rfq::protocols::aqua0::models::Aqua0Range;

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(&Bytes::from_str(address).unwrap(), symbol, decimals, 0, &[], Chain::Base, 100)
    }

    fn level(amount_in: &str, amount_out: &str, ranges: usize) -> Aqua0Level {
        Aqua0Level {
            amount_in: amount_in.into(),
            amount_out: amount_out.into(),
            fully_supported: true,
            ranges: vec![
                Aqua0Range { tick_lower: -60, tick_upper: 60, liquidity: "10".into() };
                ranges
            ],
            gas_estimate: None,
        }
    }

    fn state_with(zero_for_one: Vec<Aqua0Level>) -> Result<Aqua0State, String> {
        Aqua0State::new(
            token("0x4200000000000000000000000000000000000006", "WETH", 18),
            token("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913", "USDC", 6),
            3000,
            Aqua0Pool {
                component_id: "0x01".into(),
                chain_id: 8453,
                pool_id: format!("0x{}", "11".repeat(32)),
                class_id: "1".into(),
            },
            "v1".into(),
            u64::MAX,
            zero_for_one,
            vec![level("100", "25", 1)],
            Arc::new(Aqua0Client::new(
                "http://localhost",
                8453,
                Duration::from_secs(5),
                Duration::from_secs(5),
            )),
        )
    }

    fn state() -> Aqua0State {
        state_with(vec![level("100", "300", 1), level("200", "500", 2)]).unwrap()
    }

    #[test]
    fn interpolates_between_backed_samples() {
        let state = state();
        let result = state
            .get_amount_out(BigUint::from(150u32), &state.token0, &state.token1)
            .unwrap();
        assert_eq!(result.amount, BigUint::from(400u32));
    }

    #[test]
    fn gas_grows_with_the_ranges_the_fill_draws_on() {
        let state = state();
        let gas = |amount: u32| {
            state
                .get_amount_out(BigUint::from(amount), &state.token0, &state.token1)
                .unwrap()
                .gas
        };
        assert_eq!(gas(100), BigUint::from(700_000u32));
        assert_eq!(gas(150), BigUint::from(900_000u32));
    }

    #[test]
    fn a_measured_gas_estimate_wins_over_the_range_model() {
        let mut measured = level("100", "300", 1);
        measured.gas_estimate = Some("612345".into());
        let state = state_with(vec![measured]).unwrap();
        let result = state
            .get_amount_out(BigUint::from(100u32), &state.token0, &state.token1)
            .unwrap();
        assert_eq!(result.gas, BigUint::from(612_345u32));
    }

    #[test]
    fn refuses_amounts_above_the_largest_backed_level() {
        let state = state();
        assert!(matches!(
            state.get_amount_out(BigUint::from(201u32), &state.token0, &state.token1),
            Err(SimulationError::InvalidInput(_, Some(_)))
        ));
    }

    #[test]
    fn refuses_an_expired_snapshot() {
        let mut state = state();
        state.expires_at = 0;
        assert!(matches!(
            state.get_amount_out(BigUint::from(100u32), &state.token0, &state.token1),
            Err(SimulationError::RecoverableError(_))
        ));
    }

    #[test]
    fn malformed_levels_never_make_a_state() {
        assert!(state_with(vec![level("x", "1", 1)]).is_err());
        assert!(state_with(vec![level("100", "300", 1), level("200", "200", 1)]).is_err());
    }

    #[test]
    fn a_one_sided_market_names_its_direction() {
        let state = state_with(vec![]).unwrap();
        assert_eq!(state.directions(), vec![(state.token1.address.clone(), state.token0.address)]);
    }

    #[test]
    fn trade_depth_is_capped_by_backed_liquidity_and_caller_bounds() {
        let state = state();
        for (minimum, maximum, expected) in [
            (None, None, Some(200u32)),
            (None, Some(150u32), Some(150)),
            (Some(151u32), Some(150), None),
        ] {
            let params = QueryPoolSwapParams::new(
                state.token0.clone(),
                state.token1.clone(),
                SwapConstraint::TradeLimitPrice {
                    limit: Price::new(2u32.into(), 1u32.into()),
                    tolerance: 0.001,
                    min_amount_in: minimum.map(BigUint::from),
                    max_amount_in: maximum.map(BigUint::from),
                },
            );
            let result = state.query_pool_swap(&params);
            if let Some(expected) = expected {
                let result = result.unwrap();
                assert_eq!(result.amount_in(), &BigUint::from(expected));
                assert!(result.amount_out() >= &(result.amount_in() * 2u32));
            } else {
                assert!(result.is_err());
            }
        }
    }
}
