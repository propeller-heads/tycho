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
    fees::{fan_out, fee_scope, read_market_fees, LN_FEE_RATE_ROOT, RESERVE_FEE_PERCENT},
    keys::{
        contract_id, market_tokens_key, markets_by_yt_key, py_index_key, sy_rate_needed_key,
        MARKET_REGISTRY,
    },
    market_state::{
        last_ln_implied_rate, py_index_stored, LAST_LN_IMPLIED_RATE, PY_INDEX_STORED, TOTAL_PT,
        TOTAL_SY,
    },
    registry::{expired_markets, live_markets, markets_of_yt, sy_components, MarketEntry},
    sy::{is_class_attribute, is_creation_seed, SY_TOTAL_SUPPLY},
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

    // A new SY's classes and pause flag, and every later pause, unpause and re-classification.
    // They follow the component creation of the same transaction, which they describe.
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
            let Some((sy, pt)) = market_sy_pt(&components_store, &component_id) else {
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

            transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx))
                .add_entity_change(&EntityChanges { component_id, attributes });
        }
    }

    let created: HashSet<String> = new_components
        .tx_components
        .iter()
        .flat_map(|tx| &tx.components)
        .map(|component| component.id.clone())
        .collect();
    for (tx, change) in absolute_state_changes(&block, &components_store, &markets_by_yt, &created)
    {
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_entity_change(&change);
    }

    let forced = forced_refresh(&block, &new_components, &markets_by_yt);
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
    markets_by_yt: &StoreGetString,
    created: &HashSet<String>,
) -> Vec<(Transaction, EntityChanges)> {
    let mut changes = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let entries = if let Some(rate) = last_ln_implied_rate(log) {
                let component_id = contract_id(&log.address);
                if components_store.has_last(market_tokens_key(&component_id)) {
                    vec![(component_id, LAST_LN_IMPLIED_RATE, rate)]
                } else {
                    vec![]
                }
            } else if let Some(index) = py_index_stored(log) {
                // The index belongs to the yield token, so every market it backs takes it. A market
                // created this block is left out: its creation carries the end-of-block value, and
                // an earlier transaction would otherwise update it before it exists.
                markets_backed_by(markets_by_yt, &log.address)
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

/// Republishes the state that moves without a Pendle event: the PY index and the clock.
///
/// The SY exchange rate has no event stream, so this is the one place the package reads chain
/// state per block rather than per market lifetime. It is one batched `eth_call` for the whole
/// protocol, over the SYs rather than the markets — one SY backs several expiries.
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
/// Off a refresh block, only the components in `forced` are refreshed (see [`forced_refresh`]).
///
/// The changes are anchored to the block's last transaction. Every `EntityChanges` reaches the
/// indexer inside a `TransactionChanges`, but a refresh has no transaction that caused it, and a
/// fabricated hash would be persisted as though it were real. The last transaction is a genuine
/// one and orders the refresh after everything that actually happened in the block.
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
    let in_scope = |id: &str| refreshing || forced.contains(id);
    let Some(registry) = registry_store.get_last(MARKET_REGISTRY) else { return vec![] };
    let markets: Vec<MarketEntry> = live_markets(&registry, block.timestamp_seconds())
        .into_iter()
        .filter(|market| in_scope(&market.id))
        .collect();
    let expired: Vec<MarketEntry> = expired_markets(&registry, block.timestamp_seconds())
        .into_iter()
        .filter(|market| in_scope(&market.id))
        .collect();
    let targets = rate_targets(&registry, &markets, components_store, rate_needed_store, in_scope);
    // An SY has no expiry, so it is refreshed even when no market is live.
    if markets.is_empty() && expired.is_empty() && targets.is_empty() {
        return vec![];
    }
    let Some(anchor) = block.transactions().last() else {
        log::info!("block {} holds no transactions, skipping the SY rate refresh", block.number);
        return vec![];
    };
    let anchor: Transaction = anchor.into();

    let rates = read_exchange_rates(&targets);
    let timestamp = encode_timestamp(block.timestamp_seconds());
    let mut changes = Vec::new();

    for market in &markets {
        let rate = rates
            .get(&market.sy)
            .and_then(Option::as_ref);
        let mut attributes = vec![state_attribute(BLOCK_TIMESTAMP, timestamp.clone())];
        if let Some(rate) = rate {
            let stored = py_index_store.get_last(py_index_key(&market.id));
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
        if !components_store.has_last(sy) {
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
    markets_by_yt: &StoreGetString,
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
            forced.extend(markets_backed_by(markets_by_yt, &log.address));
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
    components_store: &StoreGetRaw,
    rate_needed_store: &StoreGetInt64,
    in_scope: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut targets: Vec<String> = sy_components(registry)
        .into_iter()
        .filter(|sy| {
            in_scope(sy) &&
                components_store.has_last(sy) &&
                rate_needed_store
                    .get_last(sy_rate_needed_key(sy))
                    .is_some_and(|needed| needed != 0)
        })
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
/// Every SY in the input appears in the result; the value is `None` where the call failed.
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
        let rate = responses
            .get(index)
            .and_then(RpcBatch::decode::<_, pendle_sy::functions::ExchangeRate>);
        if rate.is_none() {
            log::info!("SY {} did not answer exchangeRate(), leaving its rate undated", sy);
        }
        rates.insert(sy.clone(), rate);
    }
    rates
}

/// Returns a market's SY and PT addresses, or `None` if the id names something that is not a
/// market.
/// Every market the yield token at `yt` backs, or none when it backs no market.
fn markets_backed_by(markets_by_yt: &StoreGetString, yt: &[u8]) -> Vec<String> {
    markets_by_yt
        .get_last(markets_by_yt_key(yt))
        .map(|listed| markets_of_yt(&listed))
        .unwrap_or_default()
}

fn market_sy_pt(store: &StoreGetRaw, component_id: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let roles = store.get_last(market_tokens_key(component_id))?;
    let roles: Vec<Vec<u8>> = serde_sibor::from_bytes(&roles)
        .expect("deserializing market token roles from the component store");
    let [sy, pt, _yt] = roles.as_slice() else {
        panic!("market {component_id} stored {} roles, expected 3", roles.len())
    };
    Some((sy.clone(), pt.clone()))
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
    use super::*;

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
}
