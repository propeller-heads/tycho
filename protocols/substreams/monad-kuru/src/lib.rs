//! Kuru (Monad CLOB) for Tycho.
//!
//! Components = markets registered by the Kuru Router. Book levels come from market events via the
//! shared `kuru_book` rules; vault quotes, market state and fees come from the market's storage
//! (Monad blocks are Extended), because vault fills move storage without a complete event trail.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod abi;
mod pb;

use std::{collections::HashMap, str::FromStr};

use abi::{order_book::events as ob, router::events::MarketRegistered};
use kuru_book::Event as BookEvent;
use pb::{Event, Events, LevelDelta, LevelDeltas};
use substreams::{
    pb::substreams::StoreDeltas,
    scalar::BigInt,
    store::{
        StoreAdd, StoreAddBigInt, StoreDelete, StoreGet, StoreGetString, StoreNew, StoreSet,
        StoreSetBigInt, StoreSetIfNotExists, StoreSetIfNotExistsString, StoreSetString,
    },
};
use substreams_ethereum::{pb::eth::v2 as eth, Event as _};
use tycho_substreams::{balances::aggregate_balances_changes, prelude::*};

/// Market storage slots (Kuru `AbstractAMM` + `OrderBook` layout, verified on chain 09-29).
mod slot {
    pub const VAULT_BEST_BID: u8 = 0;
    pub const VAULT_BID_PARTIAL: u8 = 1; // high 96 bits; low 160 = vault address
    pub const VAULT_BEST_ASK: u8 = 2;
    pub const VAULT_ASK: u8 = 3; // low 96 ask partial, next 96 ask size
    pub const VAULT_BID: u8 = 4; // low 96 bid size, next 96 spread
    pub const STATE: u8 = 61; // orderIdCounter u40 | marketState u8 | sizePrecision u96 | pricePrecision u32
    pub const TAKER_FEE: u8 = 62;
    pub const MAKER_FEE: u8 = 63;
    pub const BASE_DECIMALS: u8 = 64;
    pub const QUOTE_DECIMALS: u8 = 66;
}

fn hex0x(b: &[u8]) -> String {
    format!("0x{}", hex::encode(b))
}

fn slot_of(key: &[u8]) -> Option<u8> {
    (key.len() == 32 && key[..31].iter().all(|b| *b == 0)).then(|| key[31])
}

/// `bits` of `word` starting at bit `from` (big-endian word), as minimal big-endian bytes.
fn field(word: &[u8], from: usize, bits: usize) -> Vec<u8> {
    let v = BigInt::from_unsigned_bytes_be(word);
    let shifted = v / BigInt::from(2).pow(from as u32);
    let masked = shifted % BigInt::from(2).pow(bits as u32);
    masked.to_bytes_be().1
}

fn attr(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_string(), value, change: ChangeType::Update.into() }
}

/// Static market parameters kept per component: "pp,sp,base_mult_exp,quote_mult_exp,base,quote".
struct Market {
    pp: u128,
    sp: u128,
    base_dec: u32,
    quote_dec: u32,
    base: Vec<u8>,
    quote: Vec<u8>,
}

impl Market {
    fn encode(&self) -> String {
        format!(
            "{},{},{},{},{},{}",
            self.pp,
            self.sp,
            self.base_dec,
            self.quote_dec,
            hex::encode(&self.base),
            hex::encode(&self.quote)
        )
    }
    fn decode(s: &str) -> Market {
        let p: Vec<&str> = s.split(',').collect();
        Market {
            pp: p[0].parse().unwrap(),
            sp: p[1].parse().unwrap(),
            base_dec: p[2].parse().unwrap(),
            quote_dec: p[3].parse().unwrap(),
            base: hex::decode(p[4]).unwrap(),
            quote: hex::decode(p[5]).unwrap(),
        }
    }
}

