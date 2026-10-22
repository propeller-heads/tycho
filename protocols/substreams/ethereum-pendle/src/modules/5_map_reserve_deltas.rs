use std::collections::HashSet;

use anyhow::Result;
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetRaw},
};
use substreams_ethereum::pb::eth::v2 as eth;
use tycho_substreams::prelude::*;

use crate::{
    deltas::in_transaction_order,
    keys::{contract_id, market_tokens_key},
    market_state::{reserve_delta, ReserveDelta},
    sy::supply_seed,
    transfers::{new_sy_components, supply_changes, SupplyChange},
};

/// Emits the per-event changes to the reserves quotes rest on: every market's PT and SY reserves,
/// and every SY's supply.
///
/// The deltas are relative and are accumulated by `store_market_reserves`; `map_protocol_changes`
/// turns the accumulated totals into the `total_pt` / `total_sy` attributes of a market and the
/// `sy_total_supply` attribute of an SY. A market's ordinals come from the log, which makes them
/// strictly increasing per market and token within a block, as the aggregation requires.
///
/// An SY's supply is seeded once, from the `totalSupply()` read at its creation, and moved
/// afterwards by its own mints and burns, so it costs no read after creation. SY shares do not
/// rebase and every mint and burn emits a `Transfer`, which is what makes the running total exact.
/// The seed is read at the end of the creation block, so that block's own mints and burns are
/// already in it and are skipped.
#[substreams::handlers::map]
pub fn map_reserve_deltas(
    block: eth::Block,
    new_components: BlockTransactionProtocolComponents,
    store: StoreGetRaw,
) -> Result<BlockBalanceDeltas> {
    let mut balance_deltas = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let Some(delta) = reserve_delta(log) else { continue };
            let component_id = contract_id(&log.address);
            let Some(roles) = store.get_last(market_tokens_key(&component_id)) else { continue };
            let roles: Vec<Vec<u8>> = serde_sibor::from_bytes(&roles)
                .expect("deserializing market token roles from the component store");
            let [sy, pt, _yt] = roles.as_slice() else {
                panic!("market {component_id} stored {} roles, expected 3", roles.len())
            };
            for (token, amount) in nonzero_sides(pt, sy, delta) {
                balance_deltas.push(BalanceDelta {
                    ord: log.ordinal,
                    tx: Some(tx.into()),
                    token: token.clone(),
                    delta: amount.to_signed_bytes_be(),
                    component_id: component_id.as_bytes().to_vec(),
                });
            }
        }
    }

    let created = new_sy_components(&new_components);
    balance_deltas.extend(sy_supply_deltas(supply_changes(&block, &store), &created));
    Ok(in_transaction_order(balance_deltas))
}

/// A market reserve change as a delta per token, leaving out a side that did not move.
///
/// A one-sided `Burn` leaves one side at zero. `aggregate_balances_changes` pairs store deltas with
/// these one to one, so a delta that may write no store delta is never emitted.
fn nonzero_sides<'a>(
    pt: &'a Vec<u8>,
    sy: &'a Vec<u8>,
    delta: ReserveDelta,
) -> Vec<(&'a Vec<u8>, BigInt)> {
    [(pt, delta.pt), (sy, delta.sy)]
        .into_iter()
        .filter(|(_, amount)| *amount != BigInt::zero())
        .collect()
}

/// Each SY supply change as a delta to the SY's holding of its own shares, plus the supply seed of
/// every SY in `created` at ordinal 0 of the transaction that created it.
///
/// A created SY's own changes are left out, whichever transaction of the block they are in: its
/// seed is read at the end of the block and already includes them.
///
/// Panics if a created SY carries no supply seed.
fn sy_supply_deltas(
    changes: Vec<SupplyChange>,
    created: &[(&Transaction, &ProtocolComponent)],
) -> Vec<BalanceDelta> {
    let seeded: HashSet<&str> = created
        .iter()
        .map(|(_, component)| component.id.as_str())
        .collect();
    let mut balance_deltas = Vec::new();
    for change in changes {
        if seeded.contains(change.sy_id.as_str()) {
            continue;
        }
        balance_deltas.push(BalanceDelta {
            ord: change.log.ordinal,
            tx: Some(change.tx.into()),
            token: change.log.address.clone(),
            delta: change.amount.to_signed_bytes_be(),
            component_id: change.sy_id.into_bytes(),
        });
    }
    for &(tx, component) in created {
        let Some(supply) = supply_seed(component) else {
            panic!("SY {} was created without its supply seed", component.id)
        };
        // An SY created empty adds nothing; `map_protocol_changes` emits its zero supply directly.
        if supply == BigInt::zero() {
            continue;
        }
        balance_deltas.push(BalanceDelta {
            ord: 0,
            tx: Some(tx.clone()),
            token: hex::decode(component.id.trim_start_matches("0x"))
                .expect("SY component id is not hex"),
            delta: supply.to_signed_bytes_be(),
            component_id: component.id.clone().into_bytes(),
        });
    }
    balance_deltas
}

#[cfg(test)]
mod tests {
    use substreams::scalar::BigInt;
    use substreams_ethereum::pb::eth::v2::{Log, TransactionTrace};

