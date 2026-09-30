#![allow(clippy::not_unsafe_ptr_arg_deref)]
mod config;

use alloy_primitives::{b256, Address, B256, U256};
use anyhow::Result;
use config::{mapping, namespace, Config};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use substreams::{
    pb::substreams::{store_delta::Operation, StoreDelta, StoreDeltas},
    prelude::*,
    scalar::BigInt,
};
use substreams_ethereum::pb::eth::v2::Block;
use tycho_substreams::{
    abi::erc20::functions::BalanceOf,
    balances::{extract_balance_deltas_from_tx, store_balance_changes},
    prelude::*,
};

const VALIDATED_BLOCK: u64 = 51_696_183;
const CURVE_UPDATED: B256 =
    b256!("9beb28798dcd13ee356d61056b666fd0b145c21c274d8743245ce6d60aa28860");
const WITHDRAW_SETTLED: B256 =
    b256!("88593277f0d65b0873b93edb43094ab40071e31177f6c8b5487d4edb6217ce13");
const WITHDRAW_EXECUTED: B256 =
    b256!("29a0bcf57d42cd9503e61240cef8d4ae3c5982196af8c0f63b0585b3a3e87f39");
const TAKER_FEE_SET: B256 =
    b256!("1f50e1aaaff835659bf08a8d3473edefb7f61e28a27572c62fa8daa40da9e268");
const TAKER_FEE_CLEARED: B256 =
    b256!("4e36b92da5e2a98be73ea9f9bd228101bb4b4e83629908148cef524aaad69b94");
const UPGRADED: B256 = b256!("bc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b");

type ReadStore<'a> = &'a dyn Fn(u64, &str) -> Option<Vec<u8>>;

fn word_key(address: Address, slot: B256) -> String {
    format!("word:{address:x}:{slot:x}")
}

/// Parses a store_keys list; append's wire encoding is semicolon-delimited.
fn addresses(value: Option<Vec<u8>>) -> Vec<Address> {
    value.map_or_else(Vec::new, |value| {
        String::from_utf8(value)
            .unwrap()
            .split(';')
            .filter(|v| !v.is_empty())
            .map(|v| v.parse().unwrap())
            .collect()
    })
}

/// Taker lists are sharded by the taker's first byte to stay under the 8 KB append limit.
fn fees_key(base: &str, taker: &str) -> String {
    format!("fees:{base}:{}", &taker[..2])
}

/// Bases whose first CurveUpdated falls in this block, with its ordinal.
fn listings(changes: &StoreDeltas) -> BTreeMap<Address, u64> {
    changes
        .deltas
        .iter()
        .filter(|d| d.operation == Operation::Create as i32)
        .filter_map(|d| {
            Some((
                d.key
                    .strip_prefix("pair:")?
                    .parse()
                    .ok()?,
                d.ordinal,
            ))
        })
        .collect()
}

/// Keep latest words and fees independently of pair discovery. Fees and claims can
/// predate a first SHAPE, and migration can initialize counters without emitting it.
fn state_changes(config: &Config, block: &Block) -> Vec<(u64, String, Vec<u8>)> {
    let claims = namespace("baibai.storage.Custodian") + U256::from(2);
    let mut changes = Vec::new();
    for tx in block.transactions() {
        let mut claim_slots = BTreeSet::new();
        for (log, _) in tx.logs_with_calls() {
            let Some(topic) = log.topics.first() else { continue };
            if log.address == config.curve_book.as_slice() && topic == CURVE_UPDATED.as_slice() {
                let base = Address::from_slice(&log.topics[1][12..]);
                changes.push((log.ordinal, format!("pair:{base:x}"), vec![1]));
            }
            if log.address == config.entrypoint.as_slice() &&
                (topic == TAKER_FEE_SET.as_slice() || topic == TAKER_FEE_CLEARED.as_slice())
            {
                let taker = Address::from_slice(&log.topics[1][12..]);
                let base = Address::from_slice(&log.topics[2][12..]);
                let configured = topic == TAKER_FEE_SET.as_slice();
                let bps = if configured { U256::from_be_slice(&log.data).to::<u16>() } else { 0 };
                changes.push((
                    log.ordinal,
                    format!("fee:{base:x}:{taker:x}"),
                    vec![u8::from(configured), (bps >> 8) as u8, bps as u8],
                ));
            }
            if log.address == config.custodian.as_slice() &&
                (topic == WITHDRAW_SETTLED.as_slice() || topic == WITHDRAW_EXECUTED.as_slice())
            {
                let token = U256::from_be_slice(&log.topics[2]);
                claim_slots.insert(B256::from(mapping(token, claims)));
            }
            if block.number > VALIDATED_BLOCK &&
                topic == UPGRADED.as_slice() &&
                [config.entrypoint, config.curve_book, config.custodian]
                    .iter()
                    .any(|a| log.address == a.as_slice())
            {
                changes.push((log.ordinal, "paused".into(), vec![1]));
            }
        }
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for write in &call.storage_changes {
                let address = Address::from_slice(&write.address);
                let slot = B256::from_slice(&write.key);
                if address == config.curve_book ||
                    (address == config.custodian && claim_slots.contains(&slot))
                {
                    changes.push((write.ordinal, word_key(address, slot), write.new_value.clone()));
                }
            }
        }
    }
    changes.sort_by_key(|c| c.0);
    changes
}

