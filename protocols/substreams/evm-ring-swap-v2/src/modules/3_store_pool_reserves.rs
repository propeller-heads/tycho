use substreams::{
    prelude::BigInt,
    store::{StoreGet, StoreGetProto, StoreNew, StoreSet, StoreSetBigInt},
};
use substreams_ethereum::pb::eth::v2 as eth;
use substreams_helper::{event_handler::EventHandler, hex::Hexable};

use crate::{abi::pool::events::Sync, store_key::StoreKey, traits::PoolAddresser};
use tycho_substreams::prelude::*;

/// Stores each pair reserve in solver-facing component token order.
#[substreams::handlers::store]
pub fn store_pool_reserves(
    block: eth::Block,
    pools_store: StoreGetProto<ProtocolComponent>,
    store: StoreSetBigInt,
) {
    let mut on_sync = |event: Sync, _tx: &eth::TransactionTrace, log: &eth::Log| {
        let pool_address = log.address.to_hex();
        let pool = pools_store.must_get_last(StoreKey::Pool.get_unique_key(&pool_address));
        let reserves = exposed_reserves(&pool, event.reserve0, event.reserve1);

        for (token, reserve) in pool.tokens.iter().zip(reserves) {
            store.set(
                log.ordinal,
                StoreKey::PoolReserve.get_unique_key(&format!(
                    "{}:{}",
                    pool.id,
                    hex::encode(token)
                )),
                &reserve,
            );
        }
    };

    let mut event_handler = EventHandler::new(&block);
    event_handler.filter_by_address(PoolAddresser { store: &pools_store });
    event_handler.on::<Sync, _>(&mut on_sync);
    event_handler.handle_events();
}

/// Converts raw pair reserves (FewToken order) into solver-facing underlying token order.
pub fn exposed_reserves(
    pool: &ProtocolComponent,
    reserve0: BigInt,
    reserve1: BigInt,
) -> [BigInt; 2] {
    if static_attribute_byte(pool, "reserves_inverted") == 1 {
        [reserve1, reserve0]
    } else {
        [reserve0, reserve1]
    }
}

fn static_attribute_byte(pool: &ProtocolComponent, name: &str) -> u8 {
    pool.static_att
        .iter()
        .find(|att| att.name == name)
        .and_then(|att| att.value.last())
        .copied()
        .unwrap_or_else(|| panic!("Ring pool {} is missing the {} static attribute", pool.id, name))
}
