use crate::{
    abi::fermi::{events as fermi_events, functions as fermi_functions},
    pool_factories::{
        create_component, pair_id, pair_store_key, token_store_key, vault_balance_key,
        DeploymentConfig, PairState, ACTIVE_ATTRIBUTE, VAULT_COMPONENT_ID,
    },
};
use anyhow::{anyhow, Result};
use itertools::Itertools;
use std::collections::{HashMap, HashSet};
use substreams::{
    pb::substreams::StoreDeltas,
    prelude::*,
    store::{
        Appender, StoreAddBigInt, StoreAppend, StoreGet, StoreGetBigInt, StoreGetInt64,
        StoreGetString, StoreNew, StoreSet, StoreSetIfNotExists, StoreSetIfNotExistsInt64,
        StoreSetString,
    },
};
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::{
    abi::erc20, balances::aggregate_balances_changes, contract::extract_contract_changes_builder,
    prelude::*,
};

const END_ORDINAL: u64 = i64::MAX as u64;

#[substreams::handlers::map]
fn map_pair_changes(params: String, block: eth::v2::Block) -> Result<BlockChanges> {
    let config: DeploymentConfig = serde_qs::from_str(params.as_str())?;
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    if block.number == config.start_block {
        if let Some(tx) = initialization_transaction(&block, &config) {
            if let Some(pairs) = (fermi_functions::GetPairs {}).call(config.engine_address.clone())
            {
                for (base_asset, quote_asset, active) in pairs {
                    add_pair_change(
                        &mut transaction_changes,
                        &tx,
                        PairState::new(base_asset, quote_asset, active),
                        &config,
                        ChangeType::Creation,
                    );
                }
            }
        }
    }

    if block.number > config.start_block {
        for eth_tx in block.transactions() {
            let tx: Transaction = eth_tx.into();
            for log in eth_tx
                .calls
                .iter()
                .filter(|call| !call.state_reverted)
                .flat_map(|call| &call.logs)
                .filter(|log| log.address == config.engine_address)
            {
                if let Some(event) = fermi_events::PairRegistered::match_and_decode(log) {
                    add_pair_change(
                        &mut transaction_changes,
                        &tx,
                        PairState::new(event.base_asset, event.quote_asset, true),
                        &config,
                        ChangeType::Creation,
                    );
                } else if let Some(event) = fermi_events::PairUnregistered::match_and_decode(log) {
                    add_pair_change(
                        &mut transaction_changes,
                        &tx,
                        PairState::new(event.base_asset, event.quote_asset, false),
                        &config,
                        ChangeType::Update,
                    );
                } else if let Some(event) = fermi_events::PairActiveSet::match_and_decode(log) {
                    add_pair_change(
                        &mut transaction_changes,
                        &tx,
                        PairState::new(event.base_asset, event.quote_asset, event.active),
                        &config,
                        ChangeType::Update,
                    );
                }
            }
        }
    }

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect(),
        storage_changes: vec![],
    })
}

#[substreams::handlers::store]
fn store_pairs(pair_changes: BlockChanges, store: StoreSetString) {
    for tx_changes in pair_changes.changes {
        let active_by_id = active_by_component_id(&tx_changes.entity_changes);
        for component in tx_changes.component_changes {
            if component.tokens.len() < 2 {
                continue;
            }

            let active = active_by_id
                .get(&component.id)
                .copied()
                .unwrap_or(true);
            let pair =
                PairState::new(component.tokens[0].clone(), component.tokens[1].clone(), active);
            store.set(
                tx_changes
                    .tx
                    .as_ref()
                    .map(|tx| tx.index)
                    .unwrap_or_default(),
                pair_store_key(&component.id),
                &pair.encode(),
            );
        }
    }
}

#[substreams::handlers::map]
fn map_protocol_components(
    pair_changes: BlockChanges,
    pair_store_deltas: StoreDeltas,
) -> Result<BlockTransactionProtocolComponents> {
    let created_pairs = pair_store_deltas
        .deltas
        .into_iter()
        .filter(|delta| delta.old_value.is_empty())
        .filter_map(|delta| {
            delta
                .key
                .strip_prefix("pair:")
                .map(str::to_string)
        })
        .collect::<HashSet<_>>();

    Ok(BlockTransactionProtocolComponents {
        tx_components: pair_changes
            .changes
            .into_iter()
            .filter_map(|tx_changes| {
                let components = tx_changes
                    .component_changes
                    .into_iter()
                    .filter(|component| created_pairs.contains(&component.id))
                    .collect::<Vec<_>>();

                if components.is_empty() {
                    None
                } else {
                    Some(TransactionProtocolComponents { tx: tx_changes.tx, components })
                }
            })
            .collect(),
    })
}

