use std::{any::Any, collections::HashMap};

use alloy::primitives::U256;
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

use super::math::{get_limits, quote_buy_exact_in, quote_sell_exact_in, spot_price};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineCurve {
    pub blv: U256,
    pub circ: U256,
    pub supply: U256,
    pub swap_fee: U256,
    pub reserves: U256,
    pub total_supply: U256,
    pub convexity_exp: U256,
    pub last_invariant: U256,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineQuoteState {
    pub snapshot_curve: BaselineCurve,
    pub quote_block_buy_delta_circ: U256,
    pub quote_block_sell_delta_circ: U256,
    pub total_supply: U256,
    pub total_b_tokens: U256,
    pub total_reserves: U256,
    pub reserve_decimals: U256,
    pub liquidity_fee_pct: U256,
    pub pending_surplus: U256,
    pub should_settle_pending_surplus: bool,
    pub max_sell_delta: U256,
    pub snapshot_active_price: U256,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineState {
    pub component_id: Bytes,
    pub relay: Bytes,
    pub b_token: Token,
    pub reserve: Token,
    pub quote_state: BaselineQuoteState,
}

impl BaselineState {
    pub fn new(
        component_id: Bytes,
        relay: Bytes,
        b_token: Token,
        reserve: Token,
        quote_state: BaselineQuoteState,
    ) -> Self {
        Self { component_id, relay, b_token, reserve, quote_state }
    }
}

#[typetag::serde]
impl ProtocolSim for BaselineState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        if base.address == self.b_token.address && quote.address == self.reserve.address {
            spot_price(&self.quote_state, true)
        } else if base.address == self.reserve.address && quote.address == self.b_token.address {
            spot_price(&self.quote_state, false)
        } else {
            Err(SimulationError::FatalError(format!(
                "Invalid Baseline token pair: {}, {}",
                base.address, quote.address
            )))
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let quote = if token_in.address == self.reserve.address
            && token_out.address == self.b_token.address
        {
            quote_buy_exact_in(&self.quote_state, &amount_in)?
        } else if token_in.address == self.b_token.address
            && token_out.address == self.reserve.address
        {
            quote_sell_exact_in(&self.quote_state, &amount_in)?
        } else {
            return Err(SimulationError::FatalError(format!(
                "Invalid Baseline token pair: {}, {}",
                token_in.address, token_out.address
            )));
        };

        let mut new_state = self.clone();
        new_state.quote_state = quote.state;
        Ok(GetAmountOutResult {
            amount: quote.amount_out,
            gas: quote.gas,
            new_state: Box::new(new_state),
        })
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        if sell_token == self.reserve.address && buy_token == self.b_token.address {
            get_limits(&self.quote_state, true)
        } else if sell_token == self.b_token.address && buy_token == self.reserve.address {
            get_limits(&self.quote_state, false)
        } else {
            Err(SimulationError::FatalError(format!(
                "Invalid Baseline token pair: {}, {}",
                sell_token, buy_token
            )))
        }
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.quote_state =
            crate::evm::protocol::baseline::decoder::decode_quote_state(&delta.updated_attributes)
                .map_err(|err| TransitionError::DecodeError(err.to_string()))?;
        Ok(())
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
            .downcast_ref::<BaselineState>()
            .is_some_and(|other| self == other)
    }

    fn query_pool_swap(
        &self,
        params: &tycho_common::simulation::protocol_sim::QueryPoolSwapParams,
    ) -> Result<tycho_common::simulation::protocol_sim::PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::primitives::U256;
    use num_bigint::BigUint;
    use tycho_common::{
        models::{token::Token, Chain},
        simulation::{
            errors::SimulationError,
            protocol_sim::{Price, ProtocolSim, QueryPoolSwapParams, SwapConstraint},
        },
        Bytes,
    };

    use super::{BaselineCurve, BaselineQuoteState, BaselineState};
    use crate::evm::protocol::u256_num::biguint_to_u256;

    const BTOKEN: &str = "0x0000000000000000000000000000000000000002";
    const RESERVE: &str = "0x0000000000000000000000000000000000000001";
    const RELAY: &str = "0x0000000000000000000000000000000000000003";

    fn state() -> BaselineState {
        BaselineState::new(
            hex_bytes(BTOKEN),
            hex_bytes(RELAY),
            Token::new(&hex_bytes(BTOKEN), "bToken", 18, 0, &[Some(100_000)], Chain::Ethereum, 100),
            Token::new(&hex_bytes(RESERVE), "WETH", 18, 0, &[Some(100_000)], Chain::Ethereum, 100),
            quote_state(),
        )
    }

    fn quote_state() -> BaselineQuoteState {
        BaselineQuoteState {
            snapshot_curve: BaselineCurve {
                blv: u("2000000000000000000"),
                circ: u("500000000000000000000000"),
                supply: u("500000000000000000000000"),
                swap_fee: u("3000000000000000"),
                reserves: u("1500000000000000000000000"),
                total_supply: u("1000000000000000000000000"),
                convexity_exp: u("2000000000000000000"),
                last_invariant: u("500000000000000000000000"),
            },
            quote_block_buy_delta_circ: U256::ZERO,
            quote_block_sell_delta_circ: U256::ZERO,
            total_supply: u("1000000000000000000000000"),
            total_b_tokens: u("500000000000000000000000"),
            total_reserves: u("1500000000000000000000000"),
            reserve_decimals: U256::from(18),
            liquidity_fee_pct: u("1000000000000000000"),
            pending_surplus: U256::ZERO,
            should_settle_pending_surplus: false,
            max_sell_delta: u("100000000000000000000000"),
            snapshot_active_price: U256::ZERO,
        }
    }

    fn u(value: &str) -> U256 {
        biguint_to_u256(&value.parse::<BigUint>().unwrap())
    }

    fn hex_bytes(value: &str) -> Bytes {
        Bytes::from_str(value).unwrap()
    }

    #[test]
    fn get_amount_out_buys_btoken_and_returns_updated_state() {
        let state = state();
        let result = state
            .get_amount_out(BigUint::from(1_000_000_000_000_000u64), &state.reserve, &state.b_token)
            .unwrap();

        assert_eq!(result.amount, BigUint::from(166_333_851_409_557u64));
        let next = result
            .new_state
            .as_any()
            .downcast_ref::<BaselineState>()
            .unwrap();
        assert_eq!(
            next.quote_state
                .quote_block_buy_delta_circ,
            u("166333851409557")
        );
        assert_eq!(
            state
                .quote_state
                .quote_block_buy_delta_circ,
            U256::ZERO
        );
    }

    #[test]
    fn get_amount_out_sells_btoken_and_returns_updated_state() {
        let state = state();
        let result = state
            .get_amount_out(BigUint::from(166_333_998_522_065u64), &state.b_token, &state.reserve)
            .unwrap();

        assert_eq!(result.amount, BigUint::from(994_011_974_287_828u64));
        let next = result
            .new_state
            .as_any()
            .downcast_ref::<BaselineState>()
            .unwrap();
        assert_eq!(
            next.quote_state
                .quote_block_sell_delta_circ,
            u("166333998522065")
        );
    }

    #[test]
    fn rejects_invalid_get_amount_out_pair() {
        let state = state();
        let result = state.get_amount_out(BigUint::from(1u8), &state.reserve, &state.reserve);

        assert!(matches!(result.unwrap_err(), SimulationError::FatalError(_)));
    }

    #[test]
    fn spot_price_and_limits_are_available_for_both_directions() {
        let state = state();

        assert!(
            state
                .spot_price(&state.b_token, &state.reserve)
                .unwrap()
                > 0.0
        );
        assert!(
            state
                .spot_price(&state.reserve, &state.b_token)
                .unwrap()
                > 0.0
        );

        let buy_limits = state
            .get_limits(state.reserve.address.clone(), state.b_token.address.clone())
            .unwrap();
        assert!(buy_limits.0 > BigUint::from(0u8));
        assert!(buy_limits.1 > BigUint::from(0u8));

        let sell_limits = state
            .get_limits(state.b_token.address.clone(), state.reserve.address.clone())
            .unwrap();
        assert!(sell_limits.0 > BigUint::from(0u8));
        assert!(sell_limits.1 > BigUint::from(0u8));
    }

    #[test]
    fn query_pool_swap_returns_reachable_trade_limit_swap() {
        let state = state();
        let (max_in, _) = state
            .get_limits(state.reserve.address.clone(), state.b_token.address.clone())
            .unwrap();
        let max_quote = state
            .get_amount_out(max_in.clone(), &state.reserve, &state.b_token)
            .unwrap();
        let limit = Price::new(
            max_quote.amount.clone() * BigUint::from(101u8),
            max_in * BigUint::from(100u8),
        );

        let result = state
            .query_pool_swap(&QueryPoolSwapParams::new(
                state.reserve.clone(),
                state.b_token.clone(),
                SwapConstraint::TradeLimitPrice {
                    limit,
                    tolerance: 0.01,
                    min_amount_in: None,
                    max_amount_in: None,
                },
            ))
            .unwrap();

        assert!(result.amount_in() > &BigUint::from(0u8));
        assert!(result.amount_out() > &BigUint::from(0u8));
        assert!(result
            .new_state()
            .as_any()
            .downcast_ref::<BaselineState>()
            .is_some());
    }
}
