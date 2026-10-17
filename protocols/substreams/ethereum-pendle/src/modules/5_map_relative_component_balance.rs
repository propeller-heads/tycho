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
    sy::balance_seeds,
    transfers::{component_transfers, new_sy_components},
};

/// Tracks the ERC-20 balances the components actually custody, from `Transfer` events.
///
/// These are deliberately not the market's `totalPt` / `totalSy` — those are reserves, they live
/// in `map_reserve_deltas`, and they differ, because `totalSy` excludes donated excess that
/// `skim()` later sweeps. The balance channel carries what the indexer can reconcile against
/// chain state, which is the real token balance of the component's own address.
///
/// The market custodies SY and PT; it holds no YT, and reports nothing for it, which the
/// reconciliation reads as zero — the market's on-chain YT balance. The market is also its own LP
/// token, so it emits `Transfer` from its own address: those are ignored because the LP token is
/// not in the market's token list.
///
/// An SY usually held tokens long before it became a component with its first market, so its
/// balances are seeded from the `balanceOf` reads taken at its creation and moved by transfers
/// afterwards. The seed is read at the end of the creation block, so that block's transfers are
/// already in it and are skipped. A rebasing token moves without a `Transfer`, so an SY's
/// reported holding of one misses every rebase since creation. The simulation reads only an SY's
/// holding of its own yield token, to bound a redemption to that token.
#[substreams::handlers::map]
pub fn map_relative_component_balance(
    block: eth::Block,
    new_components: BlockTransactionProtocolComponents,
    store: StoreGetRaw,
) -> Result<BlockBalanceDeltas> {
    let created = new_sy_components(&new_components);
    let seeded: HashSet<&str> = created
        .iter()
        .map(|(_, component)| component.id.as_str())
        .collect();

    let mut balance_deltas = Vec::new();
    for transfer in component_transfers(&block, &store) {
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
    for (tx, component) in created {
        for (token, balance) in balance_seeds(component) {
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
    Ok(in_transaction_order(balance_deltas))
}
