use crate::{
    abi::{
        fermi::events::{PairActiveSet, PairRegistered, PairUnregistered},
        registry::functions::{BatchUpdateStateWithSignature, UpdateState},
    },
    pb::fermiswap::v1::Pair,
    utils::{
        component_id, lane_index, lane_index_store_key, Config, OVERRIDE_BLOCK_TIMESTAMP_ATTRIBUTE,
        PAUSED_ATTRIBUTE,
    },
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
        StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsString,
    },
};
use substreams_ethereum::{
    pb::eth::{
        self,
        v2::{Block, Log, TransactionTrace},
    },
    Event, Function,
};
use substreams_helper::event_handler::EventHandler;
use tycho_substreams::{
    abi::{erc20, weth},
    balances::aggregate_balances_changes,
    contract::extract_contract_changes_builder,
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
                config.registry_address.as_slice(),
            ])
            .as_swap_type("fermiswap_pool", ImplementationType::Vm);

        new_pair_changes.push(TransactionEntityChanges {
            tx: Some(tycho_tx.clone()),
            entity_changes: vec![EntityChanges {
                component_id: component_id.clone(),
                attributes: vec![
                    Attribute {
                        name: PAUSED_ATTRIBUTE.to_string(),
                        value: vec![{ 1u8 }],
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "balance_owner".to_string(),
                        value: config.trader_vault.to_vec(),
                        change: ChangeType::Creation.into(),
                    },
                ],
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
fn store_lane_components(pair_changes: BlockEntityChanges, store: StoreSetIfNotExistsString) {
    for tx_changes in pair_changes.changes {
        for component in tx_changes.component_changes {
            let base_asset = &component.tokens[0];
            let quote_asset = &component.tokens[1];
            store.set_if_not_exists(0, lane_index(base_asset, quote_asset), &component.id);
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

/// Emits global trader-vault token balance deltas, not component-scoped balance deltas.
///
/// Newly tracked tokens are snapshotted once with `balanceOf`. Existing tokens are updated from
/// ERC20 `Transfer` and WETH `Deposit`/`Withdrawal` events involving the trader vault.
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

    let trader_vault = config.trader_vault.as_slice();
    let mut vault_token_deltas = Vec::new();
    for raw_tx in block.transactions() {
        let tycho_tx: Transaction = raw_tx.into();
        for (log, _) in raw_tx.logs_with_calls() {
            if let Some(transfer) = erc20::events::Transfer::match_and_decode(log) {
                if transfer.from.as_slice() == trader_vault {
                    vault_token_deltas.push((
                        tycho_tx.clone(),
                        log.ordinal,
                        log.address.clone(),
                        BigInt::zero() - transfer.value,
                    ));
                } else if transfer.to.as_slice() == trader_vault {
                    vault_token_deltas.push((
                        tycho_tx.clone(),
                        log.ordinal,
                        log.address.clone(),
                        transfer.value,
                    ));
                }
            } else if let Some(deposit) = weth::events::Deposit::match_and_decode(log) {
                if deposit.dst.as_slice() == trader_vault {
                    vault_token_deltas.push((
                        tycho_tx.clone(),
                        log.ordinal,
                        log.address.clone(),
                        deposit.wad,
                    ));
                }
            } else if let Some(withdrawal) = weth::events::Withdrawal::match_and_decode(log) {
                if withdrawal.src.as_slice() == trader_vault {
                    vault_token_deltas.push((
                        tycho_tx.clone(),
                        log.ordinal,
                        log.address.clone(),
                        BigInt::zero() - withdrawal.wad,
                    ));
                }
            }
        }
    }

    if vault_token_deltas.is_empty() {
        balance_deltas.sort_unstable_by_key(|delta| delta.ord);
        return Ok(BlockBalanceDeltas { balance_deltas });
    }

    for (tx, ord, token, delta) in vault_token_deltas {
        let token_key = hex::encode(&token);
        // Newly tracked tokens were already snapshotted with `balanceOf` above. Applying their
        // event deltas in the same block would double-count the trader-vault balance.
        if new_token_keys.contains(&token_key) {
            continue;
        }

        // The trader vault may move tokens unrelated to indexed FermiSwap pairs. Only emit global
        // token deltas for tokens that are already mapped to at least one component.
        if token_pairs_store
            .get_last(&token_key)
            .is_none()
        {
            continue;
        }

        balance_deltas.push(BalanceDelta {
            ord,
            tx: Some(tx),
            token,
            delta: delta.to_signed_bytes_be(),
            // Global trader-vault token deltas are not tied to a component yet.
            // `map_balance_deltas` fans them out to component-scoped balances later.
            component_id: vec![],
        });
    }

    balance_deltas.sort_unstable_by_key(|delta| delta.ord);
    Ok(BlockBalanceDeltas { balance_deltas })
}

#[substreams::handlers::store]
fn store_token_balances(mut token_balance_deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    token_balance_deltas
        .balance_deltas
        .sort_unstable_by_key(|delta| delta.ord);

    for delta in token_balance_deltas.balance_deltas {
        let token_key = hex::encode(&delta.token);
        store.add(delta.ord, token_key, BigInt::from_signed_bytes_be(&delta.delta));
    }
}

/// Converts global trader-vault token balance deltas into component-scoped balance deltas.
///
/// FermiSwap pairs share the same trader vault, so token balances are tracked globally first and
/// then projected onto every component that references the changed token.
#[substreams::handlers::map]
fn map_balance_deltas(
    pair_changes: BlockEntityChanges,
    token_balance_deltas: BlockBalanceDeltas,
    token_balance_store: StoreGetBigInt,
    token_pairs_store: StoreGetString,
) -> Result<BlockBalanceDeltas> {
    let mut balance_deltas = Vec::new();
    let mut new_component_ids_by_token = HashMap::<Vec<u8>, HashSet<String>>::new();

    // Component balance deltas are keyed by component id, so every newly created component needs
    // an initial balance entry for all of its tokens. `token_balance_deltas` only contains tokens
    // that moved in this block and would miss unchanged tokens, so seed new components from the
    // accumulated global trader-vault token balance store instead.
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

    // Fan out global token movements to every existing component that uses the token. Components
    // created in this block are skipped because the snapshot above already includes this block's
    // token movements; applying the delta again would double-count their initial balance.
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
    lane_component_store: StoreGetString,
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
        let tx: Transaction = trx.into();
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
                    builder.change_component_pause_state(&component_id, !ev.active);
                }
            } else if let Some(ev) = PairUnregistered::match_and_decode(log) {
                let component_id = component_id(&ev.base_asset, &ev.quote_asset);
                if pair_store
                    .get_last(&component_id)
                    .is_some()
                {
                    builder.change_component_pause_state(&component_id, true);
                }
            }
        }

        for call in trx
            .calls
            .iter()
            .filter(|call| !call.state_reverted && call.address == config.registry_address)
        {
            // Fermi reads fresh oracle state with the current block env. When the registry update
            // is for this block, pass that block env to VM simulation through component attrs.
            if let Some(update) = UpdateState::match_and_decode(call) {
                if update.target.as_slice() == config.engine_address.as_slice() {
                    // Registry calldata only contains `laneIndex`, not the token pair. `laneIndex`
                    // is keccak256(abi.encode(tokenA, tokenB)), so this is a
                    // one-way lookup through the lane store populated when the
                    // pair was created.
                    let update_timestamp = update.update_timestamp.to_u64();
                    if let Some(component_id) = lane_index_store_key(&update.lane_index)
                        .and_then(|lane_key| lane_component_store.get_last(lane_key))
                    {
                        builder.add_entity_change(&EntityChanges {
                            component_id,
                            attributes: vec![Attribute {
                                name: OVERRIDE_BLOCK_TIMESTAMP_ATTRIBUTE.to_string(),
                                value: update_timestamp.to_be_bytes().to_vec(),
                                change: ChangeType::Update.into(),
                            }],
                        });
                    }
                }
            } else if let Some(batch) = BatchUpdateStateWithSignature::match_and_decode(call) {
                for (target, _signer, lane_index, update_timestamp, _slots, _signature) in
                    batch.updates
                {
                    if target == config.engine_address.as_slice() {
                        let update_timestamp = update_timestamp.to_u64();
                        if let Some(component_id) = lane_index_store_key(&lane_index)
                            .and_then(|lane_key| lane_component_store.get_last(lane_key))
                        {
                            builder.add_entity_change(&EntityChanges {
                                component_id,
                                attributes: vec![Attribute {
                                    name: OVERRIDE_BLOCK_TIMESTAMP_ATTRIBUTE.to_string(),
                                    value: update_timestamp.to_be_bytes().to_vec(),
                                    change: ChangeType::Update.into(),
                                }],
                            });
                        }
                    }
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
        |addr| {
            addr == config.engine_address ||
                addr == config.swapper_address ||
                addr == config.registry_address ||
                addr == config.trader_vault
        },
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
