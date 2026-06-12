use std::collections::{HashMap, HashSet};

use tycho_common::{models::protocol::ProtocolComponentStateDelta, Bytes};

use crate::extractor::models::BlockChanges;

const USV3_MANDATORY_ATTRIBUTES: [&str; 3] = ["liquidity", "tick", "sqrt_price_x96"];
const USV2_MANDATORY_ATTRIBUTES: [&str; 2] = ["reserve0", "reserve1"];
static STABLE_SWAP_FACTORY: &[u8] = b"stable_swap_factory";
static PLAIN_POOL: &[u8] = b"plain_pool";

/// Post processor function that adds missing attributes to all new created components.
pub fn add_default_attributes(mut changes: BlockChanges, attributes: &[&str]) -> BlockChanges {
    for tx in &mut changes.txs_with_update {
        for c_id in tx.protocol_components.keys() {
            if let Some(state) = tx.state_updates.get_mut(c_id) {
                for mandatory_attr in attributes {
                    if !state
                        .updated_attributes
                        .contains_key(mandatory_attr.to_owned())
                    {
                        state
                            .updated_attributes
                            .insert(mandatory_attr.to_string(), Bytes::zero(32));
                    }
                }
            } else {
                let mut default_attr = HashMap::new();
                for mandatory_attr in attributes {
                    default_attr.insert(mandatory_attr.to_string(), Bytes::zero(32));
                }
                tx.state_updates.insert(
                    c_id.clone(),
                    ProtocolComponentStateDelta {
                        component_id: c_id.clone(),
                        updated_attributes: default_attr,
                        deleted_attributes: HashSet::new(),
                        created_attributes: HashSet::new(),
                    },
                );
            }
        }
    }
    changes
}

/// Trims the 0x000.. tokens of Curve stable swap plain pool protocol components within a block of
/// contract changes.
pub fn trim_curve_component_token(mut changes: BlockChanges) -> BlockChanges {
    for tx in &mut changes.txs_with_update {
        for component in tx.protocol_components.values_mut() {
            if let Some(factory_name) = component
                .static_attributes
                .get("factory_name")
            {
                if factory_name == STABLE_SWAP_FACTORY {
                    if let Some(pool_type) = component
                        .static_attributes
                        .get("pool_type")
                    {
                        if pool_type == PLAIN_POOL {
                            component
                                .tokens
                                .retain(|token| token != &Bytes::zero(20));
                        }
                    }
                }
            }
        }
    }
    changes
}

/// Post processor function that adds missing attributes to all new created uniswapV3 pools.
pub fn add_default_attributes_uniswapv3(changes: BlockChanges) -> BlockChanges {
    // TODO: Remove it while this is handled directly in the substreams modules.
    add_default_attributes(changes, &USV3_MANDATORY_ATTRIBUTES)
}

