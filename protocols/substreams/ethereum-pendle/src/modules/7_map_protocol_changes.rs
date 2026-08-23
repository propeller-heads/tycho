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
use tycho_substreams::{abi::erc20, balances::aggregate_balances_changes, prelude::*};

use crate::{
    abi::pendle_sy,
    fees::{fan_out, fee_scope, read_market_fees, LN_FEE_RATE_ROOT, RESERVE_FEE_PERCENT},
    keys::{contract_id, market_by_yt_key, market_tokens_key, py_index_key, MARKET_REGISTRY},
    market_state::{
        last_ln_implied_rate, py_index_stored, LAST_LN_IMPLIED_RATE, PY_INDEX_STORED, TOTAL_PT,
        TOTAL_SY,
    },
    registry::{live_markets, MarketEntry},
    sy_rates::{
        block_timestamp, py_index_current, stale_flag, RefreshParams, BLOCK_TIMESTAMP,
        PY_INDEX_CURRENT, SY_EXCHANGE_RATE, SY_RATE_STALE,
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
        refresh_live_state(&block, &refresh, &registry_store, &py_index_store, &components_store)
    {
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_entity_change(&change);
    }

    for (tx, change) in fee_changes(&block, &new_components, &registry_store) {
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

    // After the transfer-derived balances, so the read value wins for an SY holding a rebasing
    // token.
    for (tx, change) in sy_balances(&block, &refresh, &registry_store, &components_store) {
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_balance_change(&change);
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

/// Republishes the state that moves without a Pendle event: the PY index and the clock.
///
/// The SY exchange rate has no event stream, so this is the one place the package reads chain
/// state per block rather than per market lifetime. It is one batched `eth_call` for the whole
/// protocol: the live markets are deduped down to their SYs first, and one SY backs several
/// expiries.
///
/// `block_timestamp` rides along because the curve depends on it through `rateScalar`,
/// `rateAnchor` and `feeRate` — a quote is only valid for the timestamp it was computed for, and
/// a market that has not traded for a day would otherwise be quoted on a day-old clock. Emission
/// stops at expiry, which `live_markets` already decides.
///
/// A rate that does not resolve — a paused SY, a wrapped protocol reverting — leaves
/// `py_index_current` untouched at its previous value and raises `sy_rate_stale` instead of
/// publishing a guess.
///
/// The changes are anchored to the block's last transaction. Every `EntityChanges` reaches the
/// indexer inside a `TransactionChanges`, but a refresh has no transaction that caused it, and a
/// fabricated hash would be persisted as though it were real. The last transaction is a genuine
/// one and orders the refresh after everything that actually happened in the block.
/// Reads each SY component's real token balances, rather than accumulating them from transfers.
///
/// A share-based token moves a holder's balance on rebase without emitting `Transfer`, so no
/// event-derived accounting can follow it. The wstETH SY holds **stETH**, and the drift is real
/// and measured: at block 16032658 the transfer-derived balance was `0` where the node reported
/// `4` wei. Economically nothing, but the indexer reconciles balances against `balanceOf` for
/// exact equality, so it is a mismatch.
///
/// Markets are deliberately left on the transfer path. A market custodies only SY shares and PT,
/// neither of which rebases, so transfers are exact there and cost no RPC.
///
/// Costs one `balanceOf` per (SY component, declared token) per refresh block — around 90 calls
/// against the ~60 the rate refresh already makes. `sy_rate_refresh_blocks` throttles both.
fn sy_balances(
    block: &eth::Block,
    params: &RefreshParams,
    registry_store: &StoreGetString,
    components_store: &StoreGetRaw,
) -> Vec<(Transaction, BalanceChange)> {
    if !params.should_refresh(block.number) {
        return vec![];
    }
    let Some(registry) = registry_store.get_last(MARKET_REGISTRY) else { return vec![] };
    let markets = live_markets(&registry, block.timestamp_seconds());
    let Some(anchor) = block.transactions().last() else { return vec![] };
    let anchor: Transaction = anchor.into();

    let mut holdings: Vec<(String, Vec<u8>, Vec<u8>)> = Vec::new();
    for sy in markets.iter().map(|market| &market.sy) {
        if holdings
            .iter()
            .any(|(id, _, _)| id == sy)
        {
            continue;
        }
        let Some(tokens) = components_store.get_last(sy) else { continue };
        let tokens: Vec<Vec<u8>> = serde_sibor::from_bytes(&tokens)
            .expect("deserializing component tokens from the component store");
        let address = hex::decode(sy.trim_start_matches("0x"))
            .expect("registry holds a non-hex SY component id");
        for token in tokens {
            holdings.push((sy.clone(), address.clone(), token));
        }
    }
    if holdings.is_empty() {
        return vec![];
    }

    let mut batch = RpcBatch::new();
    for (_, owner, token) in &holdings {
        batch = batch.add(erc20::functions::BalanceOf { owner: owner.clone() }, token.clone());
    }
    let responses = batch
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let mut changes = Vec::new();
    for ((component_id, _, token), response) in holdings
        .into_iter()
        .zip(responses.iter())
    {
        let Some(balance) = RpcBatch::decode::<_, erc20::functions::BalanceOf>(response) else {
            log::info!("SY {} did not answer balanceOf, leaving the balance stale", component_id);
            continue;
        };
        changes.push((
            anchor.clone(),
            BalanceChange {
                token,
                balance: balance.to_signed_bytes_be(),
                component_id: component_id.into_bytes(),
            },
        ));
    }
    changes
}

/// Re-reads the fee of every market a fee event moved, plus every market created this block.
///
/// The fee is configuration, not market state: `readState()` fetches it from the factory on every
/// call, and the override is keyed by the calling router. It changes rarely and never silently —
/// each generation announces it — so it is read on those events rather than on every block.
///
/// Only the *scope* is decoded from the event; the value is read back from the factory. Each
/// generation resolves its fee differently (see `crate::fees`), and re-deriving that in Rust is
/// how a fee ends up subtly wrong for one factory.
///
/// Unlike the SY refresh, these changes are anchored to the transaction that actually caused
/// them: the creation, or the fee event.
fn fee_changes(
    block: &eth::Block,
    new_components: &BlockTransactionProtocolComponents,
    registry_store: &StoreGetString,
) -> Vec<(Transaction, EntityChanges)> {
    let Some(registry) = registry_store.get_last(MARKET_REGISTRY) else { return vec![] };
    let markets = live_markets(&registry, block.timestamp_seconds());
    if markets.is_empty() {
        return vec![];
    }

    // A market can be hit twice in one block — created, then caught by a factory-wide event. The
    // later transaction is the one whose state should stand, and the fee is read once either way.
    let mut targets: Vec<(Transaction, &MarketEntry)> = Vec::new();

    for tx_components in &new_components.tx_components {
        let Some(tx) = tx_components.tx.as_ref() else { continue };
        for component in &tx_components.components {
            let Some(entry) = markets
                .iter()
                .find(|m| m.id == component.id)
            else {
                continue;
            };
            targets.push((tx.clone(), entry));
        }
    }

    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let Some(scope) = fee_scope(log) else { continue };
            let transaction: Transaction = tx.into();
            for entry in fan_out(&scope, &markets) {
                targets.push((transaction.clone(), entry));
            }
        }
    }

    let mut deduped: Vec<(Transaction, &MarketEntry)> = Vec::new();
    for (tx, entry) in targets {
        match deduped
            .iter_mut()
            .find(|(_, existing)| existing.id == entry.id)
        {
            Some(slot) if slot.0.index <= tx.index => slot.0 = tx,
            Some(_) => {}
            None => deduped.push((tx, entry)),
        }
    }
    if deduped.is_empty() {
        return vec![];
    }

    let entries: Vec<MarketEntry> = deduped
        .iter()
        .map(|(_, entry)| (*entry).clone())
        .collect();
    read_market_fees(&entries)
        .into_iter()
        .filter_map(|(id, fee)| {
            let (tx, _) = deduped
                .iter()
                .find(|(_, e)| e.id == id)?;
            Some((
                tx.clone(),
                EntityChanges {
                    component_id: id,
                    attributes: vec![
                        state_attribute(
                            LN_FEE_RATE_ROOT,
                            fee.ln_fee_rate_root
                                .to_signed_bytes_be(),
                        ),
                        state_attribute(
                            RESERVE_FEE_PERCENT,
                            fee.reserve_fee_percent
                                .to_signed_bytes_be(),
                        ),
                    ],
                },
            ))
        })
        .collect()
}

fn refresh_live_state(
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
    let timestamp = block_timestamp(block.timestamp_seconds());
    let mut changes = Vec::new();

    for market in &markets {
        let rate = rates
            .get(&market.sy)
            .and_then(Option::as_ref);
        let mut attributes = vec![
            state_attribute(BLOCK_TIMESTAMP, timestamp.clone()),
            state_attribute(SY_RATE_STALE, stale_flag(rate.is_none())),
        ];
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
        let mut attributes = vec![
            state_attribute(BLOCK_TIMESTAMP, timestamp.clone()),
            state_attribute(SY_RATE_STALE, stale_flag(rate.is_none())),
        ];
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
