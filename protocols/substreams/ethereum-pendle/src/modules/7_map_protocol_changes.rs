use anyhow::Result;
use itertools::Itertools;
use std::collections::{BTreeMap, HashMap, HashSet};
use substreams::{
    log,
    pb::substreams::StoreDeltas,
    scalar::BigInt,
    store::{StoreGet, StoreGetBigInt, StoreGetInt64, StoreGetRaw, StoreGetString},
};
use substreams_ethereum::{pb::eth::v2 as eth, rpc::RpcBatch};
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

use crate::{
    abi::pendle_sy,
    entry_fee::{fee_change, ENTRY_FEE_BPS, ENTRY_FEE_FROM, FEE_BPS_KEY, FEE_FROM_KEY, PAYERS_KEY},
    fees::{
        fan_out, fee_scope, read_market_fees, MarketFee, LN_FEE_RATE_ROOT, RESERVE_FEE_PERCENT,
    },
    keys::{
        contract_id, market_tokens_key, markets_by_yt_key, py_index_key, sy_rate_needed_key,
        MARKET_REGISTRY,
    },
    market_state::{
        last_ln_implied_rate, py_index_stored, LAST_LN_IMPLIED_RATE, PY_INDEX_STORED, TOTAL_PT,
        TOTAL_SY,
    },
    registry::{expired_markets, live_markets, markets_of_yt, sy_components, MarketEntry},
    shares::{
        is_share_holder, ratio_pairs, share_balance_key, share_holders, share_ratio_key, ShareRatio,
    },
    sy::{is_class_attribute, is_creation_seed, supply_seed, SY_TOTAL_SUPPLY},
    sy_rates::{
        encode_timestamp, py_index_current, RefreshParams, BLOCK_TIMESTAMP, PY_INDEX_CURRENT,
        RATE_SAMPLED_AT, SY_EXCHANGE_RATE,
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
    sy_state: BlockChanges,
    rate_needed_store: StoreGetInt64,
    markets_by_yt: StoreGetString,
    share_ratios: StoreGetString,
    share_holders_store: StoreGetString,
    share_balances: StoreGetBigInt,
    entry_fee: StoreGetBigInt,
    entry_fee_payers: StoreGetString,
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
            builder.add_protocol_component(&as_emitted(component));
            if let Some(change) = creation_supply(component) {
                builder.add_entity_change(&change);
            }
            if let Some((_, _, yt)) = market_roles(&components_store, &component.id) {
                builder.add_balance_change(&yt_activity(&component.id, yt));
            }
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

    // A new SY's classes, pause flag and supply cap, a new market's `sy_paused`, and every later
    // pause, unpause, cap update and re-classification. They follow the component creation of the
    // same transaction, which they describe.
    for tx_changes in sy_state.changes {
        let tx = tx_changes
            .tx
            .expect("SY state change without a transaction");
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for change in &tx_changes.entity_changes {
            builder.add_entity_change(change);
        }
    }

    // Reserves arrive as accumulated absolutes keyed by the PT and SY token addresses; the market's
    // role list is what turns those back into named attributes.
    for (_, (tx, balances)) in aggregate_balances_changes(reserve_store, reserve_deltas) {
        for (component_id, token_balances) in &balances {
            let component_id = String::from_utf8(component_id.clone())
                .expect("reserve component id is not valid utf-8");
            let Some((sy, pt, yt)) = market_roles(&components_store, &component_id) else {
                // Not a market, so an SY, whose one reserve is its own supply.
                let own_address = hex::decode(component_id.trim_start_matches("0x"))
                    .expect("SY component id is not hex");
                if let Some(change) = token_balances.get(&own_address) {
                    transaction_changes
                        .entry(tx.index)
                        .or_insert_with(|| TransactionChangesBuilder::new(&tx))
                        .add_entity_change(&EntityChanges {
                            component_id,
                            attributes: vec![state_attribute(
                                SY_TOTAL_SUPPLY,
                                signed_bytes(&change.balance),
                            )],
                        });
                }
                continue;
            };

            let attributes = token_balances
                .values()
                .filter_map(|change| {
                    let name = match &change.token {
                        token if *token == pt => TOTAL_PT,
                        token if *token == sy => TOTAL_SY,
                        _ => return None,
                    };
                    Some(state_attribute(name, signed_bytes(&change.balance)))
                })
                .collect::<Vec<_>>();
            if attributes.is_empty() {
                continue;
            }

            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            builder.add_balance_change(&yt_activity(&component_id, yt));
            builder.add_entity_change(&EntityChanges { component_id, attributes });
        }
    }

    let created: HashSet<String> = new_components
        .tx_components
        .iter()
        .flat_map(|tx| &tx.components)
        .map(|component| component.id.clone())
        .collect();
    let is_market = |id: &str| components_store.has_last(market_tokens_key(id));
    let backed_by = |yt: &[u8]| markets_backed_by(&markets_by_yt, yt);
    for (tx, change) in absolute_state_changes(&block, is_market, backed_by, &created) {
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_entity_change(&change);
    }

    let forced = forced_refresh(&block, &new_components, backed_by);
    for (tx, change) in refresh_live_state(
        &block,
        &refresh,
        &forced,
        &registry_store,
        &py_index_store,
        &components_store,
        &rate_needed_store,
    ) {
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_entity_change(&change);
    }

    let registry = || registry_store.get_last(MARKET_REGISTRY);
    for (tx, change) in fee_changes(&block, &new_components, registry, read_market_fees) {
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_entity_change(&change);
    }

    for (tx, change) in entry_fee_changes(&block, &new_components, &entry_fee, &entry_fee_payers) {
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_entity_change(&change);
    }

    // A balance kept in shares leaves the store in shares and is emitted at the token's ratio.
    let ratio = |token: &[u8]| {
        share_ratios
            .get_last(share_ratio_key(token))
            .and_then(|value| ShareRatio::decode(&value))
    };
    let mut valued_in_shares: HashSet<(Vec<u8>, Vec<u8>)> = HashSet::new();
    for (_, (tx, balances)) in aggregate_balances_changes(balance_store, deltas) {
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for token_balances in balances.values() {
            for balance_change in token_balances.values() {
                let component_id = String::from_utf8(balance_change.component_id.clone())
                    .expect("component id is not utf-8");
                if !is_share_holder(&share_holders_store, &component_id, &balance_change.token) {
                    builder.add_balance_change(balance_change);
                    continue;
                }
                let ratio = ratio(&balance_change.token)
                    .expect("an SY holds a token in shares only once it has a ratio");
                let shares = BigInt::from_unsigned_bytes_be(&balance_change.balance);
                builder.add_balance_change(&BalanceChange {
                    balance: ratio.to_amount(&shares).to_bytes_be().1,
                    ..balance_change.clone()
                });
                valued_in_shares
                    .insert((balance_change.component_id.clone(), balance_change.token.clone()));
            }
        }
    }
    for (tx, change) in revalued_share_balances(
        &block,
        &share_ratios,
        &share_holders_store,
        &share_balances,
        &valued_in_shares,
    ) {
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
/// not itself a component — hence the reverse lookup. Logs are walked call by call, not by log
/// ordinal, but markets and yield tokens are non-reentrant, so one contract's logs in a
/// transaction still come in order and a market touched twice ends on its last value.
fn absolute_state_changes(
    block: &eth::Block,
    is_market: impl Fn(&str) -> bool,
    backed_by: impl Fn(&[u8]) -> Vec<String>,
    created: &HashSet<String>,
) -> Vec<(Transaction, EntityChanges)> {
    let mut changes = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let entries = if let Some(rate) = last_ln_implied_rate(log) {
                let component_id = contract_id(&log.address);
                if is_market(&component_id) {
                    vec![(component_id, LAST_LN_IMPLIED_RATE, rate)]
                } else {
                    vec![]
                }
            } else if let Some(index) = py_index_stored(log) {
                // The index belongs to the yield token, so every market it backs takes it. A market
                // created this block is left out: its creation carries the end-of-block value, and
                // an earlier transaction would otherwise update it before it exists.
                backed_by(&log.address)
                    .into_iter()
                    .filter(|market| !created.contains(market))
                    .map(|market| (market, PY_INDEX_STORED, index.clone()))
                    .collect()
            } else {
                continue;
            };

            for (component_id, name, value) in entries {
                changes.push((
                    tx.into(),
                    EntityChanges {
                        component_id,
                        attributes: vec![state_attribute(name, value.to_signed_bytes_be())],
                    },
                ));
            }
        }
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
///
/// The registry is fetched and parsed only on a block that created a component or emitted a fee
/// event, which most blocks do not.
fn fee_changes(
    block: &eth::Block,
    new_components: &BlockTransactionProtocolComponents,
    registry: impl FnOnce() -> Option<String>,
    read_fees: impl FnOnce(&[MarketEntry]) -> Vec<(String, MarketFee)>,
) -> Vec<(Transaction, EntityChanges)> {
    let mut fee_events = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            if let Some(scope) = fee_scope(log) {
                fee_events.push((Transaction::from(tx), scope));
            }
        }
    }
    let created_any = new_components
        .tx_components
        .iter()
        .any(|tx| !tx.components.is_empty());
    if fee_events.is_empty() && !created_any {
        return vec![];
    }
    let Some(registry) = registry() else { return vec![] };
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

    for (transaction, scope) in &fee_events {
        for entry in fan_out(scope, &markets) {
            targets.push((transaction.clone(), entry));
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
    read_fees(&entries)
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

/// The state [`refreshed_state`] republishes, read from the stores and the chain. Off a refresh
/// block only the components in `forced` are in scope, and with none of those nothing is read at
/// all.
fn refresh_live_state(
    block: &eth::Block,
    params: &RefreshParams,
    forced: &HashSet<String>,
    registry_store: &StoreGetString,
    py_index_store: &StoreGetBigInt,
    components_store: &StoreGetRaw,
    rate_needed_store: &StoreGetInt64,
) -> Vec<(Transaction, EntityChanges)> {
    let refreshing = params.should_refresh(block.number);
    if !refreshing && forced.is_empty() {
        return vec![];
    }
    let Some(registry) = registry_store.get_last(MARKET_REGISTRY) else { return vec![] };
    refreshed_state(
        block,
        &registry,
        |id: &str| refreshing || forced.contains(id),
        |id: &str| components_store.has_last(id),
        |sy: &str| {
            rate_needed_store
                .get_last(sy_rate_needed_key(sy))
                .is_some_and(|needed| needed != 0)
        },
        |market: &str| py_index_store.get_last(py_index_key(market)),
        read_exchange_rates,
    )
}

/// Republishes the state that moves without a Pendle event: the PY index and the clock.
///
/// The SY exchange rate has no event stream, so this is the one place the package reads chain
/// state per refresh block rather than per market lifetime or per event. It is one batched
/// `eth_call` for the whole protocol, over the SYs rather than the markets — one SY backs several
/// expiries.
///
/// Expiry is a market's property, never its SY's: an ERC-5115 wrapper has no maturity and goes on
/// wrapping after every market that priced against it has died. So the two are refreshed on
/// different rules — a market's state stops at its expiry, and an SY's rate is read for as long as
/// a quote needs it (see [`rate_targets`]).
///
/// The clocks ride along because the curve depends on time through `rateScalar`, `rateAnchor` and
/// `feeRate` — a quote is only valid for the timestamp it was computed for, and a market that has
/// not traded for a day would otherwise be quoted on a day-old clock.
///
/// A rate that does not resolve — a paused SY, a wrapped protocol reverting — leaves both
/// `py_index_current` and `rate_sampled_at` untouched rather than publishing a guess.
/// `block_timestamp` is still emitted, and that is the point: it advances past the rate's own
/// date, which is how a consumer sees that what it holds is no longer current.
///
/// The clock outlives the market by `EXPIRY_GRACE_SECONDS`, without the rate. A market's clock is
/// the only thing that can tell a consumer it has expired, and one that stopped being published
/// strictly below its expiry never carries a timestamp the market is dead at — so it would go on
/// quoting off a frozen clock forever. One window of clock past expiry says it, and both verdicts
/// it produces — expired, and holding a rate the chain has moved past — are permanent, so nothing
/// further is needed. No rate is read for those markets themselves — an expired market has no
/// curve left to price — though their SY is still read, on its own account.
///
/// Only the components `in_scope` accepts are refreshed. Off a refresh block those are the ones
/// [`forced_refresh`] names.
///
/// The changes are anchored to the block's last transaction. Every `EntityChanges` reaches the
/// indexer inside a `TransactionChanges`, but a refresh has no transaction that caused it, and a
/// fabricated hash would be persisted as though it were real. The last transaction is a genuine
/// one and orders the refresh after everything that actually happened in the block.
fn refreshed_state(
    block: &eth::Block,
    registry: &str,
    in_scope: impl Fn(&str) -> bool,
    is_component: impl Fn(&str) -> bool,
    rate_needed: impl Fn(&str) -> bool,
    py_index: impl Fn(&str) -> Option<BigInt>,
    read_rates: impl FnOnce(&[String]) -> BTreeMap<String, Option<BigInt>>,
) -> Vec<(Transaction, EntityChanges)> {
    let markets: Vec<MarketEntry> = live_markets(registry, block.timestamp_seconds())
        .into_iter()
        .filter(|market| in_scope(&market.id))
        .collect();
    let expired: Vec<MarketEntry> = expired_markets(registry, block.timestamp_seconds())
        .into_iter()
        .filter(|market| in_scope(&market.id))
        .collect();
    let targets = rate_targets(registry, &markets, &in_scope, &is_component, &rate_needed);
    // An SY has no expiry, so it is refreshed even when no market is live.
    if markets.is_empty() && expired.is_empty() && targets.is_empty() {
        return vec![];
    }
    let Some(anchor) = block.transactions().last() else {
        log::info!("block {} holds no transactions, skipping the SY rate refresh", block.number);
        return vec![];
    };
    let anchor: Transaction = anchor.into();

    let rates = read_rates(&targets);
    let timestamp = encode_timestamp(block.timestamp_seconds());
    let mut changes = Vec::new();

    for market in &markets {
        let rate = rates
            .get(&market.sy)
            .and_then(Option::as_ref);
        let mut attributes = vec![state_attribute(BLOCK_TIMESTAMP, timestamp.clone())];
        if let Some(rate) = rate {
            let stored = py_index(&market.id);
            attributes.push(state_attribute(RATE_SAMPLED_AT, timestamp.clone()));
            attributes.push(state_attribute(
                PY_INDEX_CURRENT,
                py_index_current(stored, rate).to_signed_bytes_be(),
            ));
        }
        changes
            .push((anchor.clone(), EntityChanges { component_id: market.id.clone(), attributes }));
    }

    for market in &expired {
        changes.push((anchor.clone(), clock_only(&market.id, &timestamp)));
    }

    // Not every SY is a component: one whose conversions neither closed form explains contributes
    // no wrap edges, and emitting state for it would be state on a component that never existed.
    for (sy, rate) in &rates {
        if !is_component(sy) {
            continue;
        }
        let mut attributes = vec![state_attribute(BLOCK_TIMESTAMP, timestamp.clone())];
        if let Some(rate) = rate {
            attributes.push(state_attribute(RATE_SAMPLED_AT, timestamp.clone()));
            attributes.push(state_attribute(SY_EXCHANGE_RATE, rate.to_signed_bytes_be()));
        }
        changes.push((anchor.clone(), EntityChanges { component_id: sy.clone(), attributes }));
    }

    changes
}

/// The components refreshed this block whether or not it is a refresh block.
///
/// Every component created this block, which has no rate yet: with `sy_rate_refresh_blocks` above
/// one, it would otherwise go unquotable until the next refresh. And every market whose yield
/// token emitted `NewInterestIndex`, since `py_index_current` is at least the new stored index and
/// would otherwise sit below it until the next refresh.
fn forced_refresh(
    block: &eth::Block,
    new_components: &BlockTransactionProtocolComponents,
    backed_by: impl Fn(&[u8]) -> Vec<String>,
) -> HashSet<String> {
    let mut forced: HashSet<String> = new_components
        .tx_components
        .iter()
        .flat_map(|tx| &tx.components)
        .map(|component| component.id.clone())
        .collect();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            if py_index_stored(log).is_none() {
                continue;
            }
            forced.extend(backed_by(&log.address));
        }
    }
    forced
}

/// The SYs whose `exchangeRate()` is read this block.
///
/// The SY of every market in `live`, whose curve prices off the rate, plus every SY component in
/// scope that has a rate-converted token. A `one_to_one` conversion never looks at the rate, so an
/// SY whose tokens are all `one_to_one` and whose markets have all expired is not read at all.
/// The SY of a live market may not be a component (the classifier could not read it in either
/// direction), yet the market still needs its rate.
fn rate_targets(
    registry: &str,
    live: &[MarketEntry],
    in_scope: impl Fn(&str) -> bool,
    is_component: impl Fn(&str) -> bool,
    rate_needed: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut targets: Vec<String> = sy_components(registry)
        .into_iter()
        .filter(|sy| in_scope(sy) && is_component(sy) && rate_needed(sy))
        .collect();
    for market in live {
        if !targets.contains(&market.sy) {
            targets.push(market.sy.clone());
        }
    }
    targets
}

/// The clock alone: this component was looked at, and no rate was read for it.
fn clock_only(component_id: &str, timestamp: &[u8]) -> EntityChanges {
    EntityChanges {
        component_id: component_id.to_string(),
        attributes: vec![state_attribute(BLOCK_TIMESTAMP, timestamp.to_vec())],
    }
}

/// Reads `exchangeRate()` for each SY in one batch.
///
/// Every SY in the input appears in the result; the value is `None` where the call failed or
/// answered zero (see [`usable_rate`]).
fn read_exchange_rates(sy_ids: &[String]) -> BTreeMap<String, Option<BigInt>> {
    let mut batch = RpcBatch::new();
    for sy in sy_ids {
        let address = hex::decode(sy.trim_start_matches("0x"))
            .unwrap_or_else(|_| panic!("registry holds a non-hex SY id {sy}"));
        batch = batch.add(pendle_sy::functions::ExchangeRate {}, address);
    }
    let responses = batch
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let mut rates = BTreeMap::new();
    for (index, sy) in sy_ids.iter().enumerate() {
        let rate = usable_rate(
            responses
                .get(index)
                .and_then(RpcBatch::decode::<_, pendle_sy::functions::ExchangeRate>),
        );
        if rate.is_none() {
            log::info!("SY {} gave no usable exchangeRate(), leaving its rate undated", sy);
        }
        rates.insert(sy.clone(), rate);
    }
    rates
}

/// An SY's supply at creation, absolute.
///
/// The supply store only reports a supply that a delta moved, and an SY created empty has none,
/// yet consumers need the attribute: it bounds every redemption.
fn creation_supply(component: &ProtocolComponent) -> Option<EntityChanges> {
    let supply = supply_seed(component)?;
    Some(EntityChanges {
        component_id: component.id.clone(),
        attributes: vec![state_attribute(SY_TOTAL_SUPPLY, supply.to_signed_bytes_be())],
    })
}

/// A rate the refresh can emit: `None` for a failed read and for zero.
///
/// An SY that is drained or deprecated can answer `exchangeRate()` with zero; one did at block
/// 25,835,592 and still does. No SY can be priced at zero, since every conversion divides by the
/// rate, so it is treated as a read that did not resolve: the rate stays undated and the clock
/// stops, and the consumer refuses to quote rather than receiving a value it must reject.
fn usable_rate(rate: Option<BigInt>) -> Option<BigInt> {
    rate.filter(|rate| *rate > BigInt::zero())
}

/// Every market the yield token at `yt` backs, or none when it backs no market.
fn markets_backed_by(markets_by_yt: &StoreGetString, yt: &[u8]) -> Vec<String> {
    markets_by_yt
        .get_last(markets_by_yt_key(yt))
        .map(|listed| markets_of_yt(&listed))
        .unwrap_or_default()
}

/// Returns a market's SY, PT and YT addresses, or `None` if the id names something that is not a
/// market.
fn market_roles(store: &StoreGetRaw, component_id: &str) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let roles = store.get_last(market_tokens_key(component_id))?;
    let roles: Vec<Vec<u8>> = serde_sibor::from_bytes(&roles)
        .expect("deserializing market token roles from the component store");
    let [sy, pt, yt] = roles.as_slice() else {
        panic!("market {component_id} stored {} roles, expected 3", roles.len())
    };
    Some((sy.clone(), pt.clone(), yt.clone()))
}

/// A zero balance of the market's YT, emitted when the market is created and whenever its
/// reserves move.
///
/// This is a workaround for how Tycho decides a token is in use, not a balance anyone needs. Tycho
/// counts a token as traded when a component balance of it changed, and clients drop tokens not
/// traded recently (by default on Ethereum, within 30 days). A market holds only SY and PT: a YT
/// trade mints or burns YT in the YT contract and moves only the market's SY and PT, so without
/// this no component ever reports a YT balance. YT would then never count as traded, every client
/// using the default token filter would drop it, and with it every market, since a market lists
/// its YT. Zero is the market's true YT balance, so the reported balances stay correct.
///
/// Emitting it on every reserve move keeps YT counted as traded while its market is active; a
/// market that does not move for 30 days loses its YT from those clients, as any idle pool's
/// tokens would. It does not fix YT's token quality: the token analyser needs a holder to transfer
/// from, and a zero balance is none.
fn yt_activity(market_id: &str, yt: Vec<u8>) -> BalanceChange {
    BalanceChange {
        token: yt,
        balance: BigInt::zero().to_bytes_be().1,
        component_id: market_id.as_bytes().to_vec(),
    }
}

/// The entry fee on each SY that pays it: on its creation, the fee as it stands; and on every SY
/// that pays it, whenever the vault's events change it, anchored to the last such event's
/// transaction.
fn entry_fee_changes(
    block: &eth::Block,
    new_components: &BlockTransactionProtocolComponents,
    entry_fee: &StoreGetBigInt,
    payers: &StoreGetString,
) -> Vec<(Transaction, EntityChanges)> {
    let payers: Vec<String> = payers
        .get_last(PAYERS_KEY)
        .map(|listed| {
            listed
                .split(';')
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .unique()
                .collect()
        })
        .unwrap_or_default();
    if payers.is_empty() {
        return vec![];
    }
    let fee = |id: &str| {
        let bps = entry_fee.get_last(FEE_BPS_KEY)?;
        let from = entry_fee
            .get_last(FEE_FROM_KEY)
            .unwrap_or_else(BigInt::zero);
        Some(EntityChanges {
            component_id: id.to_string(),
            attributes: vec![
                state_attribute(ENTRY_FEE_BPS, bps.to_signed_bytes_be()),
                state_attribute(ENTRY_FEE_FROM, from.to_signed_bytes_be()),
            ],
        })
    };
    let mut changes = Vec::new();
    let last_change = block
        .transactions()
        .filter(|tx| {
            tx.logs_with_calls()
                .any(|(log, _)| fee_change(log).is_some())
        })
        .last();
    if let Some(tx) = last_change {
        let tx: Transaction = tx.into();
        changes.extend(
            payers
                .iter()
                .filter_map(|id| fee(id).map(|change| (tx.clone(), change))),
        );
        return changes;
    }
    for tx_components in &new_components.tx_components {
        let tx = tx_components
            .tx
            .as_ref()
            .expect("component batch without a transaction");
        for component in &tx_components.components {
            if payers.contains(&component.id) {
                changes.extend(fee(&component.id).map(|change| (tx.clone(), change)));
            }
        }
    }
    changes
}

/// The balances kept in shares whose token's ratio moved this block, revalued at the new ratio.
///
/// Nothing moved those holdings, so no balance delta reports them: the ratio did, through a rebase
/// report, a fee or a multiplier update that the next transfer of the token reveals. Holdings
/// already emitted this block in `valued` are at the new ratio and left out. The changes are
/// anchored to the block's last transaction, as the rate refresh's are.
fn revalued_share_balances(
    block: &eth::Block,
    ratios: &StoreGetString,
    holders: &StoreGetString,
    share_balances: &StoreGetBigInt,
    valued: &HashSet<(Vec<u8>, Vec<u8>)>,
) -> Vec<(Transaction, BalanceChange)> {
    let Some(anchor) = block.transactions().last() else { return vec![] };
    let anchor: Transaction = anchor.into();
    let mut tokens: Vec<Vec<u8>> = Vec::new();
    for tx in block.transactions() {
        let logs: Vec<&eth::Log> = tx
            .logs_with_calls()
            .map(|(log, _)| log)
            .collect();
        for (token, _, _) in ratio_pairs(&logs) {
            if !tokens.iter().any(|seen| seen == token) {
                tokens.push(token.to_vec());
            }
        }
    }
    let mut changes = Vec::new();
    for token in tokens {
        let key = share_ratio_key(&token);
        let decode = |value: Option<String>| value.and_then(|value| ShareRatio::decode(&value));
        let (Some(before), Some(now)) =
            (decode(ratios.get_first(&key)), decode(ratios.get_last(&key)))
        else {
            continue;
        };
        if !now.moved_from(&before) {
            continue;
        }
        for holder in share_holders(holders, &token) {
            if valued.contains(&(holder.clone().into_bytes(), token.clone())) {
                continue;
            }
            let shares = share_balances
                .get_last(share_balance_key(&holder, &token))
                .unwrap_or_else(BigInt::zero);
            if shares <= BigInt::zero() {
                continue;
            }
            changes.push((
                anchor.clone(),
                BalanceChange {
                    token: token.clone(),
                    balance: now.to_amount(&shares).to_bytes_be().1,
                    component_id: holder.into_bytes(),
                },
            ));
        }
    }
    changes
}

/// The component as it is emitted: without the creation seeds, which only seed the stores, and
/// without the token classes, which `map_sy_state` emits as state so an upgrade can change them.
fn as_emitted(component: &ProtocolComponent) -> ProtocolComponent {
    let mut emitted = component.clone();
    emitted.static_att.retain(|attribute| {
        !is_creation_seed(&attribute.name) && !is_class_attribute(&attribute.name)
    });
    emitted
}

/// An accumulated reserve or supply as the signed big-endian bytes every other numeric attribute
/// uses. `aggregate_balances_changes` hands back the unsigned magnitude, whose top bit can be set,
/// which a signed reader takes as negative: 10e18 is `0x8ac7…`.
fn signed_bytes(unsigned: &[u8]) -> Vec<u8> {
    BigInt::from_unsigned_bytes_be(unsigned).to_signed_bytes_be()
}

fn state_attribute(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_string(), value, change: ChangeType::Update.into() }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// An SY created empty still carries its supply, zero, from creation; a market carries none.
    #[test]
    fn an_sy_created_empty_carries_a_zero_supply() {
        let sy =
            ProtocolComponent::new("0xcbc72d92b2dc8187414f6734718563898740c0bc").with_attributes(
                &[(crate::sy::CREATION_TOTAL_SUPPLY, BigInt::zero().to_signed_bytes_be())],
            );
        let change = creation_supply(&sy).expect("an SY carries its supply");
        assert_eq!(change.attributes[0].name, SY_TOTAL_SUPPLY);
        assert_eq!(BigInt::from_signed_bytes_be(&change.attributes[0].value), BigInt::zero());
        assert!(creation_supply(&ProtocolComponent::new("0x34")).is_none());
    }

    /// The YT's activity marker is the market's true YT balance, zero, under the market's id.
    #[test]
    fn a_market_reports_zero_yt() {
        let yt = hex::decode("8e5ae33b762076244fb4ebbd15ba837a176726f4").unwrap();
        let change = yt_activity("0xb986fe3f6ecf16c13e769b0d6d2a0bc41449d54f", yt.clone());
        assert_eq!(change.token, yt);
        assert_eq!(change.component_id, b"0xb986fe3f6ecf16c13e769b0d6d2a0bc41449d54f".to_vec());
        assert_eq!(BigInt::from_unsigned_bytes_be(&change.balance), BigInt::zero());
    }

    /// A zero rate is as unusable as a failed read; any positive rate passes unchanged.
    #[test]
    fn a_zero_rate_is_left_undated() {
        assert_eq!(usable_rate(Some(BigInt::zero())), None);
        assert_eq!(usable_rate(None), None);
        assert_eq!(usable_rate(Some(BigInt::from(1))), Some(BigInt::from(1)));
    }

    /// The seeds feed the stores and the classes are state; everything else is emitted unchanged.
    /// 10e18 as the aggregation hands it back has its top bit set; it must reach the consumer as a
    /// positive value.
    #[test]
    fn a_reserve_with_its_top_bit_set_is_emitted_positive() {
        let ten = hex::decode("8ac7230489e80000").unwrap();
        let emitted = signed_bytes(&ten);
        assert_eq!(emitted, hex::decode("008ac7230489e80000").unwrap());
        assert_eq!(
            BigInt::from_signed_bytes_be(&emitted),
            BigInt::from(10_000_000_000_000_000_000u64)
        );
        assert_eq!(signed_bytes(&[0x01, 0x00]), vec![0x01, 0x00]);
    }

    #[test]
    fn the_emitted_component_drops_the_seeds_and_the_classes() {
        let component = ProtocolComponent::new("0xcbc72d92b2dc8187414f6734718563898740c0bc")
            .with_attributes(&[
                ("sy_decimals", vec![18u8]),
                (crate::sy::CREATION_TOTAL_SUPPLY, vec![1u8]),
                ("creation_balance_0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0", vec![2u8]),
                (
                    "token_out_class_0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0",
                    b"one_to_one".to_vec(),
                ),
                (crate::sy::CREATION_PAUSED, vec![0u8]),
                (crate::sy::CREATION_SY_PAUSED, vec![0u8]),
                (crate::sy::CREATION_SUPPLY_CAP, vec![1u8]),
            ]);
        let names: Vec<String> = as_emitted(&component)
            .static_att
            .into_iter()
            .map(|attribute| attribute.name)
            .collect();
        assert_eq!(names, vec!["sy_decimals".to_string()]);
    }

    /// The wstETH market `0x34280882…be3b`, its SY and its yield token, with the expiry, the PY
    /// index and the implied rate the chain reported for them (see `crate::market_state`).
    const MARKET: &str = "0x34280882267ffa6383b363e278b027be083bbe3b";
    const SY: &str = "0xcbc72d92b2dc8187414f6734718563898740c0bc";
    const YT: &str = "0x04b7fa1e727d7290d6e24fa9b426d0c940283a95";
    const EXPIRY: u64 = 1830124800;
    const PY_INDEX: u64 = 1242190142783145750;
    const IMPLIED_RATE: u64 = 20832594497771595;
    /// A timestamp in the wstETH market's life.
    const NOW: u64 = 1_780_000_000;

    /// Synthetic components alongside it.
    const NEW_MARKET: &str = "0x00000000000000000000000000000000000000aa";
    const NEW_SY: &str = "0x00000000000000000000000000000000000000bb";
    const OTHER_MARKET: &str = "0x00000000000000000000000000000000000000cc";
    const DEAD_MARKET: &str = "0x00000000000000000000000000000000000000dd";
    const LONG_DEAD_MARKET: &str = "0x00000000000000000000000000000000000000ee";
    const OTHER_SY: &str = "0x00000000000000000000000000000000000000b1";
    const IDLE_SY: &str = "0x00000000000000000000000000000000000000b2";
    const LOST_SY: &str = "0x00000000000000000000000000000000000000b3";
    const UNPROFILED_SY: &str = "0x00000000000000000000000000000000000000b4";
    const OTHER_YT: &str = "0x00000000000000000000000000000000000000f1";

    /// `keccak("NewInterestIndex(uint256)")` and `keccak("UpdateImpliedRate(uint256,uint256)")`.
    const NEW_INTEREST_INDEX_TOPIC: &str =
        "71475f2f645813fdbebf53a58968008bff11ee21a58f01c5a9cc263d0bc4703d";
    const UPDATE_IMPLIED_RATE_TOPIC: &str =
        "5c0e21d57bb4cf91d8fe238d6f92e2685a695371b19209afcce6217b478f83e1";
    /// The original factory's `NewMarketConfig`, from the log at block 19419012, and the
    /// `lnFeeRateRoot` / `reserveFeePercent` its `getMarketConfig(RouterV4)` returns.
    const FACTORY_V1: &str = "27b1dacd74688af24a64bd3c9c1b143118740784";
    const NEW_MARKET_CONFIG_TOPIC: &str =
        "e7b39c33a288abde49b8d831b2125b22dbb151766d6aa52b1511b340923d4587";
    const TREASURY_TOPIC: &str = "0000000000000000000000008270400d528c34e1596ef367eedec99080a1b592";
    const LN_FEE_RATE_ROOT_V1: u64 = 499875041000000;
    const RESERVE_FEE_PERCENT_V1: u64 = 80;

    fn word(value: u64) -> Vec<u8> {
        let mut word = vec![0u8; 24];
        word.extend_from_slice(&value.to_be_bytes());
        word
    }

    fn bytes(value: u64) -> Vec<u8> {
        BigInt::from(value).to_signed_bytes_be()
    }

    fn log(address: &str, topics: Vec<Vec<u8>>, data: Vec<u8>, ordinal: u64) -> eth::Log {
        eth::Log {
            address: hex::decode(address.trim_start_matches("0x")).unwrap(),
            topics,
            data,
            ordinal,
            ..Default::default()
        }
    }

    fn new_interest_index(yt: &str, index: u64, ordinal: u64) -> eth::Log {
        log(yt, vec![hex::decode(NEW_INTEREST_INDEX_TOPIC).unwrap(), word(index)], vec![], ordinal)
    }

    fn update_implied_rate(market: &str, rate: u64, ordinal: u64) -> eth::Log {
        log(
            market,
            vec![hex::decode(UPDATE_IMPLIED_RATE_TOPIC).unwrap(), word(NOW)],
            word(rate),
            ordinal,
        )
    }

    fn new_market_config(ordinal: u64) -> eth::Log {
        log(
            FACTORY_V1,
            vec![
                hex::decode(NEW_MARKET_CONFIG_TOPIC).unwrap(),
                hex::decode(TREASURY_TOPIC).unwrap(),
            ],
            [word(LN_FEE_RATE_ROOT_V1), word(RESERVE_FEE_PERCENT_V1)].concat(),
            ordinal,
        )
    }

    /// A successful transaction at `index` emitting `logs`.
    fn trace(index: u32, logs: Vec<eth::Log>) -> eth::TransactionTrace {
        eth::TransactionTrace {
            index,
            hash: vec![index as u8; 32],
            status: 1,
            calls: vec![eth::Call { logs, ..Default::default() }],
            ..Default::default()
        }
    }

    fn failed(index: u32) -> eth::TransactionTrace {
        eth::TransactionTrace { status: 2, ..trace(index, vec![]) }
    }

    fn block(timestamp: u64, traces: Vec<eth::TransactionTrace>) -> eth::Block {
        let mut header =
            eth::BlockHeader { timestamp: Some(Default::default()), ..Default::default() };
        if let Some(stamp) = header.timestamp.as_mut() {
            stamp.seconds = timestamp as i64;
        }
        eth::Block {
            number: 1,
            header: Some(header),
            transaction_traces: traces,
            ..Default::default()
        }
    }

    /// The components created in a block, by the index of the transaction that created them.
    fn created(batches: &[(u64, &[&str])]) -> BlockTransactionProtocolComponents {
        let mut tx_components = Vec::new();
        for (index, ids) in batches {
            tx_components.push(TransactionProtocolComponents {
                tx: Some(Transaction {
                    hash: vec![*index as u8; 32],
                    index: *index,
                    ..Default::default()
                }),
                components: ids
                    .iter()
                    .map(|id| ProtocolComponent::new(id))
                    .collect(),
            });
        }
        BlockTransactionProtocolComponents { tx_components }
    }

    fn market(id: &str, sy: &str, expiry: u64) -> MarketEntry {
        MarketEntry {
            id: id.to_string(),
            sy: sy.to_string(),
            expiry,
            factory: FACTORY_V1.to_string(),
            ln_fee_rate_root_at_creation: "0".to_string(),
        }
    }

    fn registry(markets: &[MarketEntry]) -> String {
        let mut registry = String::new();
        for market in markets {
            registry.push_str(&market.encode());
            registry.push(';');
        }
        registry
    }

    /// The wstETH market's yield token backs it and the market created this block.
    fn backed_by(yt: &[u8]) -> Vec<String> {
        if contract_id(yt) == YT {
            vec![MARKET.to_string(), NEW_MARKET.to_string()]
        } else {
            vec![]
        }
    }

    type Summary = Vec<(u64, String, Vec<(String, Vec<u8>)>)>;

    /// Each change as its transaction index, component, and attribute names and values. Every
    /// attribute must be an update.
    fn summary(changes: Vec<(Transaction, EntityChanges)>) -> Summary {
        let mut summary = Vec::new();
        for (tx, change) in changes {
            let mut attributes = Vec::new();
            for attribute in change.attributes {
                assert_eq!(attribute.change, i32::from(ChangeType::Update));
                attributes.push((attribute.name, attribute.value));
            }
            summary.push((tx.index, change.component_id, attributes));
        }
        summary
    }

    fn attr(name: &str, value: Vec<u8>) -> (String, Vec<u8>) {
        (name.to_string(), value)
    }

    /// A `NewInterestIndex` reaches every market its yield token backs, in log order so the last
    /// one stands, except a market created this block: its creation carries the end-of-block
    /// index, and an earlier transaction would otherwise update it before it exists.
    #[test]
    fn an_interest_index_skips_the_markets_created_this_block() {
        let block = block(
            NOW,
            vec![
                trace(1, vec![new_interest_index(YT, PY_INDEX - 1, 10)]),
                trace(2, vec![new_interest_index(YT, PY_INDEX, 20)]),
            ],
        );

        let created_now = HashSet::from([NEW_MARKET.to_string()]);
        assert_eq!(
            summary(absolute_state_changes(&block, |_| false, backed_by, &created_now)),
            vec![
                (1, MARKET.to_string(), vec![attr(PY_INDEX_STORED, bytes(PY_INDEX - 1))]),
                (2, MARKET.to_string(), vec![attr(PY_INDEX_STORED, bytes(PY_INDEX))]),
            ]
        );

        let none_created = HashSet::new();
        let reached: Vec<(u64, String)> =
            summary(absolute_state_changes(&block, |_| false, backed_by, &none_created))
                .into_iter()
                .map(|(tx, id, _)| (tx, id))
                .collect();
        assert_eq!(
            reached,
            vec![
                (1, MARKET.to_string()),
                (1, NEW_MARKET.to_string()),
                (2, MARKET.to_string()),
                (2, NEW_MARKET.to_string()),
            ]
        );
    }

    /// `UpdateImpliedRate` is taken from a market only, including one created this block: a market
    /// is created empty and first sets its rate when liquidity is added, which its creation does
    /// not carry. A yield token backing no market updates nothing.
    #[test]
    fn an_implied_rate_is_taken_from_a_market_only() {
        let block = block(
            NOW,
            vec![trace(
                3,
                vec![
                    update_implied_rate(MARKET, IMPLIED_RATE, 1),
                    update_implied_rate(SY, IMPLIED_RATE, 2),
                    new_interest_index(OTHER_YT, PY_INDEX, 3),
                ],
            )],
        );
        let created_now = HashSet::from([MARKET.to_string()]);
        assert_eq!(
            summary(absolute_state_changes(&block, |id| id == MARKET, backed_by, &created_now)),
            vec![(3, MARKET.to_string(), vec![attr(LAST_LN_IMPLIED_RATE, bytes(IMPLIED_RATE))])]
        );
    }

    /// Every component created this block is refreshed, SY or market, and so is every market whose
    /// yield token moved its stored index. Other events force nothing.
    #[test]
    fn creations_and_interest_indexes_force_a_refresh() {
        let block = block(
            NOW,
            vec![
                trace(1, vec![update_implied_rate(OTHER_MARKET, IMPLIED_RATE, 1)]),
                trace(2, vec![new_interest_index(YT, PY_INDEX, 2)]),
                trace(3, vec![new_interest_index(OTHER_YT, PY_INDEX, 3)]),
            ],
        );
        let forced = forced_refresh(&block, &created(&[(1, &[NEW_SY, NEW_MARKET])]), backed_by);
        assert_eq!(
            forced,
            HashSet::from([NEW_SY.to_string(), NEW_MARKET.to_string(), MARKET.to_string()])
        );
    }

    #[test]
    fn a_quiet_block_forces_nothing() {
        let block = block(NOW, vec![trace(1, vec![update_implied_rate(MARKET, 1, 1)])]);
        assert!(forced_refresh(&block, &created(&[]), backed_by).is_empty());
    }

    /// A live market's SY is read once however many markets price against it, whether or not it is
    /// a component or needs the rate for its own tokens: its live markets do. An SY whose markets
    /// have all expired is read only while it is in scope, a component, and has a rate-converted
    /// token.
    #[test]
    fn the_rate_is_read_for_live_markets_and_rate_converted_sys() {
        let registry = registry(&[
            market(MARKET, SY, EXPIRY),
            market(OTHER_MARKET, SY, EXPIRY),
            market(DEAD_MARKET, OTHER_SY, NOW),
            market("0x00000000000000000000000000000000000000d2", IDLE_SY, NOW),
            market("0x00000000000000000000000000000000000000d3", LOST_SY, NOW),
            market(NEW_MARKET, UNPROFILED_SY, EXPIRY),
        ]);
        let live = live_markets(&registry, NOW);
        let is_component = |id: &str| [SY, OTHER_SY, IDLE_SY].contains(&id);
        let rate_needed = |id: &str| [SY, OTHER_SY, LOST_SY].contains(&id);

        assert_eq!(
            rate_targets(&registry, &live, |_| true, is_component, rate_needed),
            vec![SY.to_string(), OTHER_SY.to_string(), UNPROFILED_SY.to_string()]
        );
        assert_eq!(
            rate_targets(&registry, &live, |id| id != OTHER_SY, is_component, rate_needed),
            vec![SY.to_string(), UNPROFILED_SY.to_string()]
        );
        assert!(rate_targets(&registry, &[], |_| true, |_| false, |_| true).is_empty());
    }

    /// A rate reader answering `rate` for every SY, recording which SYs it was asked for.
    fn rates_answering(
        asked: &RefCell<Vec<String>>,
        rate: Option<u64>,
    ) -> impl FnOnce(&[String]) -> BTreeMap<String, Option<BigInt>> + '_ {
        move |sys: &[String]| {
            asked
                .borrow_mut()
                .extend(sys.iter().cloned());
            sys.iter()
                .map(|sy| (sy.clone(), rate.map(BigInt::from)))
                .collect()
        }
    }

    fn no_rate_read(_: &[String]) -> BTreeMap<String, Option<BigInt>> {
        panic!("no rate should be read")
    }

    /// A live market gets the clock, the rate's date and its PY index; an SY component its rate
    /// and both clocks; a market within a day past its expiry the clock alone, and one further
    /// past nothing. Every change rides on the block's last successful transaction. The rate is
    /// below the stored index, so the PY index is the stored one.
    #[test]
    fn a_refresh_republishes_the_index_the_rate_and_the_clock() {
        let registry = registry(&[
            market(MARKET, SY, EXPIRY),
            market(DEAD_MARKET, SY, NOW - 10),
            market(LONG_DEAD_MARKET, SY, NOW - crate::registry::EXPIRY_GRACE_SECONDS - 1),
        ]);
        let block = block(NOW, vec![trace(1, vec![]), trace(4, vec![]), failed(9)]);
        let asked = RefCell::new(Vec::new());
        let rate = PY_INDEX - 5;

        let changes = refreshed_state(
            &block,
            &registry,
            |_| true,
            |id| id == SY,
            |_| false,
            |id| (id == MARKET).then(|| BigInt::from(PY_INDEX)),
            rates_answering(&asked, Some(rate)),
        );

        let clock = encode_timestamp(NOW);
        assert_eq!(asked.into_inner(), vec![SY.to_string()]);
        assert_eq!(
            summary(changes),
            vec![
                (
                    4,
                    MARKET.to_string(),
                    vec![
                        attr(BLOCK_TIMESTAMP, clock.clone()),
                        attr(RATE_SAMPLED_AT, clock.clone()),
                        attr(PY_INDEX_CURRENT, bytes(PY_INDEX)),
                    ]
                ),
                (4, DEAD_MARKET.to_string(), vec![attr(BLOCK_TIMESTAMP, clock.clone())]),
                (
                    4,
                    SY.to_string(),
                    vec![
                        attr(BLOCK_TIMESTAMP, clock.clone()),
                        attr(RATE_SAMPLED_AT, clock),
                        attr(SY_EXCHANGE_RATE, bytes(rate)),
                    ]
                ),
            ]
        );
    }

    /// A rate that does not resolve is neither dated nor guessed at: the market and its SY get the
    /// clock alone, which is how a consumer sees the rate it holds has gone stale.
    #[test]
    fn an_unresolved_rate_leaves_only_the_clock() {
        let registry = registry(&[market(MARKET, SY, EXPIRY)]);
        let block = block(NOW, vec![trace(1, vec![])]);
        let asked = RefCell::new(Vec::new());
        let changes = refreshed_state(
            &block,
            &registry,
            |_| true,
            |id| id == SY,
            |_| true,
            |_| Some(BigInt::from(PY_INDEX)),
            rates_answering(&asked, None),
        );
        let clock = encode_timestamp(NOW);
        assert_eq!(
            summary(changes),
            vec![
                (1, MARKET.to_string(), vec![attr(BLOCK_TIMESTAMP, clock.clone())]),
                (1, SY.to_string(), vec![attr(BLOCK_TIMESTAMP, clock)]),
            ]
        );
    }

    /// An SY that is not a component still has its rate read for its live market, but gets no
    /// state of its own: that would be state on a component that never existed. With no stored
    /// index the market's PY index is the rate.
    #[test]
    fn an_sy_that_is_no_component_gets_no_state() {
        let registry = registry(&[market(MARKET, SY, EXPIRY)]);
        let block = block(NOW, vec![trace(1, vec![])]);
        let asked = RefCell::new(Vec::new());
        let changes = refreshed_state(
            &block,
            &registry,
            |_| true,
            |_| false,
            |_| false,
            |_| None,
            rates_answering(&asked, Some(PY_INDEX)),
        );
        assert_eq!(asked.into_inner(), vec![SY.to_string()]);
        let clock = encode_timestamp(NOW);
        assert_eq!(
            summary(changes),
            vec![(
                1,
                MARKET.to_string(),
                vec![
                    attr(BLOCK_TIMESTAMP, clock.clone()),
                    attr(RATE_SAMPLED_AT, clock),
                    attr(PY_INDEX_CURRENT, bytes(PY_INDEX)),
                ]
            )]
        );
    }

    /// Off a refresh block only the components in scope are refreshed, and only their SYs read.
    #[test]
    fn only_the_components_in_scope_are_refreshed() {
        let registry =
            registry(&[market(MARKET, SY, EXPIRY), market(OTHER_MARKET, OTHER_SY, EXPIRY)]);
        let block = block(NOW, vec![trace(1, vec![])]);
        let asked = RefCell::new(Vec::new());
        let changes = refreshed_state(
            &block,
            &registry,
            |id| id == OTHER_MARKET,
            |_| true,
            |_| true,
            |_| None,
            rates_answering(&asked, Some(PY_INDEX)),
        );
        assert_eq!(asked.into_inner(), vec![OTHER_SY.to_string()]);
        let refreshed: Vec<String> = summary(changes)
            .into_iter()
            .map(|(_, id, _)| id)
            .collect();
        assert_eq!(refreshed, vec![OTHER_MARKET.to_string(), OTHER_SY.to_string()]);
    }

    /// With nothing in scope, or no successful transaction to anchor the changes to, no rate is
    /// read and nothing is emitted.
    #[test]
    fn a_refresh_with_nothing_to_do_reads_nothing() {
        let registry = registry(&[market(MARKET, SY, EXPIRY)]);
        let busy = block(NOW, vec![trace(1, vec![])]);
        let nothing_in_scope = refreshed_state(
            &busy,
            &registry,
            |_| false,
            |_| true,
            |_| true,
            |_| None,
            no_rate_read,
        );
        assert!(nothing_in_scope.is_empty());

        let all_failed = block(NOW, vec![failed(1)]);
        let no_anchor = refreshed_state(
            &all_failed,
            &registry,
            |_| true,
            |_| true,
            |_| true,
            |_| None,
            no_rate_read,
        );
        assert!(no_anchor.is_empty());
    }

    /// A fee reader resolving the markets in `resolves`, recording which markets it was asked for.
    fn fees_answering<'a>(
        asked: &'a RefCell<Vec<String>>,
        resolves: &'a [&'a str],
    ) -> impl FnOnce(&[MarketEntry]) -> Vec<(String, MarketFee)> + 'a {
        move |markets: &[MarketEntry]| {
            let mut fees = Vec::new();
            for market in markets {
                asked
                    .borrow_mut()
                    .push(market.id.clone());
                if resolves.contains(&market.id.as_str()) {
                    let fee = MarketFee {
                        ln_fee_rate_root: BigInt::from(LN_FEE_RATE_ROOT_V1),
                        reserve_fee_percent: BigInt::from(RESERVE_FEE_PERCENT_V1),
                    };
                    fees.push((market.id.clone(), fee));
                }
            }
            fees
        }
    }

    fn no_fee_read(_: &[MarketEntry]) -> Vec<(String, MarketFee)> {
        panic!("no fee should be read")
    }

    fn fee_attributes() -> Vec<(String, Vec<u8>)> {
        vec![
            attr(LN_FEE_RATE_ROOT, bytes(LN_FEE_RATE_ROOT_V1)),
            attr(RESERVE_FEE_PERCENT, bytes(RESERVE_FEE_PERCENT_V1)),
        ]
    }

    /// A market created and caught by a factory-wide fee event in the same block is read once, on
    /// the later of the two transactions, whichever comes first. The event reaches every live
    /// market of the factory and no expired one.
    #[test]
    fn a_market_hit_twice_is_read_once_on_the_later_transaction() {
        let registry = registry(&[
            market(MARKET, SY, EXPIRY),
            market(OTHER_MARKET, SY, EXPIRY),
            market(DEAD_MARKET, SY, NOW),
        ]);
        for (creation, event) in [(1u32, 3u32), (3, 1)] {
            let block = block(NOW, vec![trace(event, vec![new_market_config(1)])]);
            let asked = RefCell::new(Vec::new());
            let mut changes = summary(fee_changes(
                &block,
                &created(&[(creation.into(), &[MARKET])]),
                || Some(registry.clone()),
                fees_answering(&asked, &[MARKET, OTHER_MARKET]),
            ));
            changes.sort_by(|a, b| a.1.cmp(&b.1));

            let mut asked = asked.into_inner();
            asked.sort();
            assert_eq!(asked, vec![OTHER_MARKET.to_string(), MARKET.to_string()]);
            assert_eq!(
                changes,
                vec![
                    (event.into(), OTHER_MARKET.to_string(), fee_attributes()),
                    (3, MARKET.to_string(), fee_attributes()),
                ],
                "creation in transaction {creation}, fee event in transaction {event}"
            );
        }
    }

    /// A market whose fee does not resolve keeps the fee it had rather than being given one.
    #[test]
    fn an_unresolved_fee_is_left_stale() {
        let registry = registry(&[market(MARKET, SY, EXPIRY), market(OTHER_MARKET, SY, EXPIRY)]);
        let block = block(NOW, vec![trace(2, vec![new_market_config(1)])]);
        let asked = RefCell::new(Vec::new());
        let changes = fee_changes(
            &block,
            &created(&[]),
            || Some(registry.clone()),
            fees_answering(&asked, &[OTHER_MARKET]),
        );
        assert_eq!(summary(changes), vec![(2, OTHER_MARKET.to_string(), fee_attributes())]);
    }

    /// No factory is called without a registry, without a live market, or without a market
    /// creation or a fee event; without either of those the registry is not even fetched.
    #[test]
    fn nothing_to_read_makes_no_fee_call() {
        let live = registry(&[market(MARKET, SY, EXPIRY)]);
        let expired = registry(&[market(MARKET, SY, NOW)]);
        let fee_event = block(NOW, vec![trace(1, vec![new_market_config(1)])]);
        let quiet = block(NOW, vec![trace(1, vec![update_implied_rate(MARKET, 1, 1)])]);
        let creation = created(&[(1, &[MARKET])]);

        let unfetched = || -> Option<String> { panic!("the registry was fetched") };
        assert!(fee_changes(&fee_event, &creation, || None, no_fee_read).is_empty());
        assert!(
            fee_changes(&fee_event, &creation, || Some(expired.clone()), no_fee_read).is_empty()
        );
        assert!(fee_changes(&quiet, &created(&[]), unfetched, no_fee_read).is_empty());
        assert!(fee_changes(
            &quiet,
            &created(&[(1, &[NEW_SY])]),
            || Some(live.clone()),
            no_fee_read
        )
        .is_empty());
    }
}