#[substreams::handlers::store]
pub fn store_state(params: String, block: Block, store: StoreSetRaw) {
    let config = Config::parse(&params).unwrap();
    for (ordinal, key, value) in state_changes(&config, &block) {
        store.set(ordinal, key, &value);
    }
}

/// Index keys once, not on every update. Its size is proportional to distinct bases
/// and configured takers, rather than the number of publications or fee changes.
#[substreams::handlers::store]
pub fn store_keys(changes: StoreDeltas, store: StoreAppend<String>) {
    for delta in changes
        .deltas
        .into_iter()
        .filter(|d| d.operation == Operation::Create as i32)
    {
        if let Some(base) = delta.key.strip_prefix("pair:") {
            store.append(delta.ordinal, "pairs", base.to_string());
        } else if let Some(fee) = delta.key.strip_prefix("fee:") {
            let (base, taker) = fee.split_once(':').unwrap();
            store.append(delta.ordinal, fees_key(base, taker), taker.to_string());
        }
    }
}

fn balance_key(config: &Config, token: Address) -> String {
    format!("{:x}:{token:x}", config.custodian)
}

/// Track actual custody once per token. New tokens are bootstrapped at the start
/// of their discovery block by subtracting that block's deltas from balanceOf(end).
fn balance_deltas(
    config: &Config,
    block: &Block,
    bases: &[Address],
    changes: &StoreDeltas,
    balance_of: &dyn Fn(Address) -> Result<BigInt>,
) -> Result<BlockBalanceDeltas> {
    let mut tokens: BTreeSet<_> = bases.iter().copied().collect();
    tokens.insert(config.quote);
    let mut deltas = Vec::new();
    for tx in block.transactions() {
        for mut delta in extract_balance_deltas_from_tx(tx, |token, owner| {
            owner == config.custodian.as_slice() && tokens.contains(&Address::from_slice(token))
        }) {
            delta.component_id = format!("{:x}", config.custodian).into_bytes();
            deltas.push(delta);
        }
    }
    let mut new_tokens: BTreeSet<_> = listings(changes).into_keys().collect();
    if block.number == config.start_block {
        new_tokens.insert(config.quote);
    }
    if !new_tokens.is_empty() {
        let tx = block
            .transactions()
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing bootstrap transaction"))?;
        for token in new_tokens {
            let net = deltas
                .iter()
                .filter(|d| d.token == token.as_slice())
                .fold(BigInt::zero(), |sum, d| sum + BigInt::from_signed_bytes_be(&d.delta));
            let opening = balance_of(token)? - net;
            anyhow::ensure!(opening >= BigInt::zero(), "negative opening custody balance");
            deltas.push(BalanceDelta {
                ord: tx.begin_ordinal,
                tx: Some(tx.into()),
                token: token.to_vec(),
                delta: opening.to_signed_bytes_be(),
                component_id: format!("{:x}", config.custodian).into_bytes(),
            });
        }
    }
    deltas.sort_by_key(|d| (d.ord, d.token.clone()));
    // A self-transfer yields equal and opposite deltas at the same ordinal.
    // The balance store requires strictly increasing ordinals per token.
    let mut merged: Vec<BalanceDelta> = Vec::with_capacity(deltas.len());
    for delta in deltas {
        if let Some(previous) = merged
            .last_mut()
            .filter(|p| p.ord == delta.ord && p.token == delta.token)
        {
            previous.delta = (BigInt::from_signed_bytes_be(&previous.delta) +
                BigInt::from_signed_bytes_be(&delta.delta))
            .to_signed_bytes_be();
        } else {
            merged.push(delta);
        }
    }
    Ok(BlockBalanceDeltas { balance_deltas: merged })
}