/// Decimals the market's `initialize` wrote in the registering transaction.
fn decimals(tx: &eth::TransactionTrace, market: &[u8]) -> (Option<u32>, Option<u32>) {
    let (mut b, mut q) = (None, None);
    for sc in tx
        .calls
        .iter()
        .filter(|c| !c.state_reverted)
        .flat_map(|c| c.storage_changes.iter())
        .filter(|sc| sc.address == market)
    {
        let v = BigInt::from_unsigned_bytes_be(&sc.new_value).to_u64() as u32;
        match slot_of(&sc.key) {
            Some(slot::BASE_DECIMALS) => b = Some(v),
            Some(slot::QUOTE_DECIMALS) => q = Some(v),
            _ => {}
        }
    }
    (b, q)
}

#[substreams::handlers::map]
fn map_markets(
    params: String,
    block: eth::Block,
) -> Result<BlockTransactionProtocolComponents, substreams::errors::Error> {
    let router = hex::decode(params.trim_start_matches("0x"))?;
    let mut out = Vec::new();
    for tx in block.transactions() {
        let components: Vec<ProtocolComponent> = tx
            .logs_with_calls()
            .filter(|(log, _)| log.address == router)
            .filter_map(|(log, _)| MarketRegistered::match_and_decode(log))
            .map(|ev| {
                let (bdec, qdec) = decimals(tx, &ev.market);
                let mut static_att = vec![
                    ("base", ev.base_asset.clone()),
                    ("quote", ev.quote_asset.clone()),
                    ("price_precision", ev.price_precision.to_signed_bytes_be()),
                    ("size_precision", ev.size_precision.to_signed_bytes_be()),
                ];
                if let (Some(b), Some(q)) = (bdec, qdec) {
                    static_att.push(("base_decimals", BigInt::from(b).to_signed_bytes_be()));
                    static_att.push(("quote_decimals", BigInt::from(q).to_signed_bytes_be()));
                }
                ProtocolComponent {
                    id: hex0x(&ev.market),
                    tokens: vec![ev.base_asset.clone(), ev.quote_asset.clone()],
                    contracts: vec![],
                    static_att: static_att
                        .into_iter()
                        .map(|(n, v)| Attribute {
                            name: n.into(),
                            value: v,
                            change: ChangeType::Creation.into(),
                        })
                        .collect(),
                    change: ChangeType::Creation.into(),
                    protocol_type: Some(ProtocolType {
                        name: "kuru_market".into(),
                        financial_type: FinancialType::Swap.into(),
                        attribute_schema: vec![],
                        implementation_type: ImplementationType::Custom.into(),
                    }),
                }
            })
            .collect();
        if !components.is_empty() {
            out.push(TransactionProtocolComponents { tx: Some(tx.into()), components });
        }
    }
    Ok(BlockTransactionProtocolComponents { tx_components: out })
}

#[substreams::handlers::store]
fn store_markets(markets: BlockTransactionProtocolComponents, store: StoreSetString) {
    for c in markets
        .tx_components
        .iter()
        .flat_map(|t| t.components.iter())
    {
        let get = |n: &str| {
            c.static_att
                .iter()
                .find(|a| a.name == n)
                .map(|a| BigInt::from_signed_bytes_be(&a.value))
        };
        let dec = |n: &str, token: &[u8]| {
            // no decimals write in the registering tx: native is 18, else unknown (no balances)
            get(n)
                .map(|v| v.to_u64() as u32)
                .or(token
                    .iter()
                    .all(|b| *b == 0)
                    .then_some(18))
                .unwrap_or(u32::MAX)
        };
        let m = Market {
            pp: get("price_precision").unwrap().to_u64() as u128,
            sp: get("size_precision")
                .unwrap()
                .to_string()
                .parse()
                .unwrap(),
            base_dec: dec("base_decimals", &c.tokens[0]),
            quote_dec: dec("quote_decimals", &c.tokens[1]),
            base: c.tokens[0].clone(),
            quote: c.tokens[1].clone(),
        };
        store.set(0, &c.id, &m.encode());
    }
}

