use std::{any::Any, collections::HashMap};

use chrono::NaiveDateTime;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{token::Token, Chain},
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::protocol::models::ProtocolComponent;

pub const NATIVE_WRAPPER_ID: &str = "native_wrapper";
const NATIVE_WRAPPER_PROTOCOL_SYSTEM: &str = "native_wrap";
const NATIVE_WRAPPER_PROTOCOL_TYPE: &str = "NativeWrapper";
const WRAP_GAS: u64 = 7_000;
const UNWRAP_GAS: u64 = 14_000;

/// Stateless 1:1 bridge between a chain's native token and its wrapped
/// counterpart (e.g. ETH ↔ WETH).
///
/// This component is auto-injected by `ProtocolStreamBuilder` so every
/// consumer automatically sees the bridge without manual wiring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeWrapperState {
    native_token: Token,
    wrapped_token: Token,
}

impl NativeWrapperState {
    pub fn new(chain: Chain) -> Self {
        Self { native_token: chain.native_token(), wrapped_token: chain.wrapped_native_token() }
    }

    /// Builds the `ProtocolComponent` metadata for stream injection.
    pub fn component(chain: Chain) -> ProtocolComponent {
        let native = chain.native_token();
        let wrapped = chain.wrapped_native_token();
        ProtocolComponent::new(
            Bytes::from(NATIVE_WRAPPER_ID.as_bytes()),
            NATIVE_WRAPPER_PROTOCOL_SYSTEM.to_string(),
            NATIVE_WRAPPER_PROTOCOL_TYPE.to_string(),
            chain,
            vec![native, wrapped],
            vec![],
            HashMap::new(),
            Bytes::default(),
            NaiveDateTime::default(),
        )
    }

    fn validate_tokens(&self, token_in: &Bytes, token_out: &Bytes) -> Result<(), SimulationError> {
        let valid_pair = (*token_in == self.native_token.address &&
            *token_out == self.wrapped_token.address) ||
            (*token_in == self.wrapped_token.address && *token_out == self.native_token.address);
        if !valid_pair {
            return Err(SimulationError::InvalidInput(
                format!(
                    "NativeWrapper only supports {} ↔ {}, got {} → {}",
                    self.native_token.address, self.wrapped_token.address, token_in, token_out,
                ),
                None,
            ));
        }
        Ok(())
    }
}

#[typetag::serde]
impl ProtocolSim for NativeWrapperState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.validate_tokens(&base.address, &quote.address)?;
        Ok(1.0)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        self.validate_tokens(&token_in.address, &token_out.address)?;
        let is_wrapping = token_in.address == self.native_token.address;
        let gas = if is_wrapping { WRAP_GAS } else { UNWRAP_GAS };
        Ok(GetAmountOutResult::new(amount_in, BigUint::from(gas), self.clone_box()))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.validate_tokens(&sell_token, &buy_token)?;
        Ok((BigUint::from(u128::MAX), BigUint::from(u128::MAX)))
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
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
            .downcast_ref::<NativeWrapperState>()
            .is_some_and(|o| {
                self.native_token == o.native_token && self.wrapped_token == o.wrapped_token
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eth_state() -> NativeWrapperState {
        NativeWrapperState::new(Chain::Ethereum)
    }

    fn native_token() -> Token {
        Chain::Ethereum.native_token()
    }

    fn wrapped_token() -> Token {
        Chain::Ethereum.wrapped_native_token()
    }

    #[test]
    fn test_fee_is_zero() {
        assert_eq!(eth_state().fee(), 0.0);
    }

    #[test]
    fn test_spot_price_is_one() {
        let state = eth_state();
        let price = state
            .spot_price(&native_token(), &wrapped_token())
            .expect("valid pair");
        assert_eq!(price, 1.0);

        let price = state
            .spot_price(&wrapped_token(), &native_token())
            .expect("valid pair");
        assert_eq!(price, 1.0);
    }

    #[test]
    fn test_get_amount_out_wrapping() {
        let state = eth_state();
        let amount = BigUint::from(1_000_000u64);
        let result = state
            .get_amount_out(amount.clone(), &native_token(), &wrapped_token())
            .expect("valid pair");
        assert_eq!(result.amount, amount);
        assert_eq!(result.gas, BigUint::from(WRAP_GAS));
    }

    #[test]
    fn test_get_amount_out_unwrapping() {
        let state = eth_state();
        let amount = BigUint::from(1_000_000u64);
        let result = state
            .get_amount_out(amount.clone(), &wrapped_token(), &native_token())
            .expect("valid pair");
        assert_eq!(result.amount, amount);
        assert_eq!(result.gas, BigUint::from(UNWRAP_GAS));
    }

    #[test]
    fn test_get_amount_out_invalid_pair() {
        let state = eth_state();
        let bogus = Token { address: Bytes::from("0xdead"), ..native_token() };
        let result = state.get_amount_out(BigUint::from(1u64), &bogus, &wrapped_token());
        assert!(result.is_err());
    }

    #[test]
    fn test_get_limits() {
        let state = eth_state();
        let (sell_limit, buy_limit) = state
            .get_limits(native_token().address, wrapped_token().address)
            .expect("valid pair");
        assert_eq!(sell_limit, BigUint::from(u128::MAX));
        assert_eq!(buy_limit, BigUint::from(u128::MAX));
    }

    #[test]
    fn test_spot_price_invalid_pair() {
        let state = eth_state();
        let bogus = Token { address: Bytes::from("0xdead"), ..native_token() };
        let result = state.spot_price(&bogus, &wrapped_token());
        assert!(result.is_err());
    }

    #[test]
    fn test_component_metadata() {
        let component = NativeWrapperState::component(Chain::Ethereum);
        assert_eq!(component.id, Bytes::from(NATIVE_WRAPPER_ID.as_bytes()));
        assert_eq!(component.protocol_system, "native_wrap");
        assert_eq!(component.protocol_type_name, "NativeWrapper");
    }
}