#[substreams::handlers::map]
pub fn map_balances(
    params: String,
    block: Block,
    changes: StoreDeltas,
    keys: StoreGetRaw,
) -> Result<BlockBalanceDeltas> {
    let config = Config::parse(&params)?;
    balance_deltas(&config, &block, &addresses(keys.get_last("pairs")), &changes, &|token| {
        BalanceOf { owner: config.custodian.to_vec() }
            .call(token.to_vec())
            .ok_or_else(|| anyhow::anyhow!("BaiBai initial balanceOf failed for {token}"))
    })
}

#[substreams::handlers::store]
pub fn store_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    store_balance_changes(deltas, store);
}

fn attribute(name: &str, value: Vec<u8>, creation: bool) -> Attribute {
    Attribute {
        name: name.into(),
        value,
        change: if creation { ChangeType::Creation } else { ChangeType::Update }.into(),
    }
}

fn snapshot(
    config: &Config,
    base: Address,
    ordinal: u64,
    state: ReadStore<'_>,
    keys: ReadStore<'_>,
) -> Vec<Attribute> {
    let mut attrs: Vec<_> = config
        .slots(base)
        .into_iter()
        .enumerate()
        .map(|(i, (address, slot))| {
            attribute(
                &format!("word_{i}"),
                state(ordinal, &word_key(address, slot)).unwrap_or_else(|| vec![0; 32]),
                true,
            )
        })
        .collect();
    attrs.push(attribute("balance_owner", config.custodian.to_vec(), true));
    for fee_base in [base, Address::ZERO] {
        for taker in (0..=u8::MAX).flat_map(|shard| {
            addresses(keys(ordinal, &fees_key(&format!("{fee_base:x}"), &format!("{shard:02x}"))))
        }) {
            let value = state(ordinal, &format!("fee:{fee_base:x}:{taker:x}")).unwrap();
            attrs.push(attribute(
                &format!("{}_fee_{taker:x}", if fee_base.is_zero() { "taker" } else { "pair" }),
                value,
                true,
            ));
        }
    }
    attrs
}

fn take<'a>(deltas: &mut &'a [StoreDelta], end: u64) -> &'a [StoreDelta] {
    let (head, rest) = deltas.split_at(deltas.partition_point(|d| d.ordinal <= end));
    *deltas = rest;
    head
}

