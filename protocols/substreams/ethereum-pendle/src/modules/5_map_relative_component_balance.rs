use anyhow::Result;
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetRaw},
};
use substreams_ethereum::{pb::eth::v2 as eth, Event};
use tycho_substreams::{abi::erc20, prelude::*};

use crate::keys::contract_id;

/// Tracks the ERC-20 balances the components actually custody, from `Transfer` events.
///
/// These are deliberately not the market's `totalPt` / `totalSy` — those are reserves, they live
/// in `map_reserve_deltas`, and they differ, because `totalSy` excludes donated excess that
/// `skim()` later sweeps. The balance channel carries what the indexer can reconcile against
/// chain state, which is the real token balance of the component's own address.
///
/// The market custodies SY and PT; it holds no YT, and reports nothing for it, which the
/// reconciliation reads as zero — the market's on-chain YT balance. The SY custodies whatever it
/// wraps. The market is also its own LP token, so it emits `Transfer` from its own address: those
/// are ignored because the LP token is not in the market's token list.
#[substreams::handlers::map]
pub fn map_relative_component_balance(
    block: eth::Block,
    store: StoreGetRaw,
) -> Result<BlockBalanceDeltas> {
    let mut balance_deltas = Vec::new();
    for tx in block.transactions() {
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let Some(transfer) = erc20::events::Transfer::match_and_decode(log) else { continue };
            for (holder, amount) in holder_deltas(&transfer) {
                let component_id = contract_id(&holder);
                let Some(tokens) = store.get_last(&component_id) else { continue };
                let tokens: Vec<Vec<u8>> = serde_sibor::from_bytes(&tokens)
                    .expect("deserializing component tokens from the component store");
                if !tokens.contains(&log.address) {
                    continue;
                }
                balance_deltas.push(BalanceDelta {
                    ord: log.ordinal,
                    tx: Some(tx.into()),
                    token: log.address.clone(),
                    delta: amount.to_signed_bytes_be(),
                    component_id: component_id.into_bytes(),
                });
            }
        }
    }
    Ok(BlockBalanceDeltas { balance_deltas })
}

/// Splits a transfer into the signed change it makes to each side's balance.
///
/// Both sides are returned: a transfer between two tracked components moves both, and stopping at
/// the first match would leave the other permanently overstated. Mints and burns name the zero
/// address, which is never a component, so that side is dropped here rather than costing a store
/// lookup.
///
/// A transfer whose sender and receiver are the same address yields nothing. The two deltas would
/// net to zero, but they would carry the same component, token and ordinal, and
/// `store_balance_changes` panics on an ordinal it has already seen for that pair — which would
/// halt the module for that block and every block after it.
fn holder_deltas(transfer: &erc20::events::Transfer) -> Vec<(Vec<u8>, BigInt)> {
    if transfer.value == BigInt::zero() || transfer.from == transfer.to {
        return vec![];
    }
    let mut deltas = Vec::new();
    if !is_zero_address(&transfer.to) {
        deltas.push((transfer.to.clone(), transfer.value.clone()));
    }
    if !is_zero_address(&transfer.from) {
        deltas.push((transfer.from.clone(), transfer.value.neg()));
    }
    deltas
}

fn is_zero_address(address: &[u8]) -> bool {
    address.iter().all(|byte| *byte == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKET: [u8; 20] = [0x34; 20];
    const SY: [u8; 20] = [0xcb; 20];
    const ZERO: [u8; 20] = [0x00; 20];

    fn transfer(from: [u8; 20], to: [u8; 20], value: i64) -> erc20::events::Transfer {
        erc20::events::Transfer { from: from.to_vec(), to: to.to_vec(), value: BigInt::from(value) }
    }

    /// A transfer between two tracked components moves both balances. Stopping at the first
    /// match — as the factory template does — leaves the sender permanently overstated.
    #[test]
    fn a_transfer_moves_both_sides() {
        assert_eq!(
            holder_deltas(&transfer(SY, MARKET, 100)),
            vec![(MARKET.to_vec(), BigInt::from(100)), (SY.to_vec(), BigInt::from(-100))]
        );
    }

    #[test]
    fn a_mint_only_credits_the_receiver() {
        assert_eq!(
            holder_deltas(&transfer(ZERO, MARKET, 100)),
            vec![(MARKET.to_vec(), BigInt::from(100))]
        );
    }

    #[test]
    fn a_burn_only_debits_the_sender() {
        assert_eq!(
            holder_deltas(&transfer(MARKET, ZERO, 100)),
            vec![(MARKET.to_vec(), BigInt::from(-100))]
        );
    }

    /// Zero-value transfers are legal ERC-20 and some routers emit them. They would contribute
    /// nothing but still occupy an ordinal in the aggregation.
    #[test]
    fn a_zero_transfer_moves_nothing() {
        assert!(holder_deltas(&transfer(SY, MARKET, 0)).is_empty());
    }

    /// A self-transfer emits nothing at all. Emitting the two halves that net to zero would put
    /// two deltas for one component, token and ordinal into the aggregation, which panics on a
    /// repeated ordinal rather than netting them.
    #[test]
    fn a_self_transfer_emits_nothing() {
        assert!(holder_deltas(&transfer(MARKET, MARKET, 100)).is_empty());
    }

    /// The aggregation the module feeds rejects a repeated ordinal for one component and token,
    /// so no single log may ever produce two deltas that collide on all three.
    #[test]
    fn no_log_yields_two_deltas_for_the_same_component_and_token() {
        for (from, to) in [(MARKET, MARKET), (SY, MARKET), (ZERO, MARKET), (MARKET, ZERO)] {
            let deltas = holder_deltas(&transfer(from, to, 100));
            let mut holders: Vec<&Vec<u8>> = deltas
                .iter()
                .map(|(holder, _)| holder)
                .collect();
            holders.sort();
            let count = holders.len();
            holders.dedup();
            assert_eq!(holders.len(), count, "duplicate holder for a transfer {from:?} -> {to:?}");
        }
    }
}
