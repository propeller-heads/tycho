// This module is used in integration tests as well
use std::{
    any::Any,
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use num_bigint::BigUint;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{
        protocol::{GetAmountOutParams, ProtocolComponent},
        token::Token,
    },
    simulation::{
        errors::{SimulationError, TransitionError},
        indicatively_priced::{IndicativelyPriced, SignedQuote},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::encoding::models::{default_token, Swap};

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct MockRFQState {
    #[serde(default)]
    pub quote_amount_in: Option<BigUint>,
    pub quote_amount_out: BigUint,
    pub quote_data: HashMap<String, Bytes>,
    /// What `get_amount_out` returns, standing in for the maker's price levels. `None` makes
    /// `get_amount_out` fail.
    #[serde(default)]
    pub level_amount_out: Option<BigUint>,
    /// How long `request_signed_quote` waits before it answers, like a network round trip.
    #[serde(default)]
    pub delay: Duration,
    /// The `origin` of every `request_signed_quote` call, in call order.
    #[serde(skip)]
    pub received_origins: Arc<Mutex<Vec<Option<Bytes>>>>,
}
#[typetag::serde]
impl ProtocolSim for MockRFQState {
    fn fee(&self) -> f64 {
        panic!("MockRFQState does not implement fee")
    }

    fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
        panic!("MockRFQState does not implement fee")
    }

    fn get_amount_out(
        &self,
        _amount_in: BigUint,
        _token_in: &Token,
        _token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let level_amount_out = self
            .level_amount_out
            .clone()
            .ok_or_else(|| {
                SimulationError::FatalError("MockRFQState has no level_amount_out".to_string())
            })?;
        Ok(GetAmountOutResult::new(
            level_amount_out,
            BigUint::ZERO,
            Box::new(MockRFQState::default()),
        ))
    }

    fn get_limits(
        &self,
        _sell_token: Bytes,
        _buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        panic!("MockRFQState does not implement fee")
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        panic!("MockRFQState does not implement fee")
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        panic!("MockRFQState does not implement fee")
    }

    fn as_any(&self) -> &dyn Any {
        panic!("MockRFQState does not implement fee")
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        panic!("MockRFQState does not implement fee")
    }

    fn eq(&self, _other: &dyn ProtocolSim) -> bool {
        panic!("MockRFQState does not implement fee")
    }

    fn as_indicatively_priced(&self) -> Result<&dyn IndicativelyPriced, SimulationError> {
        Ok(self)
    }
}

#[async_trait]
impl IndicativelyPriced for MockRFQState {
    async fn request_signed_quote(
        &self,
        params: GetAmountOutParams,
    ) -> Result<SignedQuote, SimulationError> {
        self.received_origins
            .lock()
            .unwrap()
            .push(params.origin.clone());
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(SignedQuote {
            base_token: params.token_in,
            quote_token: params.token_out,
            amount_in: self
                .quote_amount_in
                .clone()
                .unwrap_or(params.amount_in),
            amount_out: self.quote_amount_out.clone(),
            quote_attributes: self.quote_data.clone(),
        })
    }
}

/// Builds a Bebop swap whose signed quote arrives after `delay`.
pub fn delayed_bebop_swap(token_in: Bytes, token_out: Bytes, delay: Duration) -> Swap {
    bebop_swap(token_in, token_out, MockRFQState { delay, ..bebop_state() })
}

/// Builds a Bebop swap and returns the list that records the `origin` of each quote request.
pub fn recording_bebop_swap(
    token_in: Bytes,
    token_out: Bytes,
) -> (Swap, Arc<Mutex<Vec<Option<Bytes>>>>) {
    let state = bebop_state();
    let received_origins = state.received_origins.clone();
    (bebop_swap(token_in, token_out, state), received_origins)
}

fn bebop_state() -> MockRFQState {
    MockRFQState {
        quote_amount_in: None,
        quote_amount_out: BigUint::from(1_000u64),
        level_amount_out: None,
        quote_data: HashMap::from([
            ("calldata".to_string(), Bytes::from(vec![0x12, 0x34])),
            ("partial_fill_offset".to_string(), Bytes::from(12u64.to_be_bytes().to_vec())),
            ("tx_to".to_string(), Bytes::from("0xbbbbbBB520d69a9775E85b458C58c648259FAD5F")),
        ]),
        delay: Duration::ZERO,
        received_origins: Arc::default(),
    }
}

fn bebop_swap(token_in: Bytes, token_out: Bytes, state: MockRFQState) -> Swap {
    Swap::new(
        ProtocolComponent {
            id: "bebop-rfq".to_string(),
            protocol_system: "rfq:bebop".to_string(),
            ..Default::default()
        },
        default_token(token_in),
        default_token(token_out),
        BigUint::ZERO,
    )
    .with_estimated_amount_in(BigUint::from(1_000u64))
    .with_protocol_state(Arc::new(state))
}
