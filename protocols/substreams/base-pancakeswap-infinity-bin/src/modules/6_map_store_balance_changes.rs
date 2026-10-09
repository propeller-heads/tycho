use crate::{parameters, pb::pancakeswap::infinity::bin::BinDeltas};
use std::collections::BTreeMap;
use substreams::{
    scalar::BigInt,
    store::{StoreAddBigInt, StoreNew},
};
use substreams_helper::hex::Hexable;
use tycho_substreams::models::{BalanceDelta, BlockBalanceDeltas, Transaction};

/// `(ordinal, component id, token) -> (summed delta, the log's transaction)`.
type BalanceTotals = BTreeMap<(u64, Vec<u8>, Vec<u8>), (BigInt, Option<Transaction>)>;

/// Per-pool token balance deltas from the bin reserve changes. Bin reserves are absolute, so the
/// delta is a subtraction; CL has to reconstruct position amounts with `uni_math`.
#[substreams::handlers::map]
pub fn map_balance_changes(
    bin_deltas: BinDeltas,
) -> Result<BlockBalanceDeltas, substreams::errors::Error> {
    Ok(BlockBalanceDeltas { balance_deltas: balance_deltas(bin_deltas) })
}

/// Split out so tests can call it: the handler macro rewrites the signature to take raw pointers.
///
/// One event touches many bins (a `Mint`'s `ids[]`, a `Swap` crossing bins) and every resulting
/// `BinDelta` carries that log's ordinal, while `store_balance_changes` panics unless ordinals are
/// strictly increasing per (component, token). Summing per log is also the right number: the pool's
/// balance moved by the total across the bins the event touched.
fn balance_deltas(bin_deltas: BinDeltas) -> Vec<BalanceDelta> {
    // Keyed by ordinal first, so the iteration order is the order the store requires.
    let mut totals = BalanceTotals::new();

    for delta in bin_deltas.deltas {
        let (old_x, old_y) = parameters::unpack_reserves(&packed(&delta.old_packed));
        let (new_x, new_y) = parameters::unpack_reserves(&packed(&delta.new_packed));

        // UTF-8 of the 0x-hex id, as 1_map_pool_created emits it.
        let component_id = delta
            .pool_id
            .to_hex()
            .as_bytes()
            .to_vec();
        let tx = delta
            .transaction
            .as_ref()
            .map(Into::into);

        // X is currency0, Y is currency1. Signed: burns and sells go negative.
        for (token, new, old) in [(delta.currency0, new_x, old_x), (delta.currency1, new_y, old_y)]
        {
            let entry = totals
                .entry((delta.ordinal, component_id.clone(), token))
                .or_insert_with(|| (BigInt::zero(), tx.clone()));
            entry.0 = entry.0.clone() +
                (BigInt::from_unsigned_bytes_be(&new.to_be_bytes()) -
                    BigInt::from_unsigned_bytes_be(&old.to_be_bytes()));
        }
    }

    totals
        .into_iter()
        .map(|((ord, component_id, token), (total, tx))| BalanceDelta {
            token,
            delta: total.to_signed_bytes_be(),
            component_id,
            ord,
            tx,
        })
        .collect()
}

/// Packed reserve word. Short or missing reads as an empty bin, which is what a creation or
/// deletion writes.
fn packed(value: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    if value.len() == 32 {
        out.copy_from_slice(value);
    }
    out
}

/// Accumulates the deltas into absolute balances per (pool, token).
#[substreams::handlers::store]
pub fn store_pools_balances(balances_deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(balances_deltas, store);
}

#[cfg(test)]
mod tests {
    use crate::pb::pancakeswap::infinity::bin::BinDelta;

    use super::*;
    use rstest::rstest;

    const POOL_ID: [u8; 32] = [0xab; 32];
    const TOKEN0: [u8; 20] = [0x11; 20];
    const TOKEN1: [u8; 20] = [0x22; 20];

    fn word(x: u128, y: u128) -> Vec<u8> {
        let mut out = vec![0u8; 32];
        out[..16].copy_from_slice(&y.to_be_bytes());
        out[16..].copy_from_slice(&x.to_be_bytes());
        out
    }

    fn deltas(old: Vec<u8>, new: Vec<u8>) -> Vec<BalanceDelta> {
        balance_deltas(BinDeltas {
            deltas: vec![BinDelta {
                pool_id: POOL_ID.to_vec(),
                currency0: TOKEN0.to_vec(),
                currency1: TOKEN1.to_vec(),
                bin_id: 7,
                old_packed: old,
                new_packed: new,
                ordinal: 3,
                transaction: None,
                was_in_tree: true,
                in_tree: true,
            }],
        })
    }

    fn amount(delta: &BalanceDelta) -> BigInt {
        BigInt::from_signed_bytes_be(&delta.delta)
    }

    /// Reserves are absolute, so the delta is a subtraction and has to be signed: a burn or a sold
    /// side goes negative. Expectations are `(x, y)` for one bin delta, which is one balance delta
    /// per token.
    #[rstest]
    #[case::mint_into_empty_bin((0, 0), (10, 20), 10, 20)]
    #[case::burn((10, 20), (4, 5), -6, -15)]
    #[case::swap((10, 20), (13, 16), 3, -4)]
    #[case::deletion_returns_the_reserve((10, 20), (0, 0), -10, -20)]
    fn test_balance_deltas(
        #[case] old: (u128, u128),
        #[case] new: (u128, u128),
        #[case] expected_x: i64,
        #[case] expected_y: i64,
    ) {
        let out = deltas(word(old.0, old.1), word(new.0, new.1));

        assert_eq!(out.len(), 2, "one balance delta per token");
        assert_eq!(out[0].token, TOKEN0.to_vec());
        assert_eq!(amount(&out[0]), BigInt::from(expected_x));
        assert_eq!(out[1].token, TOKEN1.to_vec());
        assert_eq!(amount(&out[1]), BigInt::from(expected_y));
        assert_eq!(out[0].ord, 3, "deltas carry the log's ordinal");
    }

    /// A Mint touching three bins is one log, so it must yield one delta per token carrying the
    /// summed amounts. Emitting one per bin repeats the ordinal and panics the balance store.
    #[test]
    fn bins_touched_by_one_event_are_summed_per_token() {
        let bin = |bin_id: u32, x: u128, y: u128| BinDelta {
            pool_id: POOL_ID.to_vec(),
            currency0: TOKEN0.to_vec(),
            currency1: TOKEN1.to_vec(),
            bin_id,
            old_packed: word(0, 0),
            new_packed: word(x, y),
            ordinal: 3,
            transaction: None,
            was_in_tree: false,
            in_tree: false,
        };

        let out =
            balance_deltas(BinDeltas { deltas: vec![bin(6, 1, 10), bin(7, 2, 20), bin(8, 4, 0)] });

        assert_eq!(out.len(), 2, "one delta per token, not per bin");
        assert_eq!(amount(&out[0]), BigInt::from(7u64));
        assert_eq!(amount(&out[1]), BigInt::from(30u64));
        assert!(out.iter().all(|delta| delta.ord == 3), "summed deltas keep the log's ordinal");
    }

    /// Component id is the 0x-hex string bytes, as 1_map_pool_created emits.
    #[test]
    fn component_id_is_the_hex_string_bytes() {
        let out = deltas(word(0, 0), word(1, 1));
        assert_eq!(
            out[0].component_id,
            POOL_ID
                .to_vec()
                .to_hex()
                .as_bytes()
                .to_vec()
        );
    }
}