#[substreams::handlers::map]
fn map_events(
    block: eth::Block,
    markets: StoreGetString,
) -> Result<Events, substreams::errors::Error> {
    let mut events = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(l, _)| l) {
            let market = hex0x(&log.address);
            if markets.get_last(&market).is_none() {
                continue;
            }
            let base =
                Event { market, ordinal: log.ordinal, tx: Some(tx.into()), ..Default::default() };
            let ev = if let Some(e) = ob::OrderCreated::match_and_decode(log) {
                Event {
                    kind: 0,
                    order_id: e.order_id.to_u64(),
                    price: e.price.to_u64() as u32,
                    size: e.size.to_string(),
                    is_buy: e.is_buy,
                    ..base
                }
            } else if let Some(e) = ob::FlipOrderCreated::match_and_decode(log) {
                Event {
                    kind: 0,
                    order_id: e.order_id.to_u64(),
                    price: e.price.to_u64() as u32,
                    size: e.size.to_string(),
                    is_buy: e.is_buy,
                    ..base
                }
            } else if let Some(e) = ob::FlippedOrderCreated::match_and_decode(log) {
                Event {
                    kind: 0,
                    order_id: e.order_id.to_u64(),
                    price: e.price.to_u64() as u32,
                    size: e.size.to_string(),
                    is_buy: e.is_buy,
                    ..base
                }
            } else if let Some(e) = ob::OrderCanceled::match_and_decode(log) {
                Event {
                    kind: 1,
                    order_id: e.order_id.to_u64(),
                    price: e.price.to_u64() as u32,
                    size: e.size.to_string(),
                    is_buy: e.is_buy,
                    ..base
                }
            } else if let Some(e) = ob::FlipOrderUpdated::match_and_decode(log) {
                Event { kind: 2, order_id: e.order_id.to_u64(), size: e.size.to_string(), ..base }
            } else if let Some(e) = ob::Trade::match_and_decode(log) {
                Event {
                    kind: 3,
                    order_id: e.order_id.to_u64(),
                    is_buy: e.is_buy,
                    size: e.updated_size.to_string(),
                    price_1e18: e.price.to_string(),
                    filled: e.filled_size.to_string(),
                    ..base
                }
            } else {
                continue;
            };
            events.push(ev);
        }
    }
    events.sort_by_key(|e| e.ordinal);
    Ok(Events { events })
}

/// Decimal store/event value; empty = absent (0).
fn u(s: &str) -> u128 {
    if s.is_empty() {
        0
    } else {
        s.parse().expect("decimal u128")
    }
}

fn book_event(e: &Event) -> BookEvent {
    match e.kind {
        0 => BookEvent::Created {
            id: e.order_id,
            price: e.price,
            size: u(&e.size),
            is_buy: e.is_buy,
        },
        1 => BookEvent::OrderCanceled {
            id: e.order_id,
            price: e.price,
            size: u(&e.size),
            is_buy: e.is_buy,
        },
        2 => BookEvent::FlipOrderUpdated { id: e.order_id, size: u(&e.size) },
        _ => BookEvent::Trade {
            id: e.order_id,
            taker_buy: e.is_buy,
            price_1e18: u(&e.price_1e18),
            updated_size: u(&e.size),
            filled: u(&e.filled),
        },
    }
}

/// Trailing `:` so `delete_prefix` of one order never matches a longer id.
fn order_key(e: &Event) -> String {
    format!("{}:{}:", e.market, e.order_id)
}

/// Live order sizes; a key is deleted when its order leaves the book, so the store holds resting
/// orders only.
#[substreams::handlers::store]
fn store_order_sizes(events: Events, store: StoreSetBigInt) {
    for e in &events.events {
        match book_event(e).order_size() {
            Some((_, 0)) => store.delete_prefix(e.ordinal as i64, &order_key(e)),
            Some((_, size)) => store.set(e.ordinal, order_key(e), &big(size)),
            None => {}
        }
    }
}

