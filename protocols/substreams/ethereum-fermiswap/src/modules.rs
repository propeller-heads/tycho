use crate::{
    abi::fermi::events::{PairActiveSet, PairRegistered, PairUnregistered},
    pb::fermiswap::v1::Pair,
    utils::{component_id, Config, ACTIVE_ATTRIBUTE},
};
use anyhow::Result;
use ethabi::ethereum_types::Address;
use itertools::Itertools;
use std::collections::{HashMap, HashSet};
use substreams::{
    pb::substreams::StoreDeltas,
    prelude::*,
    store::{
        Appender, StoreAdd, StoreAddBigInt, StoreAppend, StoreGet, StoreGetBigInt, StoreGetString,
        StoreNew, StoreSetIfNotExists,
    },
};
use substreams_ethereum::{
    pb::eth::{
        self,
        v2::{Block, Log, TransactionTrace},
    },
    Event,
};
use substreams_helper::event_handler::EventHandler;
use tycho_substreams::{
    abi::erc20, balances::aggregate_balances_changes, contract::extract_contract_changes_builder,
    prelude::*,
};

#[substreams::handlers::map]
fn map_protocol_components(params: String, block: Block) -> Result<BlockEntityChanges> {
    let config: Config = serde_qs::from_str(params.as_str())?;
    let mut new_pair_changes: Vec<TransactionEntityChanges> = vec![];
    get_new_pairs(&config, &block, &mut new_pair_changes);
    Ok(BlockEntityChanges { block: Some((&block).into()), changes: new_pair_changes })
}

fn get_new_pairs(
    config: &Config,
    block: &Block,
    new_pair_changes: &mut Vec<TransactionEntityChanges>,
) {
    let mut on_pair_registered = |event: PairRegistered, _tx: &TransactionTrace, _log: &Log| {
        let tycho_tx: Transaction = _tx.into();
        let component_id = component_id(&event.base_asset, &event.quote_asset);
        let new_component = ProtocolComponent::new(component_id.as_str())
            .with_tokens(&[event.base_asset.as_slice(), event.quote_asset.as_slice()])
            .with_contracts(&[
                config.swapper_address.as_slice(),
                config.engine_address.as_slice(),
                config.trader_vault.as_slice(),
            ])
            .as_swap_type("fermiswap_pool", ImplementationType::Vm);

        new_pair_changes.push(TransactionEntityChanges {
            tx: Some(tycho_tx.clone()),
            entity_changes: vec![EntityChanges {
                component_id: component_id.clone(),
                attributes: vec![Attribute {
                    name: ACTIVE_ATTRIBUTE.to_string(),
                    value: vec![{ 0u8 }], // default false at creation
                    change: ChangeType::Creation.into(),
                }],
            }],
            component_changes: vec![new_component],
            balance_changes: vec![],
        });
    };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(vec![Address::from_slice(&config.engine_address)]);

    eh.on::<PairRegistered, _>(&mut on_pair_registered);
    eh.handle_events();
}

#[substreams::handlers::store]
fn store_pairs(pair_changes: BlockEntityChanges, store: StoreSetIfNotExistsProto<Pair>) {
    for tx_changes in pair_changes.changes {
        for component in tx_changes.component_changes {
            let pair = Pair {
                base_asset: component.tokens[0].clone(),
                quote_asset: component.tokens[1].clone(),
            };
            store.set_if_not_exists(0, &component.id, &pair);
        }
    }
}

#[substreams::handlers::store]
fn store_token_pairs(pair_changes: BlockEntityChanges, store: StoreAppend<String>) {
    for tx_changes in pair_changes.changes {
        for component in tx_changes.component_changes {
            for token in component.tokens {
                store.append(0, hex::encode(&token), component.id.clone());
            }
        }
    }
}

#[substreams::handlers::map]
fn map_token_balance_deltas(
    params: String,
    block: Block,
    token_pair_deltas: StoreDeltas,
    token_pairs_store: StoreGetString,
) -> Result<BlockBalanceDeltas> {
    let config: Config = serde_qs::from_str(params.as_str())?;
    let mut balance_deltas = Vec::new();
    let new_token_keys = token_pair_deltas
        .deltas
        .into_iter()
        .filter(|delta| delta.old_value.is_empty())
        .map(|delta| delta.key)
        .collect::<HashSet<_>>();
    let last_tx = block
        .transaction_traces
        .last()
        .map(Transaction::from);

    for token_key in &new_token_keys {
        let token = hex::decode(token_key)?;
        let Some(tx) = &last_tx else {
            continue;
        };
        let balance = erc20::functions::BalanceOf { owner: config.trader_vault.clone() }
            .call(token.clone())
            .unwrap_or_default();
        balance_deltas.push(BalanceDelta {
            ord: tx.index,
            tx: Some(tx.clone()),
            token,
            delta: balance.to_signed_bytes_be(),
            component_id: vec![],
        });
    }

    for tx in block.transactions() {
        for log in tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| &call.logs)
        {
            let Some(transfer) = erc20::events::Transfer::match_and_decode(log) else {
                continue;
            };
            let token_key = hex::encode(&log.address);
            if new_token_keys.contains(&token_key) ||
                token_pairs_store
                    .get_last(&token_key)
                    .is_none()
            {
                continue;
            }

            let delta =
                match (transfer.from == config.trader_vault, transfer.to == config.trader_vault) {
                    (true, false) => BigInt::zero() - transfer.value,
                    (false, true) => transfer.value,
                    _ => continue,
                };

            let tx: Transaction = tx.into();
            balance_deltas.push(BalanceDelta {
                ord: log.ordinal,
                tx: Some(tx),
                token: log.address.clone(),
                delta: delta.to_signed_bytes_be(),
                component_id: vec![],
            });
        }
    }

    balance_deltas.sort_unstable_by_key(|delta| delta.ord);
    Ok(BlockBalanceDeltas { balance_deltas })
}