/// Post processor function that adds missing attributes to all new created uniswapV2 pools.
#[deprecated]
pub fn add_default_attributes_uniswapv2(changes: BlockChanges) -> BlockChanges {
    // TODO: Remove it while this is handled directly in the substreams modules.
    add_default_attributes(changes, &USV2_MANDATORY_ATTRIBUTES)
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use tycho_common::models::{
        blockchain::{Block, Transaction, TxWithChanges},
        protocol::ProtocolComponent,
        Chain,
    };

    use super::*;

    const BLOCK_HASH_0: &str = "0x98b4a4fef932b1862be52de218cc32b714a295fae48b775202361a6fa09b66eb";
    const CREATED_CONTRACT: &str = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";

    #[test]
    fn test_add_default_attributes() {
        // Test that uniswap V3 post processor insert mandatory attributes with a default value on
        // new pools detected.
        let changes = BlockChanges::new(
            "native:test".to_owned(),
            Chain::Ethereum,
            Block::new(
                0,
                Chain::Ethereum,
                BLOCK_HASH_0.parse().unwrap(),
                BLOCK_HASH_0.parse().unwrap(),
                "2020-01-01T01:00:00".parse().unwrap(),
            ),
            0,
            false,
            vec![TxWithChanges {
                tx: Transaction::new(
                    Bytes::zero(32),
                    BLOCK_HASH_0.parse().unwrap(),
                    Bytes::zero(20),
                    Some(Bytes::zero(20)),
                    10,
                ),
                state_updates: HashMap::from([(
                    CREATED_CONTRACT.to_string(),
                    ProtocolComponentStateDelta {
                        component_id: CREATED_CONTRACT.to_string(),
                        updated_attributes: HashMap::from([(
                            "tick".to_string(),
                            Bytes::from(1_u64.to_be_bytes()),
                        )]),
                        deleted_attributes: HashSet::new(),
                        ..Default::default()
                    },
                )]),
                protocol_components: HashMap::from([(
                    CREATED_CONTRACT.to_string(),
                    ProtocolComponent {
                        id: CREATED_CONTRACT.to_string(),
                        protocol_system: "test".to_string(),
                        protocol_type_name: "Pool".to_string(),
                        chain: Chain::Ethereum,
                        tokens: vec![
                            Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
                            Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
                        ],
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            }],
            Vec::new(),
        );

        let expected = BlockChanges::new(
            "native:test".to_owned(),
            Chain::Ethereum,
            changes.block.clone(),
            0,
            changes.revert,
            vec![TxWithChanges {
                tx: changes
                    .txs_with_update
                    .first()
                    .unwrap()
                    .tx
                    .clone(),
                state_updates: HashMap::from([(
                    CREATED_CONTRACT.to_string(),
                    ProtocolComponentStateDelta {
                        component_id: CREATED_CONTRACT.to_string(),
                        updated_attributes: HashMap::from([
                            ("tick".to_string(), Bytes::from(1_u64.to_be_bytes())),
                            ("sqrt_price_x96".to_string(), Bytes::zero(32)),
                            ("liquidity".to_string(), Bytes::zero(32)),
                        ]),
                        deleted_attributes: HashSet::new(),
                        ..Default::default()
                    },
                )]),
                protocol_components: changes
                    .txs_with_update
                    .first()
                    .unwrap()
                    .protocol_components
                    .clone(),
                ..Default::default()
            }],
            Vec::new(),
        );

        let updated_changes = add_default_attributes(changes, &USV3_MANDATORY_ATTRIBUTES);

        assert_eq!(updated_changes, expected);
    }

    #[test]
    fn test_add_default_attributes_no_new_pools() {
        // Test that uniswap V3 post processor does nothing when no new pools are detected.
        let changes = BlockChanges::new(
            "native:test".to_owned(),
            Chain::Ethereum,
            Block::new(
                0,
                Chain::Ethereum,
                BLOCK_HASH_0.parse().unwrap(),
                BLOCK_HASH_0.parse().unwrap(),
                "2020-01-01T01:00:00".parse().unwrap(),
            ),
            0,
            false,
            vec![TxWithChanges {
                tx: Transaction::new(
                    Bytes::zero(32),
                    BLOCK_HASH_0.parse().unwrap(),
                    Bytes::zero(20),
                    Some(Bytes::zero(20)),
                    10,
                ),
                state_updates: HashMap::from([(
                    CREATED_CONTRACT.to_string(),
                    ProtocolComponentStateDelta {
                        component_id: CREATED_CONTRACT.to_string(),
                        updated_attributes: HashMap::from([(
                            "tick".to_string(),
                            Bytes::from(1_u64.to_be_bytes()),
                        )]),
                        deleted_attributes: HashSet::new(),
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            }],
            Vec::new(),
        );

        let updated_changes = add_default_attributes(changes.clone(), &USV3_MANDATORY_ATTRIBUTES);

        assert_eq!(updated_changes, changes);
    }

    #[test]
    fn test_trim_curve_tokens() {
        let changes = BlockChanges::new(
            "native:test".to_owned(),
            Chain::Ethereum,
            Block::new(
                0,
                Chain::Ethereum,
                BLOCK_HASH_0.parse().unwrap(),
                BLOCK_HASH_0.parse().unwrap(),
                "2020-01-01T01:00:00".parse().unwrap(),
            ),
            0,
            false,
            vec![TxWithChanges {
                protocol_components: HashMap::from([(
                    CREATED_CONTRACT.to_string(),
                    ProtocolComponent {
                        id: CREATED_CONTRACT.to_string(),
                        protocol_system: "test".to_string(),
                        protocol_type_name: "Pool".to_string(),
                        chain: Chain::Ethereum,
                        tokens: vec![
                            Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
                            Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
                            Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
                            Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
                        ],
                        static_attributes: HashMap::from([
                            ("pool_type".to_string(), Bytes::from(PLAIN_POOL)),
                            ("factory_name".to_string(), Bytes::from(STABLE_SWAP_FACTORY)),
                        ]),
                        ..Default::default()
                    },
                )]),
                tx: Transaction::new(
                    Bytes::zero(32),
                    BLOCK_HASH_0.parse().unwrap(),
                    Bytes::zero(20),
                    Some(Bytes::zero(20)),
                    10,
                ),
                ..Default::default()
            }],
            Vec::new(),
        );

        let expected = BlockChanges::new(
            "native:test".to_owned(),
            Chain::Ethereum,
            Block::new(
                0,
                Chain::Ethereum,
                BLOCK_HASH_0.parse().unwrap(),
                BLOCK_HASH_0.parse().unwrap(),
                "2020-01-01T01:00:00".parse().unwrap(),
            ),
            0,
            false,
            vec![TxWithChanges {
                protocol_components: HashMap::from([(
                    CREATED_CONTRACT.to_string(),
                    ProtocolComponent {
                        id: CREATED_CONTRACT.to_string(),
                        protocol_system: "test".to_string(),
                        protocol_type_name: "Pool".to_string(),
                        chain: Chain::Ethereum,
                        tokens: vec![
                            Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
                            Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
                        ],
                        static_attributes: HashMap::from([
                            ("pool_type".to_string(), Bytes::from(PLAIN_POOL)),
                            ("factory_name".to_string(), Bytes::from(STABLE_SWAP_FACTORY)),
                        ]),
                        ..Default::default()
                    },
                )]),
                tx: Transaction::new(
                    Bytes::zero(32),
                    BLOCK_HASH_0.parse().unwrap(),
                    Bytes::zero(20),
                    Some(Bytes::zero(20)),
                    10,
                ),
                ..Default::default()
            }],
            Vec::new(),
        );

        let updated_changes = trim_curve_component_token(changes);

        assert_eq!(updated_changes, expected);
    }
}