/// Emits each transaction's changes for the bases listed by its end.
#[allow(clippy::too_many_arguments)]
fn protocol_changes(
    config: &Config,
    block: &Block,
    bases: &[Address],
    changes: StoreDeltas,
    balance_changes: StoreDeltas,
    state: ReadStore<'_>,
    keys: ReadStore<'_>,
    balances: ReadStore<'_>,
) -> Result<BlockChanges> {
    let listed = listings(&changes);
    let mut slots: HashMap<String, Vec<(Address, usize)>> = HashMap::new();
    for &base in bases {
        for (i, (address, slot)) in config
            .slots(base)
            .into_iter()
            .enumerate()
        {
            slots
                .entry(word_key(address, slot))
                .or_default()
                .push((base, i));
        }
    }
    // Both delta lists are in ordinal order; consume each transaction's prefix.
    let (mut changes, mut balance_changes) = (&changes.deltas[..], &balance_changes.deltas[..]);
    let mut output = Vec::new();
    for tx in block.transactions() {
        let (deltas, balance_deltas) =
            (take(&mut changes, tx.end_ordinal), take(&mut balance_changes, tx.end_ordinal));
        if deltas.is_empty() && balance_deltas.is_empty() {
            continue;
        }
        let paused = deltas.iter().any(|d| d.key == "paused");
        let mut attrs: BTreeMap<Address, BTreeMap<String, Vec<u8>>> = BTreeMap::new();
        for delta in deltas {
            if let Some(fee) = delta.key.strip_prefix("fee:") {
                let (base, taker) = fee.split_once(':').unwrap();
                let base: Address = base.parse()?;
                let name = format!("{}_fee_{taker}", if base.is_zero() { "taker" } else { "pair" });
                for &target in bases
                    .iter()
                    .filter(|b| base.is_zero() || **b == base)
                {
                    attrs
                        .entry(target)
                        .or_default()
                        .insert(name.clone(), delta.new_value.clone());
                }
            } else if let Some(targets) = slots.get(&delta.key) {
                for &(base, i) in targets {
                    attrs
                        .entry(base)
                        .or_default()
                        .insert(format!("word_{i}"), delta.new_value.clone());
                }
            }
        }
        // The last delta per key is its value at the end of this transaction.
        let custody: HashMap<_, _> = balance_deltas
            .iter()
            .map(|d| (d.key.as_str(), &d.new_value))
            .collect();
        let mut builder = TransactionChangesBuilder::new(&tx.into());
        for &base in bases {
            let listing = listed.get(&base).copied();
            if listing.is_some_and(|ord| ord > tx.end_ordinal) {
                continue;
            }
            let id = config.id(base);
            let creation = listing.is_some_and(|ord| ord >= tx.begin_ordinal);
            if creation {
                builder.add_protocol_component(
                    &ProtocolComponent::new(&id)
                        .with_tokens(&[base, config.quote])
                        .as_swap_type("baibai_pool", ImplementationType::Custom)
                        .with_attributes(&[
                            ("base", base.to_vec()),
                            ("quote", config.quote.to_vec()),
                            ("custodian", config.custodian.to_vec()),
                        ]),
                );
                builder.add_entity_change(&EntityChanges {
                    component_id: id.clone(),
                    attributes: snapshot(config, base, tx.end_ordinal, state, keys),
                });
            } else if let Some(values) = attrs.remove(&base) {
                builder.add_entity_change(&EntityChanges {
                    component_id: id.clone(),
                    attributes: values
                        .into_iter()
                        .map(|(k, v)| attribute(&k, v, false))
                        .collect(),
                });
            }
            if paused || (creation && state(tx.end_ordinal, "paused").is_some()) {
                builder.change_component_pause_state(&id, true);
            }
            for token in [base, config.quote] {
                let key = balance_key(config, token);
                let value = match custody.get(key.as_str()) {
                    Some(value) => value.to_vec(),
                    None if creation => balances(tx.end_ordinal, &key)
                        .ok_or_else(|| anyhow::anyhow!("missing custody balance for {token}"))?,
                    None => continue,
                };
                let balance: BigInt = String::from_utf8(value)?.parse()?;
                anyhow::ensure!(balance >= BigInt::zero(), "negative custody balance");
                builder.add_balance_change(&BalanceChange {
                    token: token.to_vec(),
                    balance: balance.to_bytes_be().1,
                    component_id: id.clone().into_bytes(),
                });
            }
        }
        if let Some(change) = builder.build() {
            output.push(change);
        }
    }
    Ok(BlockChanges { block: Some(block.into()), changes: output, storage_changes: vec![] })
}

#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: Block,
    changes: StoreDeltas,
    balance_changes: StoreDeltas,
    state: StoreGetRaw,
    keys: StoreGetRaw,
    balances: StoreGetRaw,
) -> Result<BlockChanges> {
    protocol_changes(
        &Config::parse(&params)?,
        &block,
        &addresses(keys.get_last("pairs")),
        changes,
        balance_changes,
        &|ord, key| state.get_at(ord, key),
        &|ord, key| keys.get_at(ord, key),
        &|ord, key| balances.get_at(ord, key),
    )
}

#[cfg(test)]
mod tests;
