use crate::constants::{ETH_ADDRESS, RETH_ADDRESS, ROCKET_POOL_COMPONENT_ID, V1_4_UPGRADE_TX};
use anyhow::Result;
use substreams_ethereum::pb::eth;
use tycho_substreams::models::{
    BlockTransactionProtocolComponents, ImplementationType, ProtocolComponent,
    TransactionProtocolComponents,
};

/// Find and create all relevant protocol components.
///
/// As Rocket Pool has a single deposit pool that supports exchanging between ETH and rETH, we
/// emit a single hardcoded ProtocolComponent at the specified starting block, anchored to the
/// v1.4 upgrade transaction that activated all v1.4 contracts and settings.
///
/// We return early for all other blocks since ProtocolComponents only need to be emitted once.
#[substreams::handlers::map]
fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    if block.number != params.parse::<u64>()? {
        return Ok(BlockTransactionProtocolComponents { tx_components: vec![] });
    }

    let tx = block
        .transactions()
        .find(|tx| tx.hash == V1_4_UPGRADE_TX)
        .ok_or(anyhow::anyhow!("v1.4 upgrade tx not found in starting block"))?;

    let component = ProtocolComponent::new(ROCKET_POOL_COMPONENT_ID)
        .with_tokens(&[RETH_ADDRESS.to_vec(), ETH_ADDRESS.to_vec()])
        .as_swap_type("rocketpool_pool", ImplementationType::Custom);

    Ok(BlockTransactionProtocolComponents {
        tx_components: vec![TransactionProtocolComponents {
            tx: Some(tx.into()),
            components: vec![component],
        }],
    })
}
