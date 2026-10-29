use anyhow::Result;
use substreams::store::{StoreGet, StoreGetProto};
use substreams_ethereum::pb::eth;
use tycho_substreams::prelude as tycho;

use crate::{
    biconomy::attribute,
    modules::{config::Config, store_protocol_components::component_key},
    writes::Watch,
};

/// Every write to the venue's fee and maker list, the providers' signer and executor, the
/// vaults' router and the tracked token storage, per transaction. The creation transaction also
/// carries the bootstrap values, which describe that state at the end of the creation block, so
/// token storage is read from the following block on.
///
/// `store_state` keeps these values so `map_protocol_changes` can check makers against the
/// venue's list and sum provider inventory into component balances.
#[substreams::handlers::map]
pub fn map_state_writes(
    params: String,
    block: eth::v2::Block,
    new_components: tycho::BlockTransactionProtocolComponents,
    component_store: StoreGetProto<tycho::ProtocolComponent>,
) -> Result<tycho::BlockChanges> {
    let config = Config::parse(&params)?;
    let component_id = config.component_id();
    let creation_tx = new_components
        .tx_components
        .iter()
        .find(|tx| {
            tx.components
                .iter()
                .any(|component| component.id == component_id)
        })
        .and_then(|tx| tx.tx.clone());
    if creation_tx.is_none() &&
        component_store
            .get_last(component_key(&component_id))
            .is_none()
    {
        // Past `bootstrap_block` without a component: the block had no transaction calling the
        // venue, or it is below the modules' initial block. Indexing nothing would look healthy.
        if block.number > config.bootstrap_block {
            return Err(anyhow::anyhow!(
                "Biconomy component {component_id} was not created at bootstrap_block {}: \
                 check that block calls the venue and is not below initialBlock",
                config.bootstrap_block
            ));
        }
        return Ok(tycho::BlockChanges { block: Some((&block).into()), ..Default::default() });
    }

    let mut changes = Vec::new();
    let watch = Watch::new(&config);
    for trace in block.transactions() {
        let mut attributes = Vec::new();
        match &creation_tx {
            Some(created) if u64::from(trace.index) < created.index => continue,
            Some(created) if u64::from(trace.index) == created.index => attributes.extend(
                config
                    .bootstrap_attributes()
                    .into_iter()
                    .map(|(name, value)| attribute(name, value)),
            ),
            _ => {}
        }
        attributes.extend(
            watch
                .state_writes(trace, creation_tx.is_none())
                .into_iter()
                .map(|(name, value)| attribute(name, value)),
        );
        if attributes.is_empty() {
            continue;
        }
        changes.push(tycho::TransactionChanges {
            tx: Some(trace.into()),
            entity_changes: vec![tycho::EntityChanges {
                component_id: component_id.clone(),
                attributes,
            }],
            ..Default::default()
        });
    }
    Ok(tycho::BlockChanges { block: Some((&block).into()), changes, ..Default::default() })
}