/// Price and side of resting orders (`FlipOrderUpdated` and `Trade` carry no tick), deleted with
/// the order.
#[substreams::handlers::store]
fn store_order_meta(events: Events, store: StoreSetIfNotExistsString) {
    for e in &events.events {
        if e.kind == 0 {
            store.set_if_not_exists(e.ordinal, order_key(e), &format!("{}:{}", e.price, e.is_buy));
        } else if matches!(book_event(e).order_size(), Some((_, 0))) {
            store.delete_prefix(e.ordinal as i64, &order_key(e));
        }
    }
}

fn big(x: u128) -> BigInt {
    BigInt::from_str(&x.to_string()).expect("u128 is a valid integer")
}

#[substreams::handlers::map]
fn map_level_deltas(
    events: Events,
    sizes: StoreDeltas,
    meta: StoreGetString,
    markets: StoreGetString,
) -> Result<LevelDeltas, substreams::errors::Error> {
    // size before each set, by (order key, ordinal)
    let before: HashMap<(String, u64), u128> = sizes
        .deltas
        .iter()
        .map(|d| {
            let old = String::from_utf8_lossy(&d.old_value);
            ((d.key.clone(), d.ordinal), u(&old))
        })
        .collect();
    let mut deltas = Vec::new();
    for e in &events.events {
        let ev = book_event(e);
        // the order as it stood just before this event (a delete at this ordinal is the event's
        // own)
        let order = match ev {
            BookEvent::FlipOrderUpdated { .. } | BookEvent::Trade { id: 1.., .. } => {
                let key = order_key(e);
                meta.get_at(e.ordinal - 1, &key)
                    .map(|m| {
                        let (price, is_buy) = m
                            .split_once(':')
                            .expect("meta is price:is_buy");
                        let old = before
                            .get(&(key, e.ordinal))
                            .copied()
                            .unwrap_or(0);
                        (
                            price
                                .parse()
                                .expect("meta price is a u32"),
                            is_buy == "true",
                            old,
                        )
                    })
            }
            _ => None,
        };
        let pp = Market::decode(&markets.get_last(&e.market).unwrap()).pp;
        if let Some(d) = ev
            .delta(pp, order)
            .map_err(|x| anyhow::anyhow!(x))?
        {
            let delta = BigInt::from_str(&d.add.to_string()).unwrap() -
                BigInt::from_str(&d.sub.to_string()).unwrap();
            deltas.push(LevelDelta {
                market: e.market.clone(),
                ordinal: e.ordinal,
                tx: e.tx.clone(),
                is_buy: d.is_buy,
                price: d.price,
                delta: delta.to_string(),
            });
        }
    }
    Ok(LevelDeltas { deltas })
}

fn level_key(d: &LevelDelta) -> String {
    format!("{}:{}:{}", d.market, if d.is_buy { "b" } else { "a" }, d.price)
}

#[substreams::handlers::store]
fn store_levels(deltas: LevelDeltas, store: StoreAddBigInt) {
    for d in &deltas.deltas {
        store.add(d.ordinal, level_key(d), BigInt::from_str(&d.delta).unwrap());
    }
}

