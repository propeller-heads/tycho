use tycho_client::feed::BlockHeader;

use crate::evm::decoder::TychoStreamDecoder;

mod decoder;
pub mod state;

pub use state::LunarBaseState;

pub const PROTOCOL_SYSTEM: &str = "lunarbase";

pub fn register_lunarbase_decoder(decoder: &mut TychoStreamDecoder<BlockHeader>) {
    decoder.register_decoder::<LunarBaseState>(PROTOCOL_SYSTEM);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use lunarbase_pmm_math::U256;
    use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
    use tycho_common::{
        dto::{ProtocolComponent, ProtocolStateDelta, ResponseProtocolState},
        models::Chain,
        simulation::protocol_sim::{Balances, ProtocolSim},
        Bytes,
    };

    use super::{
        decoder::{decode_lunarbase_snapshot, encode_state},
        register_lunarbase_decoder,
        state::{Address, LunarBaseState},
        PROTOCOL_SYSTEM,
    };
    use crate::{
        evm::decoder::TychoStreamDecoder,
        protocol::models::{DecoderContext, TryFromWithBlock},
    };

    fn addr(byte: u8) -> Address {
        [byte; 20]
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
            reserve_y: 2_000_000,
            max_punishment_x24: 0,
            blacklist_fee_multiplier: U256::from(1u64),
            quote_caller_whitelisted: false,
            block_delay: 2,
            paused: false,
            head_block: 100,
        }
    }

    fn snapshot(state: LunarBaseState) -> ComponentWithState {
        let component_id = component_id(state.pool);
        ComponentWithState {
            state: ResponseProtocolState {
                component_id: component_id.clone(),
                attributes: encode_state(&state),
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
        format!("0x{}", hex::encode(pool))
    }

    #[test]
    fn registers_decoder_with_tycho_stream_decoder() {
        let mut decoder = TychoStreamDecoder::<BlockHeader>::new(Chain::Ethereum);
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

        let mut expected = expected;
        expected.head_block = 0;
        assert_eq!(decoded, expected);
    }

    #[tokio::test]
    async fn try_from_with_block_uses_header_as_head_block() {
        let expected = state();
        let decoded = LunarBaseState::try_from_with_header(
            snapshot(expected.clone()),
            BlockHeader { number: 101, partial_block_index: Some(3), ..Default::default() },
            &HashMap::new(),
            &HashMap::new(),
            &DecoderContext::new(),
        )
        .await
        .unwrap();

        let mut expected = expected;
        expected.head_block = 101;
        assert_eq!(decoded, expected);
    }

    #[test]
    fn delta_transition_updates_head_block_from_tycho_block_info() {
        let mut state = state();
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
    fn snapshot_requires_punishment_configuration_instead_of_legacy_concentration() {
        let mut snapshot = snapshot(state());
        snapshot
            .state
            .attributes
            .remove("max_punishment_x24");
        snapshot
            .state
            .attributes
            .insert("concentration_k".to_owned(), Bytes::from(0u32));
        let error = decode_lunarbase_snapshot(&snapshot).unwrap_err();
        assert!(
            matches!(error, crate::protocol::errors::InvalidSnapshotError::MissingAttribute(name) if name == "max_punishment_x24")
        );
    }

    #[test]
    fn snapshot_preserves_full_width_anchor_and_punishment() {
        let mut expected = state();
        expected.anchor_price_x96 = (U256::from(1u64) << 159usize) + U256::from(123u64);
        expected.max_punishment_x24 = lunarbase_pmm_math::MAX_U24;
        let decoded = decode_lunarbase_snapshot(&snapshot(expected.clone())).unwrap();
        expected.head_block = 0;
        assert_eq!(decoded, expected);
    }

    #[test]
    fn snapshot_rejects_anchor_larger_than_wire_uint160() {
        let mut snapshot = snapshot(state());
        snapshot
            .state
            .attributes
            .insert("anchor_price_x96".to_owned(), Bytes::from(vec![1u8; 21]));
        assert!(decode_lunarbase_snapshot(&snapshot).is_err());
    }

    #[test]
    fn snapshot_requires_explicit_caller_fee_policy() {
        for name in ["blacklist_fee_multiplier", "quote_caller_whitelisted"] {
            let mut snapshot = snapshot(state());
            snapshot.state.attributes.remove(name);
            let error = decode_lunarbase_snapshot(&snapshot).unwrap_err();
            assert!(
                matches!(error, crate::protocol::errors::InvalidSnapshotError::MissingAttribute(missing) if missing == name)
            );
        }
    }
}