#[substreams::handlers::store]
fn store_protocol_tokens(
    map_protocol_components: BlockTransactionProtocolComponents,
    store: StoreSetIfNotExistsInt64,
) {
    for tx_pc in map_protocol_components.tx_components {
        let ordinal = tx_pc
            .tx
            .as_ref()
            .map(|tx| tx.index)
            .unwrap_or_default();
        for component in tx_pc.components {
            for token in component.tokens {
                store.set_if_not_exists(ordinal, token_store_key(&token), &1);
            }
        }
    }
}

#[substreams::handlers::store]
fn store_token_pairs(
    map_protocol_components: BlockTransactionProtocolComponents,
    store: StoreAppend<String>,
) {
    for tx_pc in map_protocol_components.tx_components {
        let ordinal = tx_pc
            .tx
            .as_ref()
            .map(|tx| tx.index)
            .unwrap_or_default();
        for component in tx_pc.components {
            for token in component.tokens {
                store.append(ordinal, token_store_key(&token), component.id.clone());
            }
        }
    }
}

#[substreams::handlers::map]
fn map_vault_balance_deltas(
    params: String,
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
    token_deltas: StoreDeltas,
    protocol_tokens: StoreGetInt64,
) -> Result<BlockBalanceDeltas> {
    let config: DeploymentConfig = serde_qs::from_str(params.as_str())?;
    let mut balance_deltas = Vec::new();
    let new_token_keys = token_deltas
        .deltas
        .into_iter()
        .filter(|delta| delta.old_value.is_empty())
        .map(|delta| delta.key)
        .collect::<HashSet<_>>();

    for token_key in &new_token_keys {
        let token = hex::decode(token_key)?;
        let Some(tx) = tx_for_token(&new_components, &token) else {
            continue;
        };
        let balance = erc20::functions::BalanceOf { owner: config.trader_vault.clone() }
            .call(token.clone())
            .unwrap_or_else(BigInt::zero);
        balance_deltas.push(BalanceDelta {
            ord: tx.index,
            tx: Some(tx),
            token,
            delta: balance.to_signed_bytes_be(),
            component_id: VAULT_COMPONENT_ID.as_bytes().to_vec(),
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
            let token_key = token_store_key(&log.address);
            if new_token_keys.contains(&token_key) {
                continue;
            }
            if !protocol_tokens.has_last(&token_key) {
                continue;
            }

            let mut delta = BigInt::zero();
            if transfer.from == config.trader_vault {
                delta = delta - transfer.value.clone();
            }
            if transfer.to == config.trader_vault {
                delta = delta + transfer.value;
            }
            if delta.is_zero() {
                continue;
            }

            balance_deltas.push(BalanceDelta {
                ord: log.ordinal,
                tx: Some(tx.into()),
                token: log.address.clone(),
                delta: delta.to_signed_bytes_be(),
                component_id: VAULT_COMPONENT_ID.as_bytes().to_vec(),
            });
        }
    }

    balance_deltas.sort_unstable_by_key(|delta| delta.ord);
    Ok(BlockBalanceDeltas { balance_deltas })
}

#[substreams::handlers::store]
pub fn store_vault_token_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}