/// Book depth as component balances (TVL only): asks in base, bids in quote.
#[substreams::handlers::map]
fn map_balance_deltas(
    deltas: LevelDeltas,
    markets: StoreGetString,
) -> Result<BlockBalanceDeltas, substreams::errors::Error> {
    let ten = |d: u32| BigInt::from(10).pow(d);
    let balance_deltas = deltas
        .deltas
        .iter()
        .filter_map(|d| {
            let m = Market::decode(&markets.get_last(&d.market).unwrap());
            if m.base_dec == u32::MAX || m.quote_dec == u32::MAX {
                return None;
            }
            let size = BigInt::from_str(&d.delta).unwrap();
            let sp = BigInt::from_str(&m.sp.to_string()).unwrap();
            let (token, amount) = if d.is_buy {
                let pp = BigInt::from_str(&m.pp.to_string()).unwrap();
                (m.quote.clone(), size * BigInt::from(d.price) * ten(m.quote_dec) / (sp * pp))
            } else {
                (m.base.clone(), size * ten(m.base_dec) / sp)
            };
            Some(BalanceDelta {
                ord: d.ordinal,
                tx: d.tx.clone(),
                token,
                delta: amount.to_signed_bytes_be(),
                component_id: d.market.clone().into_bytes(),
            })
        })
        .collect();
    Ok(BlockBalanceDeltas { balance_deltas })
}

#[substreams::handlers::store]
fn store_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}

/// Vault, state and fee attributes from the market's own storage writes in `tx`.
fn storage_attrs(
    tx: &eth::TransactionTrace,
    markets: &StoreGetString,
) -> HashMap<String, Vec<Attribute>> {
    // (address, slot) -> (value before the tx, value after it), in storage-change order
    let mut changes: Vec<&eth::StorageChange> = tx
        .calls
        .iter()
        .filter(|c| !c.state_reverted)
        .flat_map(|c| c.storage_changes.iter())
        .collect();
    changes.sort_by_key(|sc| sc.ordinal);
    let mut last: HashMap<(Vec<u8>, u8), (Vec<u8>, Vec<u8>)> = HashMap::new();
    for sc in changes {
        if let Some(s) = slot_of(&sc.key) {
            last.entry((sc.address.clone(), s))
                .or_insert_with(|| (sc.old_value.clone(), Vec::new()))
                .1 = sc.new_value.clone();
        }
    }
    let mut out: HashMap<String, Vec<Attribute>> = HashMap::new();
    for ((addr, s), (before, w)) in last {
        let id = hex0x(&addr);
        if markets.get_last(&id).is_none() {
            continue;
        }
        let attrs = match s {
            slot::VAULT_BEST_BID => vec![attr("vault_best_bid", field(&w, 0, 256))],
            slot::VAULT_BID_PARTIAL => vec![attr("vault_bid_partial", field(&w, 160, 96))],
            slot::VAULT_BEST_ASK => vec![attr("vault_best_ask", field(&w, 0, 256))],
            slot::VAULT_ASK => vec![
                attr("vault_ask_partial", field(&w, 0, 96)),
                attr("vault_ask_size", field(&w, 96, 96)),
            ],
            slot::VAULT_BID => vec![
                attr("vault_bid_size", field(&w, 0, 96)),
                attr("vault_spread", field(&w, 96, 96)),
            ],
            // slot 61 also holds the order id counter: emit only when the state byte moves
            slot::STATE if field(&w, 40, 8) != field(&before, 40, 8) => {
                let state = field(&w, 40, 8);
                vec![attr("active", vec![u8::from(state.iter().all(|b| *b == 0))])]
            }
            slot::TAKER_FEE => vec![attr("taker_fee_bps", field(&w, 0, 256))],
            slot::MAKER_FEE => vec![attr("maker_fee_bps", field(&w, 0, 256))],
            _ => continue,
        };
        out.entry(id).or_default().extend(attrs);
    }
    out
}

