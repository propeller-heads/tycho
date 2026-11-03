use std::collections::HashMap;

use alloy_primitives::Address;
use anyhow::{Context, Result};
use substreams_ethereum::pb::eth;
use tycho_substreams::{
    models::{ChangeType, FinancialType, ImplementationType, ProtocolComponent, ProtocolType},
    prelude::*,
};

use crate::config::Config;

/// Emits the three hardcoded Sky components, each anchored to its creation transaction:
/// the DssLitePsm (DAI<->USDC), the UsdsPsmWrapper (USDS<->USDC) and the DaiUsds
/// converter (DAI<->USDS).
#[substreams::handlers::map]
pub fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let config = Config::parse(&params)?;

    let mut definitions = vec![];
    if block.number == config.psm_creation_block {
        definitions.push((
            &config.psm_creation_tx,
            component(config.psm_component_id(), config.dai, config.usdc, "psm"),
        ));
    }
    if block.number == config.wrapper_creation_block {
        definitions.push((
            &config.wrapper_creation_tx,
            component(config.wrapper_component_id(), config.usds, config.usdc, "psm_wrapper"),
        ));
    }
    if block.number == config.converter_creation_block {
        definitions.push((
            &config.converter_creation_tx,
            component(config.converter_component_id(), config.dai, config.usds, "converter"),
        ));
    }

    let mut tx_components = HashMap::new();
    for (creation_tx, comp) in definitions {
        let tx = block
            .transactions()
            .find(|tx| tx.hash == creation_tx.as_slice())
            .with_context(|| {
                format!("creation tx {creation_tx} not found in block {}", block.number)
            })?;

        tx_components
            .entry(tx.index)
            .or_insert_with(|| TransactionProtocolComponents {
                tx: Some(tx.into()),
                components: vec![],
            })
            .components
            .push(comp);
    }

    Ok(BlockTransactionProtocolComponents { tx_components: tx_components.into_values().collect() })
}

fn component(id: String, stable: Address, gem: Address, component_type: &str) -> ProtocolComponent {
    ProtocolComponent {
        id,
        tokens: vec![stable.to_vec(), gem.to_vec()],
        contracts: vec![],
        static_att: vec![
            Attribute {
                name: "component_type".into(),
                value: component_type.into(),
                change: ChangeType::Creation.into(),
            },
            // The gem is the token moved by `sellGem`/`buyGem` (USDC) or burned by
            // `usdsToDai` (USDS); the swap encoder derives the call direction from it.
            Attribute {
                name: "gem".into(),
                value: gem.to_vec(),
                change: ChangeType::Creation.into(),
            },
        ],
        change: ChangeType::Creation.into(),
        protocol_type: Some(ProtocolType {
            name: "sky".to_string(),
            financial_type: FinancialType::Psm.into(),
            attribute_schema: vec![],
            implementation_type: ImplementationType::Custom.into(),
        }),
    }
}
