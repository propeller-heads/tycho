use std::{
    any::Any,
    collections::HashMap,
    fmt,
    str::FromStr,
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
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use super::{client::Aqua0Client, models::Aqua0Level};
use crate::rfq::client::RFQClient;

#[derive(Clone, Serialize, Deserialize)]
pub struct Aqua0State {
    pub token0: Token,
    pub token1: Token,
    pub fee_units: u32,
    pub state_version: String,
    pub expires_at: u64,
    pub zero_for_one: Vec<Aqua0Level>,
    pub one_for_zero: Vec<Aqua0Level>,
    pub client: Aqua0Client,
}

impl fmt::Debug for Aqua0State {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Aqua0State")
            .field("token0", &self.token0)
            .field("token1", &self.token1)
            .field("state_version", &self.state_version)
            .field("zero_for_one_levels", &self.zero_for_one.len())
            .field("one_for_zero_levels", &self.one_for_zero.len())
            .finish_non_exhaustive()
    }
}

impl Aqua0State {
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

    fn parsed_points(levels: &[Aqua0Level]) -> Result<Vec<(BigUint, BigUint)>, SimulationError> {
        let mut points = levels
            .iter()
            .filter(|level| level.fully_supported)
            .map(|level| -> Result<_, SimulationError> {
                let amount_in = BigUint::from_str(&level.amount_in).map_err(|_| {
                    SimulationError::FatalError("Invalid Aqua0 amountIn in state".into())
                })?;
                let amount_out = BigUint::from_str(&level.amount_out).map_err(|_| {
                    SimulationError::FatalError("Invalid Aqua0 amountOut in state".into())
                })?;
                Ok((amount_in, amount_out))
            })
            .collect::<Result<Vec<_>, _>>()?;
        points.sort_by(|left, right| left.0.cmp(&right.0));
        points.dedup_by(|left, right| left.0 == right.0);
        if points.is_empty() {
            return Err(SimulationError::RecoverableError(
                "Aqua0 has no fully backed levels".into(),
            ));
        }
        Ok(points)
    }

    fn interpolate(
        &self,
        amount_in: &BigUint,
        levels: &[Aqua0Level],
    ) -> Result<BigUint, SimulationError> {
        let points = Self::parsed_points(levels)?;
        if amount_in == &BigUint::default() {
            return Ok(BigUint::default());
        }

        let mut previous_in = BigUint::default();
        let mut previous_out = BigUint::default();
        for (next_in, next_out) in &points {
            if next_in <= &previous_in || next_out < &previous_out {
                return Err(SimulationError::FatalError(
                    "Aqua0 RFQ levels are not monotonic".into(),
                ));
            }
            if amount_in <= next_in {
                let input_span = next_in - &previous_in;
                let output_span = next_out - &previous_out;
                return Ok(previous_out + ((amount_in - &previous_in) * output_span / input_span));
            }
            previous_in = next_in.clone();
            previous_out = next_out.clone();
        }

        let partial = GetAmountOutResult {
            amount: previous_out,
            gas: BigUint::from(350_000u64),
            new_state: self.clone_box(),
        };
        Err(SimulationError::InvalidInput(
            format!(
                "Aqua0 amount {} exceeds the largest fully backed level {}",
                amount_in, previous_in
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

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.ensure_fresh()?;
        let (levels, input, output) = self.direction(&base.address, &quote.address)?;
        let points = Self::parsed_points(levels)?;
        let (amount_in, amount_out) = &points[0];
        let input_human = amount_in
            .to_f64()
            .ok_or_else(|| SimulationError::FatalError("Aqua0 amountIn does not fit f64".into()))?
            / 10f64.powi(input.decimals as i32);
        let output_human = amount_out.to_f64().ok_or_else(|| {
            SimulationError::FatalError("Aqua0 amountOut does not fit f64".into())
        })? / 10f64.powi(output.decimals as i32);
        Ok(output_human / input_human)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        self.ensure_fresh()?;
        let (levels, _, _) = self.direction(&token_in.address, &token_out.address)?;
        Ok(GetAmountOutResult {
            amount: self.interpolate(&amount_in, levels)?,
            gas: BigUint::from(350_000u64),
            new_state: self.clone_box(),
        })
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.ensure_fresh()?;
        let (levels, _, _) = self.direction(&sell_token, &buy_token)?;
        Self::parsed_points(levels)?
            .last()
            .cloned()
            .ok_or_else(|| SimulationError::RecoverableError("No Aqua0 liquidity".into()))
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
                self.token0 == other.token0
                    && self.token1 == other.token1
                    && self.state_version == other.state_version
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
        Ok(self
            .client
            .request_binding_quote(&params)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tycho_common::models::Chain;

    use super::*;
    use crate::rfq::protocols::aqua0::{
        client::Aqua0Client,
        models::{Aqua0Market, Aqua0Range},
    };

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(&Bytes::from_str(address).unwrap(), symbol, decimals, 0, &[], Chain::Base, 100)
    }

    fn level(amount_in: &str, amount_out: &str) -> Aqua0Level {
        Aqua0Level {
            requested_amount_in: amount_in.into(),
            amount_in: amount_in.into(),
            amount_out: amount_out.into(),
            fully_supported: true,
            current_tick: 0,
            sqrt_price_x96: "1".into(),
            ranges: vec![Aqua0Range { tick_lower: -60, tick_upper: 60, liquidity: "10".into() }],
            route_plan: serde_json::Value::Null,
        }
    }

    fn state() -> Aqua0State {
        let market = Aqua0Market {
            pool_id: format!("0x{}", "11".repeat(32)),
            class_id: "1".into(),
            amount0_samples: vec!["100".into(), "200".into()],
            amount1_samples: vec!["100".into()],
        };
        Aqua0State {
            token0: token("0x4200000000000000000000000000000000000006", "WETH", 18),
            token1: token("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913", "USDC", 6),
            fee_units: 3000,
            state_version: "v1".into(),
            expires_at: u64::MAX,
            zero_for_one: vec![level("100", "300"), level("200", "500")],
            one_for_zero: vec![level("100", "25")],
            client: Aqua0Client::new(
                Chain::Base,
                "http://localhost/api/tycho/rfq".into(),
                market,
                "read".into(),
                "operator".into(),
                Duration::from_secs(5),
                Duration::from_secs(5),
            )
            .unwrap(),
        }
    }

    #[test]
    fn interpolates_cumulative_backend_levels_without_reimplementing_the_pool() {
        let state = state();
        let result = state
            .get_amount_out(BigUint::from(150u32), &state.token0, &state.token1)
            .unwrap();
        assert_eq!(result.amount, BigUint::from(400u32));
    }

    #[test]
    fn refuses_amounts_above_the_largest_fully_backed_level() {
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
}
