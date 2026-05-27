use std::{any::Any, collections::HashMap};

use lunarbase_pmm_math::U256;
use num_bigint::BigUint;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
            SwapConstraint,
        },
    },
    Bytes,
};

use crate::{
    evm::decoder::TychoStreamDecoder,
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

mod attributes;
#[cfg(test)]
mod component;
mod decoder;
mod quote;
mod state;

use attributes::{attrs, AttributeError, AttributeMap};
#[cfg(test)]
use component::protocol_component;
#[cfg(test)]
use decoder::encode_state;
use decoder::{apply_delta, decode_state, StateDecodeError, StateDelta};
use quote::{quote_exact_in, QuoteError, QuoteRequest};
use state::{Address, LunarBaseState};

pub const PROTOCOL_SYSTEM: &str = "lunarbase";
const DEFAULT_GAS: u64 = 180_000;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LunarBaseTychoState {
    pub state: LunarBaseState,
    pub head_block: u64,
}

pub fn register_lunarbase_decoder(decoder: &mut TychoStreamDecoder<BlockHeader>) {
    decoder.register_decoder::<LunarBaseTychoState>(PROTOCOL_SYSTEM);
}

#[typetag::serde]
impl ProtocolSim for LunarBaseTychoState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let token_in = address_from_bytes(base.address.as_ref())?;
        let token_out = address_from_bytes(quote.address.as_ref())?;
        if token_in == self.state.token_x && token_out == self.state.token_y {
            return spot_from_reserves(self.state.reserve_x, self.state.reserve_y, base, quote);
        }
        if token_in == self.state.token_y && token_out == self.state.token_x {
            return spot_from_reserves(self.state.reserve_y, self.state.reserve_x, base, quote);
        }
        Err(SimulationError::InvalidInput("invalid LunarBase token pair".to_owned(), None))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let quote = quote_exact_in(
            &self.state,
            QuoteRequest {
                token_in: address_from_bytes(token_in.address.as_ref())?,
                token_out: address_from_bytes(token_out.address.as_ref())?,
                amount_in: biguint_to_u256(&amount_in)?,
                block_number: self.head_block,
            },
        )
        .map_err(map_quote_error)?;

        Ok(GetAmountOutResult::new(
            u256_to_biguint(quote.amount_out),
            BigUint::from(DEFAULT_GAS),
            Box::new(Self { state: quote.next_state, head_block: self.head_block }),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let sell = address_from_bytes(sell_token.as_ref())?;
        let buy = address_from_bytes(buy_token.as_ref())?;
        if sell == self.state.token_x && buy == self.state.token_y {
            return Ok((BigUint::ZERO, BigUint::from(self.state.reserve_x)));
        }
        if sell == self.state.token_y && buy == self.state.token_x {
            return Ok((BigUint::ZERO, BigUint::from(self.state.reserve_y)));
        }
        Err(SimulationError::InvalidInput("invalid LunarBase token pair".to_owned(), None))
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        let state_delta = StateDelta {
            updated_attributes: delta
                .updated_attributes
                .into_iter()
                .map(|(key, value)| (key, value.to_vec()))
                .collect(),
            deleted_attributes: delta
                .deleted_attributes
                .into_iter()
                .collect(),
        };
        apply_delta(&mut self.state, &state_delta)
            .map_err(|err| TransitionError::DecodeError(format!("{err:?}")))
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { .. } | SwapConstraint::PoolTargetPrice { .. } => {
                Err(SimulationError::InvalidInput(
                    "LunarBase native simulator only supports exact-input get_amount_out"
                        .to_owned(),
                    None,
                ))
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
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

impl TryFromWithBlock<ComponentWithState, BlockHeader> for LunarBaseTychoState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let state = decode_lunarbase_snapshot(&snapshot)?;
        Ok(Self { state, head_block: block.number })
    }
}

pub fn decode_lunarbase_snapshot(
    snapshot: &ComponentWithState,
) -> Result<LunarBaseState, InvalidSnapshotError> {
    let mut attributes = AttributeMap::new();
    for (name, value) in snapshot.state.attributes.iter() {
        attributes.insert(name.clone(), value.to_vec());
    }

    let mut state = decode_state(&attributes).map_err(map_decode_error)?;
    state.pool = component_pool(snapshot)?;
    state.token_x = component_token(snapshot, 0)?;
    state.token_y = component_token(snapshot, 1)?;
    Ok(state)
}

fn component_pool(snapshot: &ComponentWithState) -> Result<Address, InvalidSnapshotError> {
    snapshot
        .component
        .static_attributes
        .get(attrs::POOL)
        .ok_or_else(|| InvalidSnapshotError::MissingAttribute(attrs::POOL.to_owned()))
        .and_then(|value| address_from_bytes(value.as_ref()).map_err(map_sim_error))
}

fn component_token(
    snapshot: &ComponentWithState,
    idx: usize,
) -> Result<Address, InvalidSnapshotError> {
    snapshot
        .component
        .tokens
        .get(idx)
        .map(|token| token.as_ref())
        .ok_or_else(|| InvalidSnapshotError::ValueError(format!("missing token index {idx}")))
        .and_then(|value| address_from_bytes(value).map_err(map_sim_error))
}

fn spot_from_reserves(
    reserve_in: u128,
    reserve_out: u128,
    token_in: &Token,
    token_out: &Token,
) -> Result<f64, SimulationError> {
    if reserve_in == 0 || reserve_out == 0 {
        return Err(SimulationError::RecoverableError("zero LunarBase reserve".to_owned()));
    }
    let decimals_adjustment = 10f64.powi(token_in.decimals as i32 - token_out.decimals as i32);
    Ok((reserve_out as f64 / reserve_in as f64) * decimals_adjustment)
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

fn map_decode_error(err: StateDecodeError) -> InvalidSnapshotError {
    match err {
        StateDecodeError::Attribute(AttributeError::Missing(name)) => {
            InvalidSnapshotError::MissingAttribute(name.to_string())
        }
        other => InvalidSnapshotError::ValueError(format!("{other:?}")),
    }
}

fn map_sim_error(err: SimulationError) -> InvalidSnapshotError {
    InvalidSnapshotError::ValueError(err.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use lunarbase_pmm_math::U256;
    use tycho_client::feed::synchronizer::ComponentWithState;
    use tycho_common::{
        dto::{ProtocolComponent, ResponseProtocolState},
        models::Chain,
        Bytes,
    };

    use super::*;

    fn addr(byte: u8) -> Address {
        [byte; 20]
    }

    fn address(hex: &str) -> Address {
        let hex = hex.strip_prefix("0x").unwrap_or(hex);
        assert_eq!(hex.len(), 40);
        let mut out = [0u8; 20];
        for i in 0..20 {
            out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
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
            anchor_price_x96: 1u128 << 96,
            fee_ask_x24: 0,
            fee_bid_x24: 0,
            latest_update_block: 100,
            reserve_x: 1_000_000,
            reserve_y: 2_000_000,
            concentration_k: 0,
            block_delay: 2,
            paused: false,
            blacklist_fee_multiplier: U256::from(1u64),
            executor_whitelisted: true,
        }
    }

    fn snapshot(state: LunarBaseState) -> ComponentWithState {
        let component = protocol_component(state.pool, state.token_x, state.token_y);
        ComponentWithState {
            state: ResponseProtocolState {
                component_id: component.id.clone(),
                attributes: encode_state(&state)
                    .into_iter()
                    .map(|(name, value)| (name, Bytes::from(value)))
                    .collect(),
                balances: HashMap::new(),
            }
            .into(),
            component: ProtocolComponent {
                id: component.id.clone(),
                protocol_system: PROTOCOL_SYSTEM.to_owned(),
                protocol_type_name: "lunarbase".to_owned(),
                chain: Chain::Base.into(),
                tokens: vec![
                    Bytes::from(state.token_x.to_vec()),
                    Bytes::from(state.token_y.to_vec()),
                ],
                contract_ids: component
                    .contract_addresses
                    .into_iter()
                    .map(Bytes::from)
                    .collect(),
                static_attributes: component
                    .static_attributes
                    .into_iter()
                    .map(|(name, value)| (name, Bytes::from(value)))
                    .collect(),
                creation_tx: Bytes::zero(32),
                ..Default::default()
            }
            .into(),
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    #[test]
    fn registers_decoder_with_tycho_stream_decoder() {
        let mut decoder = TychoStreamDecoder::<BlockHeader>::new();
        register_lunarbase_decoder(&mut decoder);
    }

    #[test]
    fn decodes_component_snapshot_into_lunarbase_state() {
        let expected = state();
        let decoded = decode_lunarbase_snapshot(&snapshot(expected.clone())).unwrap();

        assert_eq!(decoded, expected);
    }

    #[tokio::test]
    async fn try_from_with_block_uses_header_as_head_block() {
        let expected = state();
        let decoded = LunarBaseTychoState::try_from_with_header(
            snapshot(expected.clone()),
            BlockHeader { number: 101, partial_block_index: Some(3), ..Default::default() },
            &HashMap::new(),
            &HashMap::new(),
            &DecoderContext::new(),
        )
        .await
        .unwrap();

        assert_eq!(decoded.state, expected);
        assert_eq!(decoded.head_block, 101);
    }

    #[test]
    #[ignore = "manual live-state smoke test using a known Base LunarBase pool snapshot"]
    fn live_base_pool_quote_smoke_test() {
        let native = addr(0);
        let usdc = address("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913");
        let state = LunarBaseTychoState {
            state: LunarBaseState {
                pool: address("0x0000efc4ec03a7c47d3a38a9be7ff1d52dd01b99"),
                token_x: native,
                token_y: usdc,
                anchor_price_x96: u128::from_str_radix("000000000002ffb42f3bb2b1c0000000", 16)
                    .unwrap(),
                fee_ask_x24: u32::from_str_radix("000006f6", 16).unwrap(),
                fee_bid_x24: u32::from_str_radix("000021ba", 16).unwrap(),
                latest_update_block: 46_498_514,
                reserve_x: u128::from_str_radix("000000000000000091c69269d1d44388", 16).unwrap(),
                reserve_y: u128::from_str_radix("00000000000000000000000446add763", 16).unwrap(),
                concentration_k: 0,
                block_delay: 2,
                paused: false,
                blacklist_fee_multiplier: U256::from(1u64),
                executor_whitelisted: true,
            },
            head_block: 46_498_514,
        };

        let eth_token = token(native, "ETH", 18);
        let usdc_token = token(usdc, "USDC", 6);
        let amount_in = BigUint::from(10_000_000_000_000_000u64);
        let quote = state
            .get_amount_out(amount_in.clone(), &eth_token, &usdc_token)
            .unwrap();
        let next = quote
            .new_state
            .as_any()
            .downcast_ref::<LunarBaseTychoState>()
            .unwrap();

        assert!(quote.amount > BigUint::ZERO);
        assert!(quote.amount < BigUint::from(state.state.reserve_y));
        assert_eq!(next.state.reserve_x, state.state.reserve_x + 10_000_000_000_000_000u128);
        assert!(next.state.reserve_y < state.state.reserve_y);
        println!(
            "LunarBase live quote: 0.01 ETH -> {} USDC base units at block {}",
            quote.amount, state.head_block
        );
    }
}
