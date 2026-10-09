use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use substreams::store::{StoreGet, StoreGetProto, StoreGetString};
use substreams_ethereum::pb::eth;
use tycho_substreams::prelude as tycho;

use crate::{
    biconomy::{attribute, attrs, calls, inventory_attribute, to_address, topic, topics, Address},
    modules::{config::Config, store_protocol_components::component_key, store_watched::watch_key},
    writes::executor_writes,
};

/// Emits the venue's state changes for the block:
/// - every executor storage word a board, anchor or pause event wrote, as it was stored;
/// - the venue's fee and maker list when the venue's storage changed, read over RPC;
/// - `available(token)` for every provider that was bound, filled through or moved tokens, read
///   over RPC at the end of the block.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    new_components: tycho::BlockTransactionProtocolComponents,
    component_store: StoreGetProto<tycho::ProtocolComponent>,
    watched: StoreGetString,
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
    let mut transactions = HashMap::<u64, tycho::Transaction>::new();
    let mut venue_tx = None;
    // Provider to the last transaction that may have changed its inventory.
    let mut providers = BTreeMap::<Address, u64>::new();

    for tx_components in &new_components.tx_components {
        let Some(tx) = tx_components.tx.as_ref() else { continue };
        let builder = builders
            .entry(tx.index)
            .or_insert_with(|| tycho::TransactionChangesBuilder::new(tx));
        for component in tx_components
            .components
            .iter()
            .filter(|component| component.id == component_id)
        {
            builder.add_protocol_component(component);
            venue_tx = Some(tx.index);
        }
        transactions.insert(tx.index, tx.clone());
    }

    let transfer = topic(topics::TRANSFER);
    let approval = topic(topics::APPROVAL);
    for trace in block.transactions() {
        let tx: tycho::Transaction = trace.into();
        let writes = executor_writes(trace, &config.executor);
        if !writes.attributes.is_empty() {
            let attributes = writes
                .attributes
                .into_iter()
                .map(|(name, value)| attribute(name, value))
                .collect();
            builders
                .entry(tx.index)
                .or_insert_with(|| tycho::TransactionChangesBuilder::new(&tx))
                .add_entity_change(&tycho::EntityChanges {
                    component_id: component_id.clone(),
                    attributes,
                });
            transactions.insert(tx.index, tx.clone());
        }
        for provider in writes.providers {
            providers.insert(provider, tx.index);
            transactions.insert(tx.index, tx.clone());
        }

        if trace
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| call.storage_changes.iter())
            .any(|change| change.address == config.venue)
        {
            venue_tx = Some(tx.index);
            transactions.insert(tx.index, tx.clone());
        }

        // Token movements in or out of a watched provider or vault.
        for (log, _) in trace.logs_with_calls() {
            if !config
                .tokens
                .iter()
                .any(|token| log.address == token)
            {
                continue;
            }
            let Some(first) = log.topics.first() else { continue };
            if first.as_slice() != transfer && first.as_slice() != approval {
                continue;
            }
            for party in log.topics.iter().skip(1).take(2) {
                let Some(provider) = watched
                    .get_last(watch_key(party.get(12..).unwrap_or_default()))
                    .and_then(|hex| hex::decode(hex.trim_start_matches("0x")).ok())
                    .and_then(|raw| to_address(&raw))
                else {
                    continue;
                };
                providers.insert(provider, tx.index);
                transactions.insert(tx.index, tx.clone());
            }
        }
    }

    if let Some(index) = venue_tx {
        let attributes = venue_attributes_at(&config.venue);
        if !attributes.is_empty() {
            builder_for(&mut builders, &transactions, index).add_entity_change(
                &tycho::EntityChanges { component_id: component_id.clone(), attributes },
            );
        }
    }

    let mut by_tx = BTreeMap::<u64, Vec<Address>>::new();
    for (provider, index) in providers {
        by_tx
            .entry(index)
            .or_default()
            .push(provider);
    }
    for (index, providers) in by_tx {
        let attributes = inventory_attributes(&providers, &config.tokens);
        builder_for(&mut builders, &transactions, index).add_entity_change(&tycho::EntityChanges {
            component_id: component_id.clone(),
            attributes,
        });
    }

    let changes = builders
        .into_values()
        .filter_map(tycho::TransactionChangesBuilder::build)
        .collect();
    Ok(tycho::BlockChanges { block: Some((&block).into()), changes, ..Default::default() })
}

fn builder_for<'a>(
    builders: &'a mut BTreeMap<u64, tycho::TransactionChangesBuilder>,
    transactions: &HashMap<u64, tycho::Transaction>,
    index: u64,
) -> &'a mut tycho::TransactionChangesBuilder {
    builders
        .entry(index)
        .or_insert_with(|| tycho::TransactionChangesBuilder::new(&transactions[&index]))
}

fn eth_call(calls: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<Option<Vec<u8>>> {
    use substreams_ethereum::pb::eth::rpc;
    let request = rpc::RpcCalls {
        calls: calls
            .into_iter()
            .map(|(to_addr, data)| rpc::RpcCall { to_addr, data })
            .collect(),
    };
    substreams_ethereum::rpc::eth_call(&request)
        .responses
        .into_iter()
        .map(|response| (!response.failed).then_some(response.raw))
        .collect()
}

/// The venue's fee and maker order, read at the end of the block.
fn venue_attributes_at(venue: &Address) -> Vec<tycho::Attribute> {
    let responses =
        eth_call(vec![(venue.to_vec(), calls::fee_bps()), (venue.to_vec(), calls::makers())]);
    let mut attributes = Vec::new();
    if let Some(Some(fee)) = responses.first() {
        if fee.len() == 32 {
            attributes.push(attribute(attrs::FEE_BPS.to_owned(), fee[30..].to_vec()));
        }
    }
    if let Some(makers) = responses
        .get(1)
        .and_then(|raw| raw.as_deref())
        .and_then(calls::decode_address_array)
    {
        attributes.push(attribute(attrs::MAKERS.to_owned(), makers.concat()));
    }
    attributes
}

/// `available(token)` of each provider for every venue token. A failing call reads as
/// unbounded, as the executor treats it.
fn inventory_attributes(providers: &[Address], tokens: &[Address]) -> Vec<tycho::Attribute> {
    let requests: Vec<(Address, Address)> = providers
        .iter()
        .flat_map(|provider| {
            tokens
                .iter()
                .map(move |token| (*provider, *token))
        })
        .collect();
    let responses = eth_call(
        requests
            .iter()
            .map(|(provider, token)| (provider.to_vec(), calls::available(token)))
            .collect(),
    );
    requests
        .iter()
        .zip(responses)
        .map(|((provider, token), response)| {
            let value = response
                .filter(|raw| raw.len() >= 32)
                .map(|raw| raw[..32].to_vec())
                .unwrap_or_else(|| vec![0xff; 32]);
            attribute(inventory_attribute(provider, token), value)
        })
        .collect()
}
