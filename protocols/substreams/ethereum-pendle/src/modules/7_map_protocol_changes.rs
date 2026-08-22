use anyhow::Result;
use itertools::Itertools;
use std::collections::{BTreeMap, HashMap};
use substreams::{
    log,
    pb::substreams::StoreDeltas,
    scalar::BigInt,
    store::{StoreGet, StoreGetBigInt, StoreGetRaw, StoreGetString},
};
use substreams_ethereum::{pb::eth::v2 as eth, rpc::RpcBatch};
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

use crate::{
    abi::pendle_sy,
    keys::{contract_id, market_by_yt_key, market_tokens_key, py_index_key, MARKET_REGISTRY},
    market_state::{
        last_ln_implied_rate, py_index_stored, LAST_LN_IMPLIED_RATE, PY_INDEX_STORED, TOTAL_PT,
        TOTAL_SY,
    },
    registry::{live_markets, MarketEntry},
    sy_rates::{
        py_index_current, stale_flag, RefreshParams, PY_INDEX_CURRENT, SY_EXCHANGE_RATE,
        SY_RATE_STALE,
    },
};

/// Joins new components, market state and balance changes into the per-transaction output.
#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::Block,
    new_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    balance_store: StoreDeltas,
    reserve_deltas: BlockBalanceDeltas,
    reserve_store: StoreDeltas,
    components_store: StoreGetRaw,
    registry_store: StoreGetString,
    py_index_store: StoreGetBigInt,
) -> Result<BlockChanges, substreams::errors::Error> {
    let refresh = RefreshParams::parse(&params)?;
    let mut transaction_changes: HashMap<u64, TransactionChangesBuilder> = HashMap::new();

    for tx_components in &new_components.tx_components {
        let tx = tx_components
            .tx
            .as_ref()
            .expect("component batch without a transaction");
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        for component in &tx_components.components {
            builder.add_protocol_component(component);
            // The creation-time `pyIndexStored()` read lands here rather than in the component's
            // static attributes: it is state, and it changes on the yield token's next interest
            // event.
            if let Some(index) = py_index_store.get_last(py_index_key(&component.id)) {
                builder.add_entity_change(&EntityChanges {
                    component_id: component.id.clone(),
                    attributes: vec![state_attribute(PY_INDEX_STORED, index.to_signed_bytes_be())],
                });
            }
        }
    }

    // Reserves arrive as accumulated absolutes keyed by the PT and SY token addresses; the market's
    // role list is what turns those back into named attributes.
    for (_, (tx, balances)) in aggregate_balances_changes(reserve_store, reserve_deltas) {
        for (component_id, token_balances) in &balances {
            let component_id = String::from_utf8(component_id.clone())
                .expect("reserve component id is not valid utf-8");
            let Some((sy, pt)) = market_sy_pt(&components_store, &component_id) else { continue };

            let attributes = token_balances
                .values()
                .filter_map(|change| {
                    let name = match &change.token {
                        token if *token == pt => TOTAL_PT,
                        token if *token == sy => TOTAL_SY,
                        _ => return None,
                    };
                    Some(state_attribute(name, change.balance.clone()))
                })
                .collect::<Vec<_>>();
            if attributes.is_empty() {
                continue;
            }

            transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx))
                .add_entity_change(&EntityChanges { component_id, attributes });
        }
    }

    for (tx, change) in absolute_state_changes(&block, &components_store) {
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_entity_change(&change);
    }

    for (tx, change) in
        refresh_sy_rates(&block, &refresh, &registry_store, &py_index_store, &components_store)
    {
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_entity_change(&change);
    }

    for (_, (tx, balances)) in aggregate_balances_changes(balance_store, deltas) {
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for token_balances in balances.values() {
            for balance_change in token_balances.values() {
                builder.add_balance_change(balance_change);
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
        // Raw storage is only consumed by the Dynamic Contract Indexer. Pendle is a native
        // integration: everything the simulation needs is an explicit attribute here.
        storage_changes: vec![],
    })
}

/// Collects the state attributes that events report in full rather than as a change.
///
/// `UpdateImpliedRate` comes from the market and `NewInterestIndex` from its yield token, which is
/// not itself a component — hence the reverse lookup. Logs are walked in block order, so a market
/// touched twice in one transaction ends on its last value.
fn absolute_state_changes(
    block: &eth::Block,
    components_store: &StoreGetRaw,
) -> Vec<(Transaction, EntityChanges)> {
    let mut changes = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let entry = if let Some(rate) = last_ln_implied_rate(log) {
                let component_id = contract_id(&log.address);
                components_store
                    .get_last(market_tokens_key(&component_id))
                    .map(|_| (component_id, LAST_LN_IMPLIED_RATE, rate))
            } else if let Some(index) = py_index_stored(log) {
                components_store
                    .get_last(market_by_yt_key(&log.address))
                    .map(|id| {
                        let component_id =
                            String::from_utf8(id).expect("market id is not valid utf-8");
                        (component_id, PY_INDEX_STORED, index)
                    })
            } else {
                continue;
            };

            let Some((component_id, name, value)) = entry else { continue };
            changes.push((
                tx.into(),
                EntityChanges {
                    component_id,
                    attributes: vec![state_attribute(name, value.to_signed_bytes_be())],
                },
            ));
        }
    }
    changes
}

