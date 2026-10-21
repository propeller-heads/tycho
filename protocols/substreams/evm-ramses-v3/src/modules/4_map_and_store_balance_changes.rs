use substreams_helper::hex::Hexable;
use tycho_substreams::models::{BalanceDelta, BlockBalanceDeltas};

use crate::pb::ramses::v3::{events::pool_event::Typ, Events};
use substreams::{
    scalar::BigInt,
    store::{StoreAddBigInt, StoreNew},
};

#[substreams::handlers::map]
pub fn map_balance_changes(events: Events) -> BlockBalanceDeltas {
    let balance_deltas = events
        .pool_events
        .into_iter()
        .flat_map(|event| {
            let (delta0, delta1) = maybe_balance_deltas(event.typ.unwrap())?;
            let component_id = event.pool_address.to_hex().into_bytes();
            let tx = event.transaction;

            Some(
                [(delta0, event.token0), (delta1, event.token1)]
                    .into_iter()
                    .map(move |(delta, token)| BalanceDelta {
                        ord: event.log_ordinal,
                        tx: tx.clone(),
                        token,
                        delta,
                        component_id: component_id.clone(),
                    }),
            )
        })
        .flatten()
        .collect();

    BlockBalanceDeltas { balance_deltas }
}

#[substreams::handlers::store]
pub fn store_pools_balances(balances_deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(balances_deltas, store);
}

fn maybe_balance_deltas(ty: Typ) -> Option<(Vec<u8>, Vec<u8>)> {
    Some(match ty {
        Typ::Mint(e) => (inflow(&e.amount_0), inflow(&e.amount_1)),
        Typ::Swap(e) => (e.amount_0, e.amount_1),
        Typ::Collect(e) => (outflow(&e.amount_0), outflow(&e.amount_1)),
        Typ::Flash(e) => (inflow(&e.paid_0), inflow(&e.paid_1)),
        Typ::CollectProtocol(e) => (outflow(&e.amount_0), outflow(&e.amount_1)),
        _ => return None,
    })
}

fn inflow(unsigned: &[u8]) -> Vec<u8> {
    BigInt::from_unsigned_bytes_be(unsigned).to_signed_bytes_be()
}

fn outflow(unsigned: &[u8]) -> Vec<u8> {
    BigInt::from_unsigned_bytes_be(unsigned)
        .neg()
        .to_signed_bytes_be()
}
