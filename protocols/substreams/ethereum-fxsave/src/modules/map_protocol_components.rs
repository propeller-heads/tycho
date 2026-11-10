use alloy_primitives::Address;
use anyhow::{Context, Result};
use substreams_ethereum::pb::eth;
use tycho_substreams::{
    models::{ChangeType, FinancialType, ImplementationType, ProtocolComponent, ProtocolType},
    prelude::*,
};

use crate::config::Config;

/// Name of the static attribute listing the directions a component trades, 40 bytes each: the
/// token sold followed by the token bought. Solvers build graph edges from it.
pub const SWAP_DIRECTIONS: &str = "swap_directions";

/// Emits the fxSAVE deposit component in its configured creation transaction.
#[substreams::handlers::map]
pub fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    protocol_components(&Config::parse(&params)?, &block)
}

fn protocol_components(
    config: &Config,
    block: &eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    if block.number != config.creation_block {
        return Ok(BlockTransactionProtocolComponents { tx_components: vec![] });
    }

    let tx = block
        .transactions()
        .find(|tx| tx.hash == config.creation_tx.as_slice())
        .with_context(|| {
            format!("creation tx {} not found in block {}", config.creation_tx, block.number)
        })?;

    Ok(BlockTransactionProtocolComponents {
        tx_components: vec![TransactionProtocolComponents {
            tx: Some(tx.into()),
            components: vec![component(config)],
        }],
    })
}

/// The component deposits USDC or fxUSD into fxSP and the fxSP into fxSAVE. Leaving fxSAVE pays
/// out both stable tokens after a cooldown, so it only trades into fxSAVE.
fn component(config: &Config) -> ProtocolComponent {
    ProtocolComponent {
        id: config.component_id(),
        tokens: vec![config.usdc.to_vec(), config.fxusd.to_vec(), config.fxsave.to_vec()],
        contracts: vec![],
        static_att: vec![
            attribute(
                SWAP_DIRECTIONS,
                swap_directions(&[(config.usdc, config.fxsave), (config.fxusd, config.fxsave)]),
            ),
            attribute("fx_base_pool", config.fx_base_pool.to_vec()),
        ],
        change: ChangeType::Creation.into(),
        protocol_type: Some(ProtocolType {
            name: "fxsave".into(),
            financial_type: FinancialType::Swap.into(),
            attribute_schema: vec![],
            implementation_type: ImplementationType::Custom.into(),
        }),
    }
}

fn swap_directions(directions: &[(Address, Address)]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(directions.len() * 40);
    for (token_in, token_out) in directions {
        encoded.extend_from_slice(token_in.as_slice());
        encoded.extend_from_slice(token_out.as_slice());
    }
    encoded
}

fn attribute(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.into(), value, change: ChangeType::Creation.into() }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::hex;
    use substreams_ethereum::pb::eth::v2::{Block, TransactionTrace, TransactionTraceStatus};

    use super::*;
    use crate::config::tests::PARAMS;

    const CREATION_BLOCK: u64 = 26_349_181;
    const CREATION_TX: [u8; 32] =
        hex!("0763603bd2699b43a9ecdf3de8ceed345a98c5571cc4a98375cc4ce0c266b84d");

    fn block(number: u64, tx_hash: [u8; 32]) -> Block {
        let transaction = TransactionTrace {
            hash: tx_hash.to_vec(),
            index: 336,
            status: TransactionTraceStatus::Succeeded.into(),
            ..Default::default()
        };
        Block { number, transaction_traces: vec![transaction], ..Default::default() }
    }

    fn components(block: &Block) -> Result<BlockTransactionProtocolComponents> {
        protocol_components(&Config::parse(PARAMS).unwrap(), block)
    }

    /// Expected values from `integration_test.tycho.yaml`.
    #[test]
    fn test_creation_tx_creates_the_component() {
        let components = components(&block(CREATION_BLOCK, CREATION_TX)).unwrap();

        let [created] = components.tx_components.as_slice() else {
            panic!("expected one transaction, got {:?}", components.tx_components)
        };
        assert_eq!(
            created
                .tx
                .as_ref()
                .map(|tx| (tx.hash.as_slice(), tx.index)),
            Some((&CREATION_TX[..], 336))
        );
        let [component] = created.components.as_slice() else {
            panic!("expected one component, got {:?}", created.components)
        };
        assert_eq!(component.id, "0x7743e50f534a7f9f1791dde7dcd89f7783eefc39");
        assert_eq!(
            component.tokens,
            vec![
                hex!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").to_vec(),
                hex!("085780639cc2cacd35e474e71f4d000e2405d8f6").to_vec(),
                hex!("7743e50f534a7f9f1791dde7dcd89f7783eefc39").to_vec(),
            ]
        );
        assert_eq!(
            component.static_att,
            vec![
                attribute(
                    SWAP_DIRECTIONS,
                    hex!(
                        "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
                        "7743e50f534a7f9f1791dde7dcd89f7783eefc39"
                        "085780639cc2cacd35e474e71f4d000e2405d8f6"
                        "7743e50f534a7f9f1791dde7dcd89f7783eefc39"
                    )
                    .to_vec(),
                ),
                attribute(
                    "fx_base_pool",
                    hex!("65c9a641afceb9c0e6034e558a319488fa0fa3be").to_vec()
                ),
            ]
        );
        assert_eq!(component.change, i32::from(ChangeType::Creation));
        let protocol_type = component
            .protocol_type
            .as_ref()
            .unwrap();
        assert_eq!(protocol_type.name, "fxsave");
        assert_eq!(protocol_type.implementation_type, i32::from(ImplementationType::Custom));
    }

    #[test]
    fn test_creation_block_without_the_creation_tx_fails() {
        let error = components(&block(CREATION_BLOCK, [0x11; 32])).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("0x0763603bd2699b43a9ecdf3de8ceed345a98c5571cc4a98375cc4ce0c266b84d"),
            "error names the missing tx: {error}"
        );
    }

    #[test]
    fn test_other_blocks_create_nothing() {
        let components = components(&block(CREATION_BLOCK + 1, CREATION_TX)).unwrap();

        assert!(components.tx_components.is_empty());
    }
}