/// Re-reads every live SY's `exchangeRate()` and republishes the PY index it implies.
///
/// The rate has no event stream, so this is the one place the package reads chain state per
/// block rather than per market lifetime. It is one batched `eth_call` for the whole protocol:
/// the live markets are deduped down to their SYs first, and one SY backs several expiries.
///
/// A rate that does not resolve — a paused SY, a wrapped protocol reverting — leaves
/// `py_index_current` untouched at its previous value and raises `sy_rate_stale` instead of
/// publishing a guess.
///
/// The changes are anchored to the block's last transaction. Every `EntityChanges` reaches the
/// indexer inside a `TransactionChanges`, but a refresh has no transaction that caused it, and a
/// fabricated hash would be persisted as though it were real. The last transaction is a genuine
/// one and orders the refresh after everything that actually happened in the block.
fn refresh_sy_rates(
    block: &eth::Block,
    params: &RefreshParams,
    registry_store: &StoreGetString,
    py_index_store: &StoreGetBigInt,
    components_store: &StoreGetRaw,
) -> Vec<(Transaction, EntityChanges)> {
    if !params.should_refresh(block.number) {
        return vec![];
    }
    let Some(registry) = registry_store.get_last(MARKET_REGISTRY) else { return vec![] };
    let markets = live_markets(&registry, block.timestamp_seconds());
    if markets.is_empty() {
        return vec![];
    }
    let Some(anchor) = block.transactions().last() else {
        log::info!("block {} holds no transactions, skipping the SY rate refresh", block.number);
        return vec![];
    };
    let anchor: Transaction = anchor.into();

    let rates = read_exchange_rates(&markets);
    let mut changes = Vec::new();

    for market in &markets {
        let rate = rates
            .get(&market.sy)
            .and_then(Option::as_ref);
        let mut attributes = vec![state_attribute(SY_RATE_STALE, stale_flag(rate.is_none()))];
        if let Some(rate) = rate {
            let stored = py_index_store.get_last(py_index_key(&market.id));
            attributes.push(state_attribute(
                PY_INDEX_CURRENT,
                py_index_current(stored, rate).to_signed_bytes_be(),
            ));
        }
        changes
            .push((anchor.clone(), EntityChanges { component_id: market.id.clone(), attributes }));
    }

    // Not every SY is a component: one whose conversions neither closed form explains contributes
    // no wrap edges, and emitting state for it would be state on a component that never existed.
    for (sy, rate) in &rates {
        if !components_store.has_last(sy) {
            continue;
        }
        let mut attributes = vec![state_attribute(SY_RATE_STALE, stale_flag(rate.is_none()))];
        if let Some(rate) = rate {
            attributes.push(state_attribute(SY_EXCHANGE_RATE, rate.to_signed_bytes_be()));
        }
        changes.push((anchor.clone(), EntityChanges { component_id: sy.clone(), attributes }));
    }

    changes
}

/// Reads `exchangeRate()` for the SY behind each live market, deduped, in one batch.
///
/// Every SY in the input appears in the result; the value is `None` where the call failed.
fn read_exchange_rates(markets: &[MarketEntry]) -> BTreeMap<String, Option<BigInt>> {
    let mut sy_ids: Vec<&String> = Vec::new();
    for market in markets {
        if !sy_ids.contains(&&market.sy) {
            sy_ids.push(&market.sy);
        }
    }

    let mut batch = RpcBatch::new();
    for sy in &sy_ids {
        let address = hex::decode(sy.trim_start_matches("0x"))
            .unwrap_or_else(|_| panic!("registry holds a non-hex SY id {sy}"));
        batch = batch.add(pendle_sy::functions::ExchangeRate {}, address);
    }
    let responses = batch
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let mut rates = BTreeMap::new();
    for (index, sy) in sy_ids.into_iter().enumerate() {
        let rate = responses
            .get(index)
            .and_then(RpcBatch::decode::<_, pendle_sy::functions::ExchangeRate>);
        if rate.is_none() {
            log::info!("SY {} did not answer exchangeRate(), marking it stale", sy);
        }
        rates.insert(sy.clone(), rate);
    }
    rates
}

/// Returns a market's SY and PT addresses, or `None` if the id names something that is not a
/// market.
fn market_sy_pt(store: &StoreGetRaw, component_id: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let roles = store.get_last(market_tokens_key(component_id))?;
    let roles: Vec<Vec<u8>> = serde_sibor::from_bytes(&roles)
        .expect("deserializing market token roles from the component store");
    let [sy, pt, _yt] = roles.as_slice() else {
        panic!("market {component_id} stored {} roles, expected 3", roles.len())
    };
    Some((sy.clone(), pt.clone()))
}

fn state_attribute(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_string(), value, change: ChangeType::Update.into() }
}
