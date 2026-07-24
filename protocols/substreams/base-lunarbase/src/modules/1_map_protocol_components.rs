use anyhow::Result;
use substreams_ethereum::pb::eth;
use tycho_substreams::prelude as tycho;

use crate::{
    lunarbase,
    modules::config::{Config, PoolConfig},
};

#[substreams::handlers::map]
pub fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<tycho::BlockTransactionProtocolComponents> {
    let config = Config::parse(&params)?;
    let mut tx_components = Vec::<tycho::TransactionProtocolComponents>::new();
    for pool in config
        .pools
        .iter()
        .filter(|pool| pool.bootstrap_block == Some(block.number))
    {
        let Some(tx) = block
            .transactions()
            .find(|tx| has_pool_log(tx, pool))
        else {
            continue;
        };
        let component = lunarbase::protocol_component(pool.pool, pool.token_x, pool.token_y);
        if let Some(existing) = tx_components
            .iter_mut()
            .find(|tx_components| {
                tx_components
                    .tx
                    .as_ref()
                    .is_some_and(|known| known.hash == tx.hash)
            })
        {
            existing.components.push(component);
        } else {
            tx_components.push(tycho::TransactionProtocolComponents {
                tx: Some(tx.into()),
                components: vec![component],
            });
        }
    }

    Ok(tycho::BlockTransactionProtocolComponents { tx_components })
}

fn has_pool_log(tx: &eth::v2::TransactionTrace, pool: &PoolConfig) -> bool {
    tx.logs_with_calls()
        .any(|(log, _)| log.address == pool.pool)
}
