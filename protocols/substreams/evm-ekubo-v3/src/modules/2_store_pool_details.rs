use alloy_primitives::B256;
use ekubo_sdk::chain::evm::{EvmPoolConfig, EvmPoolTypeConfig};
use substreams::store::{StoreNew, StoreSetIfNotExists, StoreSetIfNotExistsProto};
use substreams_helper::hex::Hexable;

use crate::pb::ekubo::{
    block_transaction_events::transaction_events::pool_log::Event, BlockTransactionEvents,
    PoolDetails,
};

// Since only the PoolInitialized event contains the complete pool key we need to store some info
// required when processing other events
#[substreams::handlers::store]
fn store_pool_details(
    block_tx_events: BlockTransactionEvents,
    store: StoreSetIfNotExistsProto<PoolDetails>,
) {
    let store = &store;

    block_tx_events
        .block_transaction_events
        .into_iter()
        .for_each(|tx_events| {
            tx_events
                .pool_logs
                .into_iter()
                .flat_map(|log| {
                    maybe_pool_details(log.event.unwrap()).map(|details| (details, log.pool_id))
                })
                .for_each(move |(pool_details, pool_id)| {
                    store.set_if_not_exists(0, pool_id.to_hex(), &pool_details);
                })
        });
}

fn maybe_pool_details(event: Event) -> Option<PoolDetails> {
    let Event::PoolInitialized(pi) = event else {
        return None;
    };

    let config = EvmPoolConfig::try_from(
        B256::try_from(pi.config.as_slice()).expect("pool config to be 32 bytes long"),
    )
    .expect("pool config to be valid");

    Some(PoolDetails {
        token0: pi.token0,
        token1: pi.token1,
        is_stableswap: matches!(
            config.pool_type_config,
            EvmPoolTypeConfig::FullRange(_) | EvmPoolTypeConfig::Stableswap(_)
        ),
    })
}