    use super::*;
    use crate::{consts::PENDLE_SY, sy::CREATION_TOTAL_SUPPLY};

    /// The wstETH SY, created in the block under test, and an SY created earlier.
    const NEW_SY: &str = "0xcbc72d92b2dc8187414f6734718563898740c0bc";
    const OLD_SY: &str = "0x00000000000000000000000000000000000000b1";

    fn trace(index: u32) -> TransactionTrace {
        TransactionTrace { index, hash: vec![index as u8; 32], status: 1, ..Default::default() }
    }

    /// A mint or burn of `sy`'s shares, as the SY itself logs it.
    fn share_log(sy: &str, ordinal: u64) -> Log {
        Log { address: hex::decode(&sy[2..]).unwrap(), ordinal, ..Default::default() }
    }

    fn change<'a>(tx: &'a TransactionTrace, log: &'a Log, amount: i64) -> SupplyChange<'a> {
        let sy_id = contract_id(&log.address);
        SupplyChange { tx, log, sy_id, amount: BigInt::from(amount) }
    }

    fn sy(id: &str, supply: Option<i64>) -> ProtocolComponent {
        let mut attributes = vec![];
        if let Some(supply) = supply {
            attributes.push((CREATION_TOTAL_SUPPLY, BigInt::from(supply).to_signed_bytes_be()));
        }
        ProtocolComponent::new(id)
            .with_attributes(&attributes)
            .as_swap_type(PENDLE_SY, ImplementationType::Custom)
    }

    /// Each delta as its transaction, ordinal, component, token and amount.
    fn rows(deltas: Vec<BalanceDelta>) -> Vec<(u64, u64, String, String, BigInt)> {
        in_transaction_order(deltas)
            .balance_deltas
            .into_iter()
            .map(|delta| {
                (
                    delta.tx.unwrap().index,
                    delta.ord,
                    String::from_utf8(delta.component_id).unwrap(),
                    contract_id(&delta.token),
                    BigInt::from_signed_bytes_be(&delta.delta),
                )
            })
            .collect()
    }

    /// An SY created this block starts from its end-of-block supply, written at ordinal 0 of the
    /// creating transaction. Its own mints and burns anywhere in the block are already in that
    /// seed and are skipped; another SY's are kept.
    #[test]
    fn a_new_sy_is_seeded_and_its_creation_block_skipped() {
        let (tx1, tx2, tx3) = (trace(1), trace(2), trace(3));
        let early = share_log(NEW_SY, 5);
        let late = share_log(NEW_SY, 30);
        let other = share_log(OLD_SY, 31);
        let changes =
            vec![change(&tx1, &early, 400), change(&tx3, &late, -100), change(&tx3, &other, 7)];
        let creation: Transaction = (&tx2).into();
        let component = sy(NEW_SY, Some(300));

        assert_eq!(
            rows(sy_supply_deltas(changes, &[(&creation, &component)])),
            vec![
                (2, 0, NEW_SY.to_string(), NEW_SY.to_string(), BigInt::from(300)),
                (3, 31, OLD_SY.to_string(), OLD_SY.to_string(), BigInt::from(7)),
            ]
        );
    }

    /// Without a creation, every change is kept.
    #[test]
    fn an_existing_sy_follows_its_mints_and_burns() {
        let (tx1, tx3) = (trace(1), trace(3));
        let mint = share_log(NEW_SY, 5);
        let burn = share_log(NEW_SY, 30);
        assert_eq!(
            rows(sy_supply_deltas(vec![change(&tx1, &mint, 400), change(&tx3, &burn, -100)], &[])),
            vec![
                (1, 5, NEW_SY.to_string(), NEW_SY.to_string(), BigInt::from(400)),
                (3, 30, NEW_SY.to_string(), NEW_SY.to_string(), BigInt::from(-100)),
            ]
        );
    }

    /// A zero supply seed is no delta: whether adding zero writes a store delta is up to the
    /// engine, and the aggregation pairs the two one to one. `map_protocol_changes` emits the SY's
    /// zero supply directly instead.
    #[test]
    fn a_zero_supply_seed_is_not_a_delta() {
        let creation: Transaction = (&trace(2)).into();
        let component = sy(NEW_SY, Some(0));
        assert!(rows(sy_supply_deltas(vec![], &[(&creation, &component)])).is_empty());
    }

    /// A `Burn` that returns only SY moves the SY reserve alone; the zero PT side is no delta.
    #[test]
    fn a_one_sided_burn_moves_one_reserve() {
        let (pt, sy) = (vec![0x0b; 20], vec![0xcb; 20]);
        let burn = ReserveDelta { pt: BigInt::zero(), sy: BigInt::from(-500) };
        assert_eq!(nonzero_sides(&pt, &sy, burn), vec![(&sy, BigInt::from(-500))]);
    }

    #[test]
    #[should_panic(expected = "created without its supply seed")]
    fn an_sy_created_without_its_seed_halts() {
        let creation: Transaction = (&trace(2)).into();
        let component = sy(NEW_SY, None);
        sy_supply_deltas(vec![], &[(&creation, &component)]);
    }
}
