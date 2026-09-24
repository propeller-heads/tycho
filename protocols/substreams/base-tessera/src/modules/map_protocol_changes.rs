use crate::{common::*, config::DeploymentConfig};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use substreams::{
    pb::substreams::StoreDeltas,
    store::{StoreGet, StoreGetString},
};
use substreams_ethereum::pb::eth;
use tycho_substreams::{
    balances::aggregate_balances_changes, contract::extract_contract_changes_builder, prelude::*,
};

fn attribute(builder: &mut TransactionChangesBuilder, component: &str, name: &str, value: Vec<u8>) {
    builder.add_entity_change(&EntityChanges {
        component_id: component.into(),
        attributes: vec![Attribute { name: name.into(), value, change: ChangeType::Update.into() }],
    });
    builder.mark_component_as_updated(component);
}
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    pair_store: StoreGetString,
    treasury_store: StoreGetString,
    safety_store: StoreGetString,
    balance_store: StoreDeltas,
) -> Result<BlockChanges> {
    let config = DeploymentConfig::parse(&params)?;
    let known: HashSet<_> = pairs(&pair_store, "pairs")
        .into_iter()
        .collect();
    let owner = treasury_store
        .get_last("treasury")
        .map(hex::decode)
        .transpose()?
        .unwrap_or(config.treasury.clone());
    let mut changes = HashMap::new();
    let created: HashMap<_, _> = new_components
        .tx_components
        .iter()
        .flat_map(|g| {
            let index =
                g.tx.as_ref()
                    .expect("component transaction")
                    .index;
            g.components
                .iter()
                .map(move |c| (c.id.clone(), index))
        })
        .collect();
    for group in new_components.tx_components {
        let tx = group.tx.expect("component transaction");
        let builder = changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for c in group.components {
            builder.add_protocol_component(&c);
            attribute(builder, &c.id, "balance_owner", owner.clone());
        }
    }
    for (_, (tx, balances)) in aggregate_balances_changes(balance_store, deltas) {
        let builder = changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for values in balances.values() {
            for change in values.values() {
                builder.add_balance_change(change);
            }
        }
    }
    extract_contract_changes_builder(
        &block,
        |addr| addr == config.tesseraswap || addr == config.engine || known.contains(&id(addr)),
        &mut changes,
    );
    for tx in block.transactions() {
        for w in committed_writes(tx) {
            let pair_id = id(&w.address);
            let targets: Vec<_> = if w.address == config.engine || w.address == config.tesseraswap {
                known.iter().cloned().collect()
            } else if known.contains(&pair_id) {
                vec![pair_id]
            } else {
                continue;
            };
            let transaction: Transaction = tx.into();
            let builder = changes
                .entry(transaction.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&transaction));
            for pair in targets {
                if created
                    .get(&pair)
                    .is_some_and(|index| *index > transaction.index)
                {
                    continue;
                }
                builder.mark_component_as_updated(&pair);
                if w.address == config.tesseraswap {
                    if w.key == slot(0) {
                        attribute(builder, &pair, "engine", address(&w.new_value));
                    }
                    if w.key == slot(config.treasury_slot) {
                        attribute(builder, &pair, "balance_owner", address(&w.new_value));
                    }
                } else if w.address != config.engine {
                    let index = if w.key == IMPLEMENTATION_SLOT {
                        Some(0)
                    } else if w.key == slot(config.pair_lib_slot) {
                        Some(1)
                    } else if w.key == slot(config.pair_write_helper_slot) {
                        Some(2)
                    } else {
                        None
                    };
                    if let Some(i) = index {
                        if !zero(&w.new_value) {
                            attribute(
                                builder,
                                &pair,
                                &format!("stateless_contract_addr_{i}"),
                                id(&address(&w.new_value)).into_bytes(),
                            );
                        }
                    }
                }
            }
        }
    }
    // Always enforce persisted signals on emitted component updates. Fee-table writes can
    // occur without a Pair write, so include their transaction explicitly as a trigger.
    let fee_key = fee_tag_zero_slot();
    let helpers: HashSet<_> = known
        .iter()
        .filter_map(|pair| safety_store.get_last(format!("helper:{pair}")))
        .collect();
    for tx in block.transactions() {
        if tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
            .flat_map(|c| &c.storage_changes)
            .any(|w| w.key == fee_key.as_slice() && helpers.contains(&hex::encode(&w.address)))
        {
            let tx: Transaction = tx.into();
            changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        }
    }
    let epoch_changed = safety_store
        .get_last("engine")
        .is_some_and(|e| e != hex::encode(&config.engine));
    for (index, builder) in &mut changes {
        for pair in &known {
            if created
                .get(pair)
                .is_some_and(|created_index| created_index > index)
            {
                continue;
            }
            let fee = safety_store
                .get_last(format!("helper:{pair}"))
                .and_then(|helper| safety_store.get_last(format!("fee:0x{helper}")))
                .and_then(|value| hex::decode(value).ok());
            if epoch_changed
                || fee
                    .as_ref()
                    .is_some_and(|value| !zero(value))
            {
                builder.change_component_pause_state(pair, true);
            }
        }
    }
    let mut changes: Vec<_> = changes.into_iter().collect();
    changes.sort_by_key(|(index, _)| *index);
    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: changes
            .into_iter()
            .filter_map(|(_, b)| b.build())
            .collect(),
        storage_changes: vec![],
    })
}
