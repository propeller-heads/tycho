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
    let config = Config::parse(&params)?;
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
            components: vec![component(&config)],
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
    use super::*;
    use crate::config::tests::PARAMS;

    #[test]
    fn component_trades_only_into_fxsave() {
        let config = Config::parse(PARAMS).unwrap();

        let component = component(&config);

        let directions = &component
            .static_att
            .iter()
            .find(|attribute| attribute.name == SWAP_DIRECTIONS)
            .unwrap()
            .value;
        let pairs: Vec<(&[u8], &[u8])> = directions
            .chunks(40)
            .map(|direction| (&direction[..20], &direction[20..]))
            .collect();
        assert_eq!(
            pairs,
            vec![
                (config.usdc.as_slice(), config.fxsave.as_slice()),
                (config.fxusd.as_slice(), config.fxsave.as_slice()),
            ]
        );
        assert_eq!(component.tokens.len(), 3);
    }
}
