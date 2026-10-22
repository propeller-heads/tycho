use std::collections::HashSet;

use anyhow::Result;
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetRaw, StoreGetString},
};
use substreams_ethereum::pb::eth::v2 as eth;
use tycho_substreams::prelude::*;

use crate::{
    deltas::in_transaction_order,
    shares::{is_share_holder, share_ratio_key, ShareRatio},
    sy::balance_seeds,
    transfers::{component_transfers, new_sy_components, ComponentTransfer},
};

/// Tracks the ERC-20 balances the components actually custody, from `Transfer` events.
///
/// These are deliberately not the market's `totalPt` / `totalSy` — those are reserves, they live
/// in `map_reserve_deltas`, and they differ, because `totalSy` excludes donated excess that
/// `skim()` later sweeps. The balance channel carries what the indexer can reconcile against
/// chain state, which is the real token balance of the component's own address.
///
/// The market custodies SY and PT; it holds no YT, and reports nothing for it here.
/// `map_protocol_changes` reports the market's YT balance as zero, which it is on chain, so that
/// Tycho counts YT as traded. The market is also its own LP
/// token, so it emits `Transfer` from its own address: those are ignored because the LP token is
/// not in the market's token list.
///
/// An SY usually held tokens long before it became a component with its first market, so its
/// balances are seeded from the `balanceOf` reads taken at its creation and moved by transfers
/// afterwards. The seed is read at the end of the creation block, so that block's transfers are
/// already in it and are skipped. The simulation reads only an SY's holding of its own yield
/// token, to bound a redemption to that token.
///
/// A token that keeps balances in shares (`crate::shares`) is followed in shares instead, for the
/// SYs `store_share_holders` lists: its seed is converted at the token's ratio then, and its
/// `TransferShares` replace its `Transfer`s. `map_protocol_changes` turns the shares back into a
/// balance. Any other token that rebases still moves without a `Transfer`, and an SY's reported
/// holding of it misses every rebase since creation.
#[substreams::handlers::map]
pub fn map_relative_component_balance(
    block: eth::Block,
    new_components: BlockTransactionProtocolComponents,
    store: StoreGetRaw,
    ratios: StoreGetString,
    holders: StoreGetString,
) -> Result<BlockBalanceDeltas> {
    let created = new_sy_components(&new_components);
    let in_shares =
        |component_id: &str, token: &[u8]| is_share_holder(&holders, component_id, token);
    let ratio = |token: &[u8]| {
        ratios
            .get_last(share_ratio_key(token))
            .and_then(|value| ShareRatio::decode(&value))
    };
    Ok(in_transaction_order(balance_deltas(
        component_transfers(&block, &store, in_shares),
        &created,
        |component_id, token, balance| {
            if !in_shares(component_id, token) {
                return balance;
            }
            let ratio =
                ratio(token).expect("an SY holds a token in shares only once it has a ratio");
            ratio.to_shares(&balance)
        },
    )))
}