#[allow(clippy::too_many_arguments)]
#[substreams::handlers::map]
fn map_protocol_changes(
    block: eth::Block,
    new_markets: BlockTransactionProtocolComponents,
    markets: StoreGetString,
    level_deltas: LevelDeltas,
    level_store: StoreDeltas,
    balance_deltas: BlockBalanceDeltas,
    balance_store: StoreDeltas,
) -> Result<BlockChanges, substreams::errors::Error> {
    let mut txs: HashMap<u64, TransactionChangesBuilder> = HashMap::new();

    for t in &new_markets.tx_components {
        let tx = t.tx.as_ref().unwrap();
        let b = txs
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        for c in &t.components {
            b.add_protocol_component(c);
            // mutable parameters start at their registration values; storage writes update them
            if let Some(ev) = block
                .transactions()
                .filter(|x| x.index as u64 == tx.index)
                .flat_map(|x| x.logs_with_calls().map(|(l, _)| l))
                .filter_map(MarketRegistered::match_and_decode)
                .find(|ev| hex0x(&ev.market) == c.id)
            {
                b.add_entity_change(&EntityChanges {
                    component_id: c.id.clone(),
                    attributes: vec![
                        attr("taker_fee_bps", ev.taker_fee_bps.to_signed_bytes_be()),
                        attr("maker_fee_bps", ev.maker_fee_bps.to_signed_bytes_be()),
                        attr("vault_spread", ev.kuru_amm_spread.to_signed_bytes_be()),
                    ],
                });
            }
        }
    }

    for tx in block.transactions() {
        for (id, attributes) in storage_attrs(tx, &markets) {
            let t: Transaction = tx.into();
            txs.entry(t.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&t))
                .add_entity_change(&EntityChanges { component_id: id, attributes });
        }
    }

    // store deltas and map deltas are 1:1, in order
    for (sd, d) in level_store
        .deltas
        .iter()
        .zip(level_deltas.deltas.iter())
    {
        let new = BigInt::from_str(&String::from_utf8_lossy(&sd.new_value)).unwrap();
        let name = format!("{}/{}", if d.is_buy { "b" } else { "a" }, d.price);
        let change = if new.is_zero() { ChangeType::Deletion } else { ChangeType::Update };
        let tx = d.tx.as_ref().unwrap();
        txs.entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx))
            .add_entity_change(&EntityChanges {
                component_id: d.market.clone(),
                attributes: vec![Attribute {
                    name,
                    value: new.to_signed_bytes_be(),
                    change: change.into(),
                }],
            });
    }

    aggregate_balances_changes(balance_store, balance_deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let b = txs
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            balances
                .values()
                .flat_map(|m| m.values())
                .for_each(|bc| b.add_balance_change(bc));
        });

    let mut changes: Vec<_> = txs
        .into_values()
        .filter_map(|b| b.build())
        .collect();
    changes.sort_by_key(|c| c.tx.as_ref().map(|t| t.index));
    Ok(BlockChanges { block: Some((&block).into()), changes, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_fields_match_chain() {
        // MON/USDC market slot 61 and slot 4, read 09-29
        let w61 = hex::decode("0000000000000000000005f5e1000000000000000002540be400000006c336fc")
            .unwrap();
        assert!(field(&w61, 40, 8)
            .iter()
            .all(|b| *b == 0)); // state 0 = active
        assert_eq!(BigInt::from_unsigned_bytes_be(&field(&w61, 48, 96)).to_u64(), 10_000_000_000);
        let w4 = hex::decode("000000000000000000000000000000000000001e000000000000000000000000")
            .unwrap();
        assert_eq!(BigInt::from_unsigned_bytes_be(&field(&w4, 96, 96)).to_u64(), 30);
        assert_eq!(slot_of(&w4), None);
        let mut k = [0u8; 32];
        k[31] = 61;
        assert_eq!(slot_of(&k), Some(slot::STATE));
    }

    #[test]
    fn market_roundtrip() {
        let m = Market {
            pp: 100_000_000,
            sp: 10_000_000_000,
            base_dec: 18,
            quote_dec: 6,
            base: vec![0; 20],
            quote: vec![1; 20],
        };
        let d = Market::decode(&m.encode());
        assert_eq!((d.pp, d.sp, d.base_dec, d.quote_dec, d.quote), (m.pp, m.sp, 18, 6, m.quote));
    }
}
