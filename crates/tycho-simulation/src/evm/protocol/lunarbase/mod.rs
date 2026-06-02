use tycho_client::feed::BlockHeader;

use crate::evm::decoder::TychoStreamDecoder;

mod decoder;
pub mod state;

pub use state::LunarBaseTychoState;

pub const PROTOCOL_SYSTEM: &str = "lunarbase";

pub fn register_lunarbase_decoder(decoder: &mut TychoStreamDecoder<BlockHeader>) {
    decoder.register_decoder::<LunarBaseTychoState>(PROTOCOL_SYSTEM);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use lunarbase_pmm_math::U256;
    use num_bigint::BigUint;
    use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
    use tycho_common::{
        dto::{ProtocolComponent, ProtocolStateDelta, ResponseProtocolState},
        models::{token::Token, Chain},
        simulation::protocol_sim::{Balances, ProtocolSim},
        Bytes,
    };

    use super::{
        decoder::{decode_lunarbase_snapshot, encode_state},
        register_lunarbase_decoder,
        state::{Address, LunarBaseState, LunarBaseTychoState},
        PROTOCOL_SYSTEM,
    };
    use crate::{
        evm::decoder::TychoStreamDecoder,
        protocol::models::{DecoderContext, TryFromWithBlock},
    };

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
        let component_id = component_id(state.pool);
        ComponentWithState {
            state: ResponseProtocolState {
                component_id: component_id.clone(),
                attributes: encode_state(&state)
                    .into_iter()
                    .map(|(name, value)| (name, Bytes::from(value)))
                    .collect(),
                balances: HashMap::new(),
            }
            .into(),
            component: ProtocolComponent {
                id: component_id,
                protocol_system: PROTOCOL_SYSTEM.to_owned(),
                protocol_type_name: "lunarbase".to_owned(),
                chain: Chain::Base.into(),
                tokens: vec![
                    Bytes::from(state.token_x.to_vec()),
                    Bytes::from(state.token_y.to_vec()),
                ],
                contract_ids: Vec::new(),
                static_attributes: HashMap::new(),
                creation_tx: Bytes::zero(32),
                ..Default::default()
            }
            .into(),
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    fn component_id(pool: Address) -> String {
        address_to_hex(pool)
    }

    fn address_to_hex(address: Address) -> String {
        let mut out = String::with_capacity(42);
        out.push_str("0x");
        for byte in address {
            out.push(nibble_to_hex(byte >> 4));
            out.push(nibble_to_hex(byte & 0x0f));
        }
        out
    }

    fn nibble_to_hex(value: u8) -> char {
        match value {
            0..=9 => (b'0' + value) as char,
            10..=15 => (b'a' + value - 10) as char,
            _ => unreachable!("nibble is always <= 15"),
        }
    }

    #[test]
    fn registers_decoder_with_tycho_stream_decoder() {
        let mut decoder = TychoStreamDecoder::<BlockHeader>::new();
        register_lunarbase_decoder(&mut decoder);
    }

    #[test]
    fn builds_stable_component_id() {
        assert_eq!(component_id([0xab; 20]), "0xabababababababababababababababababababab");
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
    fn delta_transition_updates_head_block_from_tycho_block_info() {
        let mut state = LunarBaseTychoState { state: state(), head_block: 100 };
        let delta = ProtocolStateDelta {
            component_id: "component".to_owned(),
            updated_attributes: HashMap::from([(
                "block_number".to_owned(),
                Bytes::from(105u64.to_be_bytes().to_vec()),
            )]),
            deleted_attributes: Default::default(),
        };

        state
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();

        assert_eq!(state.head_block, 105);
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
