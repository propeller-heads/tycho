use anyhow::Result;
use ethabi::ethereum_types::Address;
use itertools::Itertools;
use std::collections::{HashMap, HashSet};
use substreams::{
    pb::substreams::StoreDeltas,
    prelude::*,
    store::{
        Appender, StoreAdd, StoreAddBigInt, StoreAppend, StoreGet, StoreGetBigInt, StoreGetString,
        StoreNew, StoreSet, StoreSetString,
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
    abi::erc20,
    attributes::json_serialize_address_list,
    balances::{aggregate_balances_changes, extract_balance_deltas_from_tx},
    block_storage::get_block_storage_changes,
    contract::extract_contract_changes_builder,
    entrypoint::create_entrypoint,
    models::{entry_point_params::TraceData, RpcTraceData},
    prelude::*,
};

use crate::{
    abi::{
        registry::functions::{BatchUpdateStateWithSignature, UpdateState},
        tempest::{
            self,
            events::{PairRegistered, Paused, Unpaused, VaultUpdated},
        },
    },
    utils::{
        component_id, lane_for, lane_index_to_lane_key, lane_key, sort_tokens, Config,
        ALL_COMPONENTS_KEY, BALANCE_OWNER_ATTRIBUTE, LANE_KEY_PREFIX,
        OVERRIDE_BLOCK_TIMESTAMP_ATTRIBUTE, PAUSED_KEY, VAULT_KEY,
    },
};

#[substreams::handlers::map]
fn map_protocol_components(
    params: String,
    block: Block,
    pair_registered_deltas: StoreDeltas,
    router_state_store: StoreGetString,
) -> Result<BlockEntityChanges> {
    let config: Config = serde_qs::from_str(params.as_str())?;
    let mut changes: Vec<TransactionEntityChanges> = vec![];
    let vault = vault_address(&router_state_store, &config);
    get_new_pairs(
        &config,
        &vault,
        &block,
        first_registrations(pair_registered_deltas),
        &mut changes,
    );
    Ok(BlockEntityChanges { block: Some((&block).into()), changes })
}

/// Component ids being registered for the very first time in this block.
///
/// `addPair` after a `removePair` re-emits `PairRegistered`, and re-creating the component then
/// would emit a duplicate. The registration store distinguishes the two: a first registration has
/// no previous value, while a re-registration transitions from the `"0"` written by `removePair`.
fn first_registrations(pair_registered_deltas: StoreDeltas) -> HashSet<String> {
    pair_registered_deltas
        .deltas
        .into_iter()
        .filter(|delta| delta.old_value.is_empty())
        .map(|delta| delta.key)
        // The store also holds lane -> component id entries, which are not component ids and
        // must not be mistaken for one.
        .filter(|key| !key.starts_with(LANE_KEY_PREFIX))
        .collect()
}

/// Decides the pause transition a `PairRegistered` event implies.
///
/// Returns `Some(paused)` when the component's pause state has to change, or `None` when the event
/// is the one that created the component — a new component starts unpaused, so there is nothing to
/// emit for it.
///
/// `pending_creation` holds the ids whose creation event has not been seen yet in this block, and
/// the entry is **consumed** the first time a registration is seen. That matters when a pair is
/// added, removed and added again within one block: the store deltas are `("", "1")`, `("1", "0")`
/// and `("0", "1")`, so the id looks like a first registration for all three. Consuming the entry
/// means only the first event is treated as the creation, and the third correctly lifts the pause
/// that the second applied instead of leaving the component paused while it is registered on chain.
fn pause_transition(
    pending_creation: &mut HashSet<String>,
    id: &str,
    registered: bool,
) -> Option<bool> {
    if !registered {
        return Some(true);
    }
    // `remove` reports whether this is the still-unconsumed creation event.
    if pending_creation.remove(id) {
        None
    } else {
        Some(false)
    }
}

/// Emits a component for every pair the router registers.
///
/// `PairRegistered` carries a `registered` flag because `removePair` reuses the event; only
/// registrations create components. Deregistration is handled in `map_protocol_changes`, which
/// flips the component's pause state instead — Tycho components are immutable once created.
fn get_new_pairs(
    config: &Config,
    vault: &[u8],
    block: &Block,
    mut pending_creation: HashSet<String>,
    changes: &mut Vec<TransactionEntityChanges>,
) {
    // Neither the router's implementation nor the vault is listed. A component's contract set is
    // frozen at creation, so anything that can move behind the router goes stale in it: the
    // implementation on the next `upgradeToAndCall`, and the vault on the next `VaultUpdated`.
    // Listing either would split components into cohorts that diverge permanently, since the set
    // becomes `involved_contracts` in simulation.
    //
    // The implementation is discovered by DCI instead -- see `add_entrypoints`. The vault needs no
    // discovery at all: nothing ever calls it, because `quote` reads its balance and allowance out
    // of the *token* contracts, and simulation reaches its inventory through the mutable
    // `balance_owner` attribute rather than through this set. `map_protocol_changes` repoints that
    // attribute on `VaultUpdated`, so a rotation is followed without anything being frozen here.
    let contracts = [config.router_address.as_slice(), config.registry_address.as_slice()];

    let mut on_pair_registered = |event: PairRegistered, tx: &TransactionTrace, _log: &Log| {
        if !event.registered {
            return;
        }

        let tycho_tx: Transaction = tx.into();
        let (token0, token1) = sort_tokens(&event.token0, &event.token1);
        let id = component_id(&config.router_address, token0, token1);

        // A re-registered pair already has a component; `map_protocol_changes` unpauses it
        // instead. The entry is consumed so that an add/remove/add within one block — which
        // leaves the id looking like a first registration for both `registered` events — emits
        // the component exactly once.
        if !pending_creation.remove(&id) {
            return;
        }

        let component = ProtocolComponent::new(id.as_str())
            .with_tokens(&[token0, token1])
            .with_contracts(&contracts)
            // Tempest emits no token contract storage, so in the shared simulation DB its tokens
            // are self-contained proxies. Flag them so simulation resolves their transfers locally
            // rather than binding them to an implementation another VM protocol indexed for the
            // same token.
            .with_attributes(&[(
                "self_contained_tokens",
                json_serialize_address_list(&[token0.to_vec(), token1.to_vec()]),
            )])
            .as_swap_type("tempest_pair", ImplementationType::Vm);

        // No `paused` attribute at creation: `addPair` is itself the activation, and an absent
        // attribute already means "not paused".
        changes.push(TransactionEntityChanges {
            tx: Some(tycho_tx),
            entity_changes: vec![EntityChanges {
                component_id: id,
                attributes: vec![Attribute {
                    name: BALANCE_OWNER_ATTRIBUTE.to_string(),
                    value: vault.to_vec(),
                    change: ChangeType::Creation.into(),
                }],
            }],
            component_changes: vec![component],
            balance_changes: vec![],
        });
    };

    let mut eh = EventHandler::new(block);
    eh.filter_by_address(vec![Address::from_slice(&config.router_address)]);
    eh.on::<PairRegistered, _>(&mut on_pair_registered);
    eh.handle_events();
}

/// Tracks router-level state the rest of the package needs: the vault address and the pause flag.
///
/// The vault is read from `VaultUpdated` rather than hardcoded, because the router can migrate it
/// and a stale address would silently stop balance tracking. The proxy emits `VaultUpdated` when
/// it initialises, which is the package's `initialBlock`, so the key is populated from the first
/// block the package sees.
#[substreams::handlers::store]
fn store_router_state(params: String, block: Block, store: StoreSetString) {
    let Ok(config) = serde_qs::from_str::<Config>(params.as_str()) else {
        return;
    };

    for trx in block.transactions() {
        for (log, _) in trx.logs_with_calls() {
            if log.address != config.router_address {
                continue;
            }
            if let Some(ev) = VaultUpdated::match_and_decode(log) {
                store.set(log.ordinal, VAULT_KEY, &hex::encode(&ev.new_vault));
            } else if Paused::match_and_decode(log).is_some() {
                store.set(log.ordinal, PAUSED_KEY, &"1".to_string());
            } else if Unpaused::match_and_decode(log).is_some() {
                store.set(log.ordinal, PAUSED_KEY, &"0".to_string());
            }
        }
    }
}

/// The live `TempestVault` address.
///
/// Prefers the value [`store_router_state`] recorded from `VaultUpdated`, so a rotation is picked
/// up, and falls back to the configured address. The fallback matters because the only
/// `VaultUpdated` on chain is the initialisation in the package's `initialBlock`: any run that
/// starts after it — a partial re-index, a shortened test range — would otherwise see no vault at
/// all and silently emit neither components nor balances.
fn vault_address(router_state_store: &StoreGetString, config: &Config) -> Vec<u8> {
    router_state_store
        .get_last(VAULT_KEY)
        .and_then(|raw| hex::decode(raw).ok())
        .unwrap_or_else(|| config.vault_address.clone())
}

/// Whether the router is currently paused, per [`store_router_state`].
fn router_paused(router_state_store: &StoreGetString) -> bool {
    router_state_store
        .get_last(PAUSED_KEY)
        .as_deref() ==
        Some("1")
}

/// Tracks whether each component's pair is currently registered on the router.
///
/// Keyed by component id: `"1"` while registered, `"0"` after `removePair`. Presence of the key
/// means the component is one of ours; the value distinguishes a live pair from a delisted one, so
/// a router unpause does not revive a pair that was separately removed.
#[substreams::handlers::store]
fn store_pair_registered(params: String, block: Block, store: StoreSetString) {
    let Ok(config) = serde_qs::from_str::<Config>(params.as_str()) else {
        return;
    };

    let mut on_pair_registered = |event: PairRegistered, _tx: &TransactionTrace, log: &Log| {
        let id = component_id(&config.router_address, &event.token0, &event.token1);
        // The component id is router-scoped and so cannot be derived from a registry lane index.
        // Record the mapping here so `add_override_block_timestamp` can resolve one.
        store.set(log.ordinal, lane_key(&lane_for(&event.token0, &event.token1)), &id);
        store.set(log.ordinal, id, &if event.registered { "1" } else { "0" }.to_string());
    };

    let mut eh = EventHandler::new(&block);
    eh.filter_by_address(vec![Address::from_slice(&config.router_address)]);
    eh.on::<PairRegistered, _>(&mut on_pair_registered);
    eh.handle_events();
}

/// Indexes component ids by token, and under [`ALL_COMPONENTS_KEY`] for router-wide events.
///
/// Token keys drive the vault balance fan-out; the catch-all key lets `map_protocol_changes`
/// enumerate every known component when the router pauses, which is otherwise impossible with a
/// `get`-only store.
#[substreams::handlers::store]
fn store_component_index(components: BlockEntityChanges, store: StoreAppend<String>) {
    for tx_changes in components.changes {
        for component in tx_changes.component_changes {
            for token in component.tokens {
                store.append(0, token_key(&token), component.id.clone());
            }
            store.append(0, ALL_COMPONENTS_KEY, component.id.clone());
        }
    }
}

/// Store key under which the component ids trading `token` are indexed.
fn token_key(token: &[u8]) -> String {
    format!("token:{}", hex::encode(token))
}

/// Emits global vault token balance deltas, not component-scoped ones.
///
/// All Tempest pairs draw on one shared vault, so inventory is tracked per token first and fanned
/// out to components in `map_balance_deltas`. Newly tracked tokens are snapshotted once with
/// `balanceOf`; thereafter the balance follows ERC20 `Transfer` and WETH `Deposit`/`Withdrawal`
/// events touching the vault.
#[substreams::handlers::map]
fn map_vault_balance_deltas(
    params: String,
    block: Block,
    token_component_deltas: StoreDeltas,
    token_components_store: StoreGetString,
    router_state_store: StoreGetString,
) -> Result<BlockBalanceDeltas> {
    let config: Config = serde_qs::from_str(params.as_str())?;
    let mut balance_deltas = Vec::new();
    let vault = vault_address(&router_state_store, &config);

    // Only `token:` keys carry a token to snapshot; the catch-all component index shares this
    // store and must be skipped.
    let new_tokens = token_component_deltas
        .deltas
        .into_iter()
        .filter(|delta| delta.old_value.is_empty())
        .filter_map(|delta| {
            delta
                .key
                .strip_prefix("token:")
                .map(str::to_string)
        })
        .collect::<HashSet<_>>();
    let last_tx = block
        .transaction_traces
        .last()
        .map(Transaction::from);

    for token_hex in &new_tokens {
        let Some(tx) = &last_tx else {
            continue;
        };
        let token = hex::decode(token_hex)?;
        // A failed `balanceOf` means the token is not a conforming ERC20. Seeding zero keeps the
        // stream alive for the other pairs rather than halting every component on one bad token,
        // but it under-reports this token's reserve, so make the reason findable in the logs.
        let balance = erc20::functions::BalanceOf { owner: vault.clone() }
            .call(token.clone())
            .unwrap_or_else(|| {
                substreams::log::info!("balanceOf failed for token 0x{token_hex}; seeding zero");
                BigInt::zero()
            });
        balance_deltas.push(BalanceDelta {
            ord: tx.index,
            tx: Some(tx.clone()),
            token,
            delta: balance.to_signed_bytes_be(),
            component_id: vec![],
        });
    }

    // `extract_balance_deltas_from_tx` credits and debits independently, so a vault-to-vault
    // transfer nets to zero instead of only debiting, and WETH `Deposit`/`Withdrawal` are covered
    // the same way. It tags each delta with the transactor address; the fan-out to components
    // happens in `map_balance_deltas`, so the tag is dropped here.
    let vault_token_deltas: Vec<_> = block
        .transactions()
        .flat_map(|trx| {
            extract_balance_deltas_from_tx(trx, |_token, address| address == vault.as_slice())
        })
        .collect();

    for delta in vault_token_deltas {
        let BalanceDelta { ord, tx, token, delta, .. } = delta;
        // Tokens tracked for the first time in this block were snapshotted with `balanceOf` above,
        // which already reflects this block's movements. Applying the deltas too would
        // double-count.
        if new_tokens.contains(&hex::encode(&token)) {
            continue;
        }

        // The vault may hold tokens that belong to no registered pair. Only emit deltas for tokens
        // mapped to at least one component.
        if token_components_store
            .get_last(token_key(&token))
            .is_none()
        {
            continue;
        }

        balance_deltas.push(BalanceDelta {
            ord,
            tx,
            token,
            delta,
            // Global vault deltas are not component-scoped yet; `map_balance_deltas` fans them out.
            component_id: vec![],
        });
    }

    balance_deltas.sort_unstable_by_key(|delta| delta.ord);
    Ok(BlockBalanceDeltas { balance_deltas })
}

#[substreams::handlers::store]
fn store_vault_token_balances(mut deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    deltas
        .balance_deltas
        .sort_unstable_by_key(|delta| delta.ord);

    for delta in deltas.balance_deltas {
        store.add(delta.ord, hex::encode(&delta.token), BigInt::from_signed_bytes_be(&delta.delta));
    }
}

/// Projects global vault token balances onto every component that trades the token.
#[substreams::handlers::map]
fn map_balance_deltas(
    components: BlockEntityChanges,
    vault_balance_deltas: BlockBalanceDeltas,
    vault_balance_store: StoreGetBigInt,
    token_components_store: StoreGetString,
) -> Result<BlockBalanceDeltas> {
    let mut balance_deltas = Vec::new();
    let mut new_component_ids_by_token = HashMap::<Vec<u8>, HashSet<String>>::new();

    // Component balances are keyed by component id, so a new component needs an initial entry for
    // both its tokens. `vault_balance_deltas` only covers tokens that moved this block and would
    // miss the rest, so seed new components from the accumulated global vault balance instead.
    for tx_changes in components.changes {
        let Some(tx) = tx_changes.tx else {
            continue;
        };

        for component in tx_changes.component_changes {
            for token in component.tokens {
                new_component_ids_by_token
                    .entry(token.clone())
                    .or_default()
                    .insert(component.id.clone());

                let balance = vault_balance_store
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

    // Fan global token movements out to existing components. Components created in this block are
    // skipped: the snapshot above already includes this block's movements.
    for token_delta in vault_balance_deltas.balance_deltas {
        let Some(component_ids) = token_components_store.get_last(token_key(&token_delta.token))
        else {
            continue;
        };
        let new_component_ids = new_component_ids_by_token.get(&token_delta.token);

        for id in component_ids
            .split(';')
            .filter(|id| !id.is_empty())
            .unique()
        {
            if new_component_ids
                .map(|ids| ids.contains(id))
                .unwrap_or(false)
            {
                continue;
            }

            balance_deltas.push(BalanceDelta {
                ord: token_delta.ord,
                tx: token_delta.tx.clone(),
                token: token_delta.token.clone(),
                delta: token_delta.delta.clone(),
                component_id: id.as_bytes().to_vec(),
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
    components: BlockEntityChanges,
    pair_registered_store: StoreGetString,
    pair_registered_deltas: StoreDeltas,
    component_index_store: StoreGetString,
    router_state_store: StoreGetString,
    // Component-scoped, despite being derived from the shared vault's token balances: the deltas
    // arrive from `map_balance_deltas`/`store_balances`, which have already fanned the vault's
    // per-token movements out to every component that trades the token.
    component_balance_deltas: BlockBalanceDeltas,
    component_balance_store_deltas: StoreDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let config: Config = serde_qs::from_str(params.as_str())?;
    let mut pending_creation = first_registrations(pair_registered_deltas);
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();
    let vault = vault_address(&router_state_store, &config);
    let paused = router_paused(&router_state_store);

    for tx_changes in components.changes {
        let Some(tycho_tx) = tx_changes.tx else {
            continue;
        };
        let builder = transaction_changes
            .entry(tycho_tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tycho_tx));

        for component in &tx_changes.component_changes {
            builder.add_protocol_component(component);
            add_entrypoints(builder, &config, component);
        }
        for entity_change in &tx_changes.entity_changes {
            builder.add_entity_change(entity_change);
        }
    }

    for trx in block.transactions() {
        // Most transactions in a block touch neither the router nor the registry. Skipping them
        // here avoids allocating a builder per transaction just to drop it again at `build()`.
        // Scanning `calls` rather than `logs_with_calls` keeps the gate itself allocation-free:
        // the latter collects and sorts every log in the transaction before yielding one. The
        // router is a proxy, so a `PairRegistered` log is emitted from the delegatecall frame
        // whose `address` is the implementation — but the entry frame that called the proxy
        // still carries `address == router`, and this is an `any` over every frame.
        let touches_tempest = trx.calls.iter().any(|call| {
            !call.state_reverted &&
                (call.address == config.router_address ||
                    call.address == config.registry_address)
        });
        if !touches_tempest {
            continue;
        }

        let tx: Transaction = trx.into();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));

        for (log, _) in trx.logs_with_calls() {
            if log.address != config.router_address {
                continue;
            }

            if let Some(ev) = PairRegistered::match_and_decode(log) {
                // `removePair` re-emits `PairRegistered` with `registered = false`. Components are
                // immutable, so a delisting is surfaced as a pause rather than a removal — and a
                // later `addPair` for the same pair has to lift that pause, because no new
                // component is created for it.
                let id = component_id(&config.router_address, &ev.token0, &ev.token1);
                if pair_registered_store
                    .get_last(&id)
                    .is_none()
                {
                    continue;
                }
                // Consume the creation entry either way, so the router's pause flag cannot
                // leave a stale entry behind and make a later event look like a creation.
                let transition = pause_transition(&mut pending_creation, &id, ev.registered);
                // Every quote and settlement entrypoint is `whenNotPaused`, so a pair is not
                // quotable while the router is paused however it came to be registered. Whether
                // `addPair` itself is reachable during a pause is a property of the router's
                // admin functions and is not observable from here, so this is defensive: if it
                // is reachable the component is correctly reported paused, and if it is not the
                // branch is simply never taken. The later `Unpaused` lifts it either way.
                let transition = if paused { Some(true) } else { transition };
                if let Some(state) = transition {
                    builder.change_component_pause_state(&id, state);
                }
            } else if let Some(ev) = VaultUpdated::match_and_decode(log) {
                // A component's `contract_addresses` is frozen at creation and cannot follow a
                // rotation, but attributes are mutable. Repointing `balance_owner` keeps the
                // balance overwrites addressed at the live vault; without it every existing
                // component's inventory is written to the old address and the venue silently
                // stops quoting.
                for id in known_component_ids(&component_index_store) {
                    builder.add_entity_change(&EntityChanges {
                        component_id: id,
                        attributes: vec![Attribute {
                            name: BALANCE_OWNER_ATTRIBUTE.to_string(),
                            value: ev.new_vault.clone(),
                            change: ChangeType::Update.into(),
                        }],
                    });
                }
            } else if Paused::match_and_decode(log).is_some() {
                // The router is `Pausable` and every settlement and quote entrypoint is
                // `whenNotPaused`, so a router pause stops all pairs at once.
                for id in known_component_ids(&component_index_store) {
                    builder.change_component_pause_state(&id, true);
                }
            } else if Unpaused::match_and_decode(log).is_some() {
                for id in known_component_ids(&component_index_store) {
                    // A pair delisted with `removePair` stays paused: `pairRegistered` gates
                    // quoting independently of the router's pause flag, so an unpause must not
                    // revive it.
                    if pair_registered_store
                        .get_last(&id)
                        .as_deref() !=
                        Some("1")
                    {
                        continue;
                    }
                    builder.change_component_pause_state(&id, false);
                }
            }
        }

        for call in trx
            .calls
            .iter()
            .filter(|call| !call.state_reverted && call.address == config.registry_address)
        {
            // The router reads its lane with the settling block's timestamp and reverts
            // `StaleUpdate` outside a 12s window. Pin simulation to the committed quote timestamp
            // so the gate passes off-chain.
            if let Some(update) = UpdateState::match_and_decode(call) {
                if update.target.as_slice() != config.router_address.as_slice() {
                    continue;
                }
                add_override_block_timestamp(
                    builder,
                    &pair_registered_store,
                    &update.lane_index,
                    update.update_timestamp.to_u64(),
                );
            } else if let Some(batch) = BatchUpdateStateWithSignature::match_and_decode(call) {
                for (target, _signer, lane_index, update_timestamp, _slots, _signature) in
                    batch.updates
                {
                    if target != config.router_address.as_slice() {
                        continue;
                    }
                    add_override_block_timestamp(
                        builder,
                        &pair_registered_store,
                        &lane_index,
                        update_timestamp.to_u64(),
                    );
                }
            }
        }
    }

    aggregate_balances_changes(component_balance_store_deltas, component_balance_deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            let mut contract_change = InterimContractChange::new(&vault, false);
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
        |addr| addr == config.router_address || addr == vault || addr == config.registry_address,
        &mut transaction_changes,
    );

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        // Consumed by DCI to keep the storage of the contracts it discovers (the router's
        // implementation above all) up to date after the initial trace.
        storage_changes: get_block_storage_changes(&block),
    })
}

/// Registers the DCI entrypoints a component's simulation depends on.
///
/// The router is an ERC1967 proxy behind which the logic contract moves on every
/// `upgradeToAndCall`, and a component's contract set cannot change after creation — so the
/// implementation cannot be indexed by listing it as a contract. DCI resolves this: tracing an
/// entrypoint discovers every contract the call touches, and the proxy's implementation slot is
/// flagged as a retrigger, so an upgrade re-traces and the new implementation is picked up without
/// a re-index.
///
/// `isActive` is the traced call rather than `quote`: its calldata is fully known (just the token
/// pair, no amount to invent, which DCI cannot generate), and it returns `false` instead of
/// reverting when the lane is stale, so the trace still reaches the registry and token reads. It
/// covers the router implementation, the registry, and the `tokenOut` contract. Both directions are
/// registered so each token is seen as `tokenOut`.
fn add_entrypoints(
    builder: &mut TransactionChangesBuilder,
    config: &Config,
    component: &ProtocolComponent,
) {
    if component.tokens.len() != 2 {
        return;
    }

    for (token_in, token_out) in
        [(&component.tokens[0], &component.tokens[1]), (&component.tokens[1], &component.tokens[0])]
    {
        let calldata = tempest::functions::IsActive {
            token_in: token_in.clone(),
            token_out: token_out.clone(),
        }
        .encode();

        let (entrypoint, params) = create_entrypoint(
            config.router_address.clone(),
            "isActive(address,address)".to_string(),
            component.id.clone(),
            TraceData::Rpc(RpcTraceData { caller: None, calldata }),
        );
        builder.add_entrypoint(&entrypoint);
        builder.add_entrypoint_params(&params);
    }
}

/// Every component the package has created so far, from the catch-all index key.
fn known_component_ids(component_index_store: &StoreGetString) -> Vec<String> {
    component_index_store
        .get_last(ALL_COMPONENTS_KEY)
        .map(|ids| {
            ids.split(';')
                .filter(|id| !id.is_empty())
                .unique()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Pins a component's simulation timestamp to the quote the maker just committed for its lane.
///
/// Component ids are router-scoped, so the lane index is resolved through the mapping
/// `store_pair_registered` records at registration. An unmapped lane belongs to another router
/// sharing the registry and is ignored.
fn add_override_block_timestamp(
    builder: &mut TransactionChangesBuilder,
    pair_registered_store: &StoreGetString,
    lane_index: &BigInt,
    update_timestamp: u64,
) {
    let Some(key) = lane_index_to_lane_key(lane_index) else {
        return;
    };
    let Some(id) = pair_registered_store.get_last(&key) else {
        return;
    };

    builder.add_entity_change(&EntityChanges {
        component_id: id,
        attributes: vec![Attribute {
            name: OVERRIDE_BLOCK_TIMESTAMP_ATTRIBUTE.to_string(),
            value: update_timestamp.to_be_bytes().to_vec(),
            change: ChangeType::Update.into(),
        }],
    });
}

#[cfg(test)]
mod tests {
    use substreams::pb::substreams::StoreDelta;

    use super::*;

    fn delta(key: &str, old_value: &str, new_value: &str) -> StoreDelta {
        StoreDelta {
            operation: 0,
            ordinal: 0,
            key: key.to_string(),
            old_value: old_value.as_bytes().to_vec(),
            new_value: new_value.as_bytes().to_vec(),
        }
    }

    /// A pair re-added after `removePair` transitions `"0"` -> `"1"` and must not count as a first
    /// registration, otherwise `get_new_pairs` emits a duplicate component for an existing id.
    #[test]
    fn test_first_registrations_excludes_re_registrations() {
        let deltas =
            StoreDeltas { deltas: vec![delta("0xfirst", "", "1"), delta("0xreadded", "0", "1")] };

        let first = first_registrations(deltas);

        assert!(first.contains("0xfirst"));
        assert!(!first.contains("0xreadded"));
    }

    /// `addPair` -> `removePair` -> `addPair` in one block. All three deltas carry the id as a
    /// first registration, so a non-consuming check would skip the final event and leave the
    /// component paused while the pair is registered on chain.
    #[test]
    fn test_pause_transition_add_remove_add_in_one_block() {
        let id = "0xpair";
        let mut pending: HashSet<String> = [id.to_string()].into_iter().collect();

        assert_eq!(pause_transition(&mut pending, id, true), None, "creation emits nothing");
        assert_eq!(pause_transition(&mut pending, id, false), Some(true), "removePair pauses");
        assert_eq!(pause_transition(&mut pending, id, true), Some(false), "re-add must unpause");
    }

    /// A pair created and delisted in the same block ends paused, matching the chain.
    #[test]
    fn test_pause_transition_add_then_remove_in_one_block() {
        let id = "0xpair";
        let mut pending: HashSet<String> = [id.to_string()].into_iter().collect();

        assert_eq!(pause_transition(&mut pending, id, true), None);
        assert_eq!(pause_transition(&mut pending, id, false), Some(true));
    }

    /// An existing component re-registered in a later block has no pending creation, so the very
    /// first registration it sees must lift the pause.
    #[test]
    fn test_pause_transition_reregistration_without_pending_creation() {
        let mut pending: HashSet<String> = HashSet::new();

        assert_eq!(pause_transition(&mut pending, "0xpair", true), Some(false));
    }

    /// A delisting also writes the store, and must not be mistaken for a creation.
    /// The pair-registered store also carries lane -> component id entries. Those are not
    /// component ids and must not be treated as pending creations.
    #[test]
    fn test_first_registrations_excludes_lane_mappings() {
        let lane = lane_key(&[0u8; 32]);
        let deltas =
            StoreDeltas { deltas: vec![delta("0xabc", "", "1"), delta(&lane, "", "0xabc")] };
        let first = first_registrations(deltas);
        assert!(first.contains("0xabc"));
        assert!(!first.contains(&lane), "lane mapping leaked into pending creations");
        assert_eq!(first.len(), 1);
    }

    #[test]
    fn test_first_registrations_excludes_delistings() {
        let deltas = StoreDeltas { deltas: vec![delta("0xremoved", "1", "0")] };

        assert!(first_registrations(deltas).is_empty());
    }
}