/// Each component transfer as a balance delta, plus the balance seeds of every SY in `created` at
/// ordinal 0 of the transaction that created it.
///
/// A created SY's own transfers are left out, whichever transaction of the block they are in: its
/// seeds are read at the end of the block and already include them. Each seed goes through
/// `seed_unit`, which turns the balance read at creation into the unit the balance is kept in. A
/// zero seed is left out.
fn balance_deltas(
    transfers: Vec<ComponentTransfer>,
    created: &[(&Transaction, &ProtocolComponent)],
    seed_unit: impl Fn(&str, &[u8], BigInt) -> BigInt,
) -> Vec<BalanceDelta> {
    let seeded: HashSet<&str> = created
        .iter()
        .map(|(_, component)| component.id.as_str())
        .collect();

    let mut balance_deltas = Vec::new();
    for transfer in transfers {
        if seeded.contains(transfer.component_id.as_str()) {
            continue;
        }
        balance_deltas.push(BalanceDelta {
            ord: transfer.log.ordinal,
            tx: Some(transfer.tx.into()),
            token: transfer.log.address.clone(),
            delta: transfer.amount.to_signed_bytes_be(),
            component_id: transfer.component_id.into_bytes(),
        });
    }
    for &(tx, component) in created {
        for (token, balance) in balance_seeds(component) {
            let balance = seed_unit(&component.id, &token, balance);
            // A zero seed adds nothing, and the aggregation pairs store deltas with these one
            // to one, so it is left out rather than written as an empty change.
            if balance == BigInt::zero() {
                continue;
            }
            balance_deltas.push(BalanceDelta {
                ord: 0,
                tx: Some(tx.clone()),
                token,
                delta: balance.to_signed_bytes_be(),
                component_id: component.id.clone().into_bytes(),
            });
        }
    }
    balance_deltas
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::{Log, TransactionTrace};

    use super::*;
    use crate::{consts::PENDLE_SY, keys::contract_id, sy::CREATION_BALANCE_PREFIX};

    /// The wstETH SY, created in the block under test, its wstETH and the wstETH market; and an
    /// SY created earlier.
    const NEW_SY: &str = "0xcbc72d92b2dc8187414f6734718563898740c0bc";
    const WSTETH: &str = "0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0";
    const MARKET: &str = "0x34280882267ffa6383b363e278b027be083bbe3b";
    const OLD_SY: &str = "0x00000000000000000000000000000000000000b1";
    /// A token the new SY holds none of at creation.
    const EMPTY_TOKEN: &str = "0x00000000000000000000000000000000000000e1";

    fn trace(index: u32) -> TransactionTrace {
        TransactionTrace { index, hash: vec![index as u8; 32], status: 1, ..Default::default() }
    }

    fn token_log(token: &str, ordinal: u64) -> Log {
        Log { address: hex::decode(&token[2..]).unwrap(), ordinal, ..Default::default() }
    }

    fn transfer<'a>(
        tx: &'a TransactionTrace,
        log: &'a Log,
        component: &str,
        amount: i64,
    ) -> ComponentTransfer<'a> {
        ComponentTransfer {
            tx,
            log,
            component_id: component.to_string(),
            amount: BigInt::from(amount),
        }
    }

    fn new_sy(seeds: &[(&str, i64)]) -> ProtocolComponent {
        let attributes: Vec<(String, Vec<u8>)> = seeds
            .iter()
            .map(|(token, balance)| {
                (
                    format!("{CREATION_BALANCE_PREFIX}{}", &token[2..]),
                    BigInt::from(*balance).to_signed_bytes_be(),
                )
            })
            .collect();
        ProtocolComponent::new(NEW_SY)
            .with_attributes(&attributes)
            .as_swap_type(PENDLE_SY, ImplementationType::Custom)
    }

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

    /// An SY created this block starts from its end-of-block holdings, written at ordinal 0 of the
    /// creating transaction; a token it holds none of gets no seed. Its own transfers anywhere in
    /// the block are already in those seeds and are skipped, while the other side of the same
    /// transfer, and every other component's transfers, are kept.
    #[test]
    fn a_new_sy_is_seeded_and_its_creation_block_skipped() {
        let (tx1, tx2, tx3) = (trace(1), trace(2), trace(3));
        let deposit = token_log(WSTETH, 5);
        let to_market = token_log(NEW_SY, 30);
        let elsewhere = token_log(WSTETH, 31);
        let transfers = vec![
            transfer(&tx1, &deposit, NEW_SY, 400),
            transfer(&tx3, &to_market, MARKET, 50),
            transfer(&tx3, &to_market, NEW_SY, -50),
            transfer(&tx3, &elsewhere, OLD_SY, 7),
        ];
        let creation: Transaction = (&tx2).into();
        let component = new_sy(&[(WSTETH, 1000), (EMPTY_TOKEN, 0)]);

        assert_eq!(
            rows(balance_deltas(transfers, &[(&creation, &component)], |_, _, balance| balance)),
            vec![
                (2, 0, NEW_SY.to_string(), WSTETH.to_string(), BigInt::from(1000)),
                (3, 30, MARKET.to_string(), NEW_SY.to_string(), BigInt::from(50)),
                (3, 31, OLD_SY.to_string(), WSTETH.to_string(), BigInt::from(7)),
            ]
        );
    }

    /// Without a creation, every transfer is kept.
    #[test]
    fn an_existing_component_follows_its_transfers() {
        let (tx1, tx3) = (trace(1), trace(3));
        let deposit = token_log(WSTETH, 5);
        let withdrawal = token_log(WSTETH, 30);
        assert_eq!(
            rows(balance_deltas(
                vec![
                    transfer(&tx1, &deposit, NEW_SY, 400),
                    transfer(&tx3, &withdrawal, NEW_SY, -100)
                ],
                &[],
                |_, _, balance| balance,
            )),
            vec![
                (1, 5, NEW_SY.to_string(), WSTETH.to_string(), BigInt::from(400)),
                (3, 30, NEW_SY.to_string(), WSTETH.to_string(), BigInt::from(-100)),
            ]
        );
    }
}