#[substreams::handlers::map]
fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    pair_changes: BlockChanges,
    new_components: BlockTransactionProtocolComponents,
    vault_balance_deltas: BlockBalanceDeltas,
    vault_balance_store: StoreGetBigInt,
    vault_balance_store_deltas: StoreDeltas,
    pairs_store: StoreGetString,
    token_pairs_store: StoreGetString,
) -> Result<BlockChanges, substreams::errors::Error> {
    let config: DeploymentConfig = serde_qs::from_str(params.as_str())?;
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();
    let mut new_component_ids = HashSet::new();

    for tx_component in new_components.tx_components {
        let tx = tx_component
            .tx
            .as_ref()
            .ok_or_else(|| anyhow!("component change without transaction"))?;
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        for component in tx_component.components {
            new_component_ids.insert(component.id.clone());
            builder.add_protocol_component(&component);
        }
    }

    for tx_changes in pair_changes.changes {
        let tx = tx_changes
            .tx
            .as_ref()
            .ok_or_else(|| anyhow!("pair change without transaction"))?;
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        let active_by_id = active_by_component_id(&tx_changes.entity_changes);

        for entity_change in &tx_changes.entity_changes {
            let mut entity_change = entity_change.clone();
            if !new_component_ids.contains(&entity_change.component_id) {
                for attribute in &mut entity_change.attributes {
                    attribute.change = ChangeType::Update.into();
                }
            }
            builder.add_entity_change(&entity_change);
        }

        for component in tx_changes.component_changes {
            let Some(active) = active_by_id.get(&component.id).copied() else {
                continue;
            };
            let Some(pair) = pair_from_component(&component, active) else {
                continue;
            };
            add_pair_balance_snapshot(builder, &pair, &vault_balance_store);
        }
    }

    aggregate_balances_changes(vault_balance_store_deltas, vault_balance_deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            let mut contract_change = InterimContractChange::new(&config.trader_vault, false);

            balances
                .values()
                .for_each(|token_balance_map| {
                    token_balance_map
                        .values()
                        .for_each(|balance_change| {
                            contract_change.upsert_token_balance(
                                &balance_change.token,
                                &balance_change.balance,
                            );
                            add_component_balances_for_token(
                                builder,
                                &balance_change.token,
                                &balance_change.balance,
                                &pairs_store,
                                &token_pairs_store,
                            );
                        });
                });

            builder.add_contract_changes(&contract_change);
        });

    extract_contract_changes_builder(
        &block,
        |addr| addr == config.engine_address,
        &mut transaction_changes,
    );

    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|call| !call.state_reverted && call.address == config.engine_address)
        {
            let updated_pair_ids = updated_pair_ids_from_call(call);
            if updated_pair_ids.is_empty() {
                continue;
            }
            let tx: Transaction = tx.into();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            for pair_id in updated_pair_ids {
                builder.mark_component_as_updated(&pair_id);
            }
        }
    }

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

fn initialization_transaction(
    block: &eth::v2::Block,
    config: &DeploymentConfig,
) -> Option<Transaction> {
    let configured_hash = config
        .init_tx_hash
        .as_ref()
        .and_then(|hash| hex::decode(hash.trim_start_matches("0x")).ok());

    if let Some(hash) = configured_hash {
        return block
            .transactions()
            .find(|tx| tx.hash == hash)
            .map(|tx| tx.into());
    }

    block
        .transactions()
        .next()
        .map(|tx| tx.into())
}

fn add_pair_change(
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
    tx: &Transaction,
    pair: PairState,
    config: &DeploymentConfig,
    change: ChangeType,
) {
    let builder = transaction_changes
        .entry(tx.index)
        .or_insert_with(|| TransactionChangesBuilder::new(tx));
    let mut component = create_component(&pair, config);
    component.change = change.into();
    builder.add_protocol_component(&component);
    builder.add_entity_change(&EntityChanges {
        component_id: component.id,
        attributes: vec![Attribute {
            name: ACTIVE_ATTRIBUTE.to_string(),
            value: vec![if pair.active { 1u8 } else { 0u8 }],
            change: change.into(),
        }],
    });
}

fn decode_register_pair(call: &eth::v2::Call) -> Option<PairState> {
    if !fermi_functions::RegisterPair::match_call(call) {
        return None;
    }
    fermi_functions::RegisterPair::decode(call)
        .ok()
        .map(|call| PairState::new(call.base_asset, call.quote_asset, true))
}

fn decode_unregister_pair(call: &eth::v2::Call) -> Option<PairState> {
    if !fermi_functions::UnregisterPair::match_call(call) {
        return None;
    }
    fermi_functions::UnregisterPair::decode(call)
        .ok()
        .map(|call| PairState::new(call.base_asset, call.quote_asset, false))
}

fn decode_set_pair_active(call: &eth::v2::Call) -> Option<PairState> {
    if !fermi_functions::SetPairActive::match_call(call) {
        return None;
    }
    fermi_functions::SetPairActive::decode(call)
        .ok()
        .map(|call| PairState::new(call.base_asset, call.quote_asset, call.active))
}

