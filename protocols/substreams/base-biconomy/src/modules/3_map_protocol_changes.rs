use std::collections::BTreeMap;

use anyhow::Result;
use substreams::store::{StoreGet, StoreGetProto};
use substreams_ethereum::pb::eth;
use tycho_substreams::prelude as tycho;

use crate::{
    biconomy::{attribute, provider_attribute},
    modules::{config::Config, store_protocol_components::component_key},
    writes::Watch,
};

/// Emits the venue's state changes for the block, all from storage changes, with no RPC:
/// - every executor storage word a board, anchor or pause event wrote;
/// - the venue's fee and maker list slots;
/// - the configured providers' token balances and their vaults' balances and allowances.
///
/// The creation transaction also carries the provider configs and the bootstrap balances and
/// allowances, which describe token state at the end of the creation block, so token storage
/// is read from the following block on.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    new_components: tycho::BlockTransactionProtocolComponents,
    component_store: StoreGetProto<tycho::ProtocolComponent>,
) -> Result<tycho::BlockChanges> {
    let config = Config::parse(&params)?;
    let component_id = config.component_id();
    let created = new_components
        .tx_components
        .iter()
        .flat_map(|tx| tx.components.iter())
        .any(|component| component.id == component_id);
    if !created &&
        component_store
            .get_last(component_key(&component_id))
            .is_none()
    {
        return Ok(tycho::BlockChanges { block: Some((&block).into()), ..Default::default() });
    }

    let mut builders = BTreeMap::<u64, tycho::TransactionChangesBuilder>::new();
    for tx_components in &new_components.tx_components {
        let Some(tx) = tx_components.tx.as_ref() else { continue };
        for component in tx_components
            .components
            .iter()
            .filter(|component| component.id == component_id)
        {
            let builder = builders
                .entry(tx.index)
                .or_insert_with(|| tycho::TransactionChangesBuilder::new(tx));
            builder.add_protocol_component(component);
            let mut attributes: Vec<tycho::Attribute> = config
                .providers
                .iter()
                .map(|provider| attribute(provider_attribute(&provider.address), provider.encode()))
                .collect();
            attributes.extend(
                config
                    .bootstrap
                    .iter()
                    .map(|b| attribute(b.attribute.clone(), b.value.clone())),
            );
            builder.add_entity_change(&tycho::EntityChanges {
                component_id: component_id.clone(),
                attributes,
            });
        }
    }

    let watch = Watch::new(&config);
    for trace in block.transactions() {
        let writes = watch.writes(trace, &config, !created);
        if writes.is_empty() {
            continue;
        }
        let tx: tycho::Transaction = trace.into();
        builders
            .entry(tx.index)
            .or_insert_with(|| tycho::TransactionChangesBuilder::new(&tx))
            .add_entity_change(&tycho::EntityChanges {
                component_id: component_id.clone(),
                attributes: writes
                    .into_iter()
                    .map(|(name, value)| attribute(name, value))
                    .collect(),
            });
    }

    let changes = builders
        .into_values()
        .filter_map(tycho::TransactionChangesBuilder::build)
        .collect();
    Ok(tycho::BlockChanges { block: Some((&block).into()), changes, ..Default::default() })
}
