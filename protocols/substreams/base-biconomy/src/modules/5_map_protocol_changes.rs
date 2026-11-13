use std::collections::BTreeMap;

use anyhow::Result;
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetProto, StoreGetRaw},
};
use substreams_ethereum::pb::eth;
use tycho_substreams::prelude as tycho;

use crate::{
    biconomy::{
        allowance_attribute, attribute, attrs, balance_attribute, maker_attribute,
        provider_attribute, router_attribute, router_count_attribute, to_address, Address,
        MAX_MAKERS,
    },
    modules::{
        config::{Config, ProviderKind},
        store_protocol_components::component_key,
    },
    writes::executor_writes,
};

/// Emits the venue's state changes for the block, all from storage changes, with no RPC:
/// - the venue, provider, vault and token writes `map_state_writes` found, with the bootstrap
///   values and provider configs in the creation transaction;
/// - every executor storage word a board, anchor or pause event wrote, for makers in the venue's
///   list at that transaction and pairs of tracked tokens;
/// - the providers' inventory per token as the component's balances, whenever it can change.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    new_components: tycho::BlockTransactionProtocolComponents,
    component_store: StoreGetProto<tycho::ProtocolComponent>,
    state_writes: tycho::BlockChanges,
    state: StoreGetRaw,
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
            builder.add_entity_change(&tycho::EntityChanges {
                component_id: component_id.clone(),
                attributes: config
                    .providers
                    .iter()
                    .map(|provider| {
                        attribute(provider_attribute(&provider.address), provider.encode())
                    })
                    .collect(),
            });
            add_balances(builder, &config, &state, tx.index);
        }
    }

    for tx_changes in &state_writes.changes {
        let Some(tx) = tx_changes.tx.as_ref() else { continue };
        let builder = builders
            .entry(tx.index)
            .or_insert_with(|| tycho::TransactionChangesBuilder::new(tx));
        let mut inventory = false;
        for change in &tx_changes.entity_changes {
            inventory |= change
                .attributes
                .iter()
                .any(|a| moves_inventory(&a.name));
            builder.add_entity_change(change);
        }
        if inventory {
            add_balances(builder, &config, &state, tx.index);
        }
    }

    for trace in block.transactions() {
        if !trace
            .logs_with_calls()
            .any(|(log, _)| log.address == config.executor)
        {
            continue;
        }
        let makers = listed_makers(&state, u64::from(trace.index));
        let writes = executor_writes(trace, &config, &|mm| makers.contains(mm));
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

/// Attributes provider inventory depends on.
fn moves_inventory(name: &str) -> bool {
    ["balance/", "allowance/", "router_count/", "router/"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn word_at(state: &StoreGetRaw, ord: u64, name: &str) -> BigInt {
    state
        .get_at(ord, name)
        .map(|value| BigInt::from_unsigned_bytes_be(&value))
        .unwrap_or_default()
}

/// A token balance word without the flags some tokens keep in its top bit (USDC's blacklist).
fn balance_at(state: &StoreGetRaw, ord: u64, name: &str) -> BigInt {
    let mut value = state
        .get_at(ord, name)
        .unwrap_or_default();
    if value.len() == 32 {
        value[0] &= 0x7f;
    }
    BigInt::from_unsigned_bytes_be(&value)
}

/// The venue's `makerList` as of the end of transaction `ord`.
fn listed_makers(state: &StoreGetRaw, ord: u64) -> Vec<Address> {
    let count = word_at(state, ord, attrs::MAKER_COUNT)
        .min(BigInt::from(u64::from(MAX_MAKERS)))
        .to_u64() as u8;
    (0..count)
        .filter_map(|i| {
            state
                .get_at(ord, maker_attribute(i))
                .and_then(|value| to_address(&value))
        })
        .collect()
}

/// Each token's inventory across the configured providers as of the end of transaction `ord`,
/// as the providers' `available` computes it: a balance provider's own balance, a vault's
/// balance capped by its allowance while the vault routes to the provider. Unbounded providers
/// add nothing.
fn add_balances(
    builder: &mut tycho::TransactionChangesBuilder,
    config: &Config,
    state: &StoreGetRaw,
    ord: u64,
) {
    for token in &config.tokens {
        let mut total = BigInt::zero();
        for provider in &config.providers {
            let available = match provider.kind {
                ProviderKind::Balance => {
                    balance_at(state, ord, &balance_attribute(&token.address, &provider.address))
                }
                ProviderKind::Vault(vault) => {
                    let routes = !word_at(state, ord, &router_count_attribute(&vault)).is_zero() &&
                        state
                            .get_at(ord, router_attribute(&vault))
                            .and_then(|value| to_address(&value)) ==
                            Some(provider.address);
                    if !routes {
                        continue;
                    }
                    let balance =
                        balance_at(state, ord, &balance_attribute(&token.address, &vault));
                    let allowance = word_at(
                        state,
                        ord,
                        &allowance_attribute(&token.address, &vault, &provider.address),
                    );
                    balance.min(allowance)
                }
                ProviderKind::Unbounded => continue,
            };
            total = total + available;
        }
        builder.add_balance_change(&tycho::BalanceChange {
            token: token.address.to_vec(),
            balance: total.to_signed_bytes_be(),
            component_id: config.component_id().into_bytes(),
        });
    }
}