fn active_by_component_id(entity_changes: &[EntityChanges]) -> HashMap<String, bool> {
    entity_changes
        .iter()
        .filter_map(|entity_change| {
            entity_change
                .attributes
                .iter()
                .find(|attribute| attribute.name == ACTIVE_ATTRIBUTE)
                .and_then(|attribute| attribute.value.first())
                .map(|value| (entity_change.component_id.clone(), *value != 0))
        })
        .collect()
}

fn pair_from_component(component: &ProtocolComponent, active: bool) -> Option<PairState> {
    if component.tokens.len() < 2 {
        return None;
    }
    Some(PairState::new(component.tokens[0].clone(), component.tokens[1].clone(), active))
}

fn tx_for_token(
    new_components: &BlockTransactionProtocolComponents,
    token: &[u8],
) -> Option<Transaction> {
    new_components
        .tx_components
        .iter()
        .find_map(|tx_components| {
            if tx_components
                .components
                .iter()
                .any(|component| {
                    component
                        .tokens
                        .iter()
                        .any(|component_token| component_token == token)
                })
            {
                tx_components.tx.clone()
            } else {
                None
            }
        })
}

fn add_pair_balance_snapshot(
    builder: &mut TransactionChangesBuilder,
    pair: &PairState,
    vault_balance_store: &StoreGetBigInt,
) {
    if !pair.active {
        add_component_balance(builder, &pair.id(), &pair.base_asset, BigInt::zero());
        add_component_balance(builder, &pair.id(), &pair.quote_asset, BigInt::zero());
        return;
    }

    for token in [&pair.base_asset, &pair.quote_asset] {
        let balance = vault_balance_store
            .get_at(END_ORDINAL, vault_balance_key(token))
            .unwrap_or_else(BigInt::zero);
        add_component_balance(builder, &pair.id(), token, balance);
    }
}

fn add_component_balances_for_token(
    builder: &mut TransactionChangesBuilder,
    token: &[u8],
    balance: &[u8],
    pairs_store: &StoreGetString,
    token_pairs_store: &StoreGetString,
) {
    let Some(pair_ids) = token_pairs_store.get_at(END_ORDINAL, token_store_key(token)) else {
        return;
    };

    let mut seen = HashSet::new();
    for pair_id in pair_ids
        .split(';')
        .filter(|pair_id| !pair_id.is_empty())
    {
        if !seen.insert(pair_id.to_string()) {
            continue;
        }
        let Some(encoded_pair) = pairs_store.get_at(END_ORDINAL, pair_store_key(pair_id)) else {
            continue;
        };
        let Some(pair) = PairState::decode(&encoded_pair) else {
            continue;
        };
        if pair.active {
            builder.add_balance_change(&BalanceChange {
                token: token.to_vec(),
                balance: balance.to_vec(),
                component_id: pair_id.as_bytes().to_vec(),
            });
        }
    }
}

fn add_component_balance(
    builder: &mut TransactionChangesBuilder,
    pair_id: &str,
    token: &[u8],
    balance: BigInt,
) {
    let balance = if balance < BigInt::zero() { BigInt::zero() } else { balance };
    builder.add_balance_change(&BalanceChange {
        token: token.to_vec(),
        balance: balance.to_bytes_be().1,
        component_id: pair_id.as_bytes().to_vec(),
    });
}

fn updated_pair_ids_from_call(call: &eth::v2::Call) -> Vec<String> {
    if let Some(pair) = decode_register_pair(call)
        .or_else(|| decode_unregister_pair(call))
        .or_else(|| decode_set_pair_active(call))
    {
        return vec![pair.id()];
    }

    if fermi_functions::SetPairParams::match_call(call) {
        return fermi_functions::SetPairParams::decode(call)
            .ok()
            .map(|call| vec![pair_id(&call.base_asset, &call.quote_asset)])
            .unwrap_or_default();
    }

    if fermi_functions::SetFairPriceE8::match_call(call) {
        return fermi_functions::SetFairPriceE8::decode(call)
            .ok()
            .map(|call| vec![format!("0x{}", hex::encode(call.k))])
            .unwrap_or_default();
    }

    if fermi_functions::SetFairPricesE8::match_call(call) {
        return fermi_functions::SetFairPricesE8::decode(call)
            .map(|call| {
                call.u
                    .into_iter()
                    .map(|(key, _)| format!("0x{}", hex::encode(key)))
                    .collect()
            })
            .unwrap_or_default();
    }

    Vec::new()
}