#[substreams::handlers::store]
fn store_token_balances(token_balance_deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    let mut previous_ordinal = HashMap::<String, u64>::new();
    for delta in token_balance_deltas.balance_deltas {
        let token_key = hex::encode(&delta.token);
        previous_ordinal
            .entry(token_key.clone())
            .and_modify(|ord| {
                if *ord >= delta.ord {
                    panic!(
                        "Invalid ordinal sequence for token balance {token_key}: {} >= {}",
                        *ord, delta.ord
                    );
                }
                *ord = delta.ord;
            })
            .or_insert(delta.ord);

        store.add(delta.ord, token_key, BigInt::from_signed_bytes_be(&delta.delta));
    }
}

#[substreams::handlers::map]
fn map_balance_deltas(
    pair_changes: BlockEntityChanges,
    token_balance_deltas: BlockBalanceDeltas,
    token_balance_store: StoreGetBigInt,
    token_pairs_store: StoreGetString,
) -> Result<BlockBalanceDeltas> {
    let mut balance_deltas = Vec::new();
    let mut new_component_ids_by_token = HashMap::<Vec<u8>, HashSet<String>>::new();

    for tx_changes in pair_changes.changes {
        let Some(tx) = tx_changes.tx else {
            continue;
        };

        for component in tx_changes.component_changes {
            for token in component.tokens {
                new_component_ids_by_token
                    .entry(token.clone())
                    .or_default()
                    .insert(component.id.clone());

                let balance = token_balance_store
                    .get_last(hex::encode(&token))
                    .unwrap_or_else(BigInt::zero);
                balance_deltas.push(BalanceDelta {
                    ord: tx.index,
                    tx: Some(tx.clone()),
                    token,
                    delta: balance.to_signed_bytes_be(),
                    component_id: component.id.as_bytes().to_vec(),
                });
            }
        }
    }

    for token_delta in token_balance_deltas.balance_deltas {
        let token_key = hex::encode(&token_delta.token);
        let Some(component_ids) = token_pairs_store.get_last(&token_key) else {
            continue;
        };
        let new_component_ids = new_component_ids_by_token.get(&token_delta.token);

        for component_id in component_ids
            .split(';')
            .filter(|component_id| !component_id.is_empty())
            .unique()
        {
            if new_component_ids
                .map(|ids| ids.contains(component_id))
                .unwrap_or(false)
            {
                continue;
            }

            balance_deltas.push(BalanceDelta {
                ord: token_delta.ord,
                tx: token_delta.tx.clone(),
                token: token_delta.token.clone(),
                delta: token_delta.delta.clone(),
                component_id: component_id.as_bytes().to_vec(),
            });
        }
    }

    balance_deltas.sort_unstable_by_key(|delta| delta.ord);
    Ok(BlockBalanceDeltas { balance_deltas })
}

#[substreams::handlers::store]
pub fn store_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}

#[substreams::handlers::map]
fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    pair_changes: BlockEntityChanges,
    pair_store: StoreGetProto<Pair>,
    vault_balance_deltas: BlockBalanceDeltas,
    vault_balance_store_deltas: StoreDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let config: Config = serde_qs::from_str(params.as_str())?;
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    for tx_changes in pair_changes.changes {
        let Some(tycho_tx) = tx_changes.tx else {
            continue;
        };
        let builder = transaction_changes
            .entry(tycho_tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tycho_tx));

        for component in &tx_changes.component_changes {
            builder.add_protocol_component(component);
        }

        for entity_change in &tx_changes.entity_changes {
            builder.add_entity_change(entity_change);
        }
    }

    for trx in block.transactions() {
        let tx = Transaction {
            to: trx.to.clone(),
            from: trx.from.clone(),
            hash: trx.hash.clone(),
            index: trx.index.into(),
        };
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));

        for (log, _) in trx.logs_with_calls() {
            if log.address != config.engine_address {
                continue;
            }
            if let Some(ev) = PairActiveSet::match_and_decode(log) {
                let component_id = component_id(&ev.base_asset, &ev.quote_asset);
                if pair_store
                    .get_last(&component_id)
                    .is_some()
                {
                    builder.add_entity_change(&EntityChanges {
                        component_id,
                        attributes: vec![Attribute {
                            name: ACTIVE_ATTRIBUTE.to_string(),
                            value: vec![if ev.active { 1u8 } else { 0u8 }],
                            change: ChangeType::Creation.into(),
                        }],
                    });
                }
            } else if let Some(ev) = PairUnregistered::match_and_decode(log) {
                let component_id = component_id(&ev.base_asset, &ev.quote_asset);
                if pair_store
                    .get_last(&component_id)
                    .is_some()
                {
                    builder.add_entity_change(&EntityChanges {
                        component_id,
                        attributes: vec![Attribute {
                            name: ACTIVE_ATTRIBUTE.to_string(),
                            value: vec![{ 0u8 }],
                            change: ChangeType::Creation.into(),
                        }],
                    });
                }
            }
        }
    }

    aggregate_balances_changes(vault_balance_store_deltas, vault_balance_deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            let mut contract_change = InterimContractChange::new(&config.trader_vault, false);
            for token_balance_map in balances.values() {
                for balance_change in token_balance_map.values() {
                    contract_change
                        .upsert_token_balance(&balance_change.token, &balance_change.balance);
                    builder.add_balance_change(balance_change);
                }
            }

            builder.add_contract_changes(&contract_change);
        });

    extract_contract_changes_builder(
        &block,
        |addr| addr == config.engine_address || addr == config.swapper_address,
        &mut transaction_changes,
    );

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        storage_changes: vec![],
    })
}
