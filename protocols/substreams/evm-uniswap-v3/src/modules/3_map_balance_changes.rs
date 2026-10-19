use anyhow::Ok;
use substreams::store::{StoreGet, StoreGetProto};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::hex::Hexable;

use crate::{
    events::get_log_changed_balances,
    pb::uniswap::v3::{BalanceDeltas, Pool},
};

#[substreams::handlers::map]
pub fn map_balance_changes(
    block: eth::Block,
    pools_store: StoreGetProto<Pool>,
) -> Result<BalanceDeltas, anyhow::Error> {
    let mut balances_deltas = Vec::new();
    for trx in block.transactions() {
        let mut tx_deltas = Vec::new();
        for log in trx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| &call.logs)
        {
            // Skip if the log is not from a known uniswapV3 pool.
            if let Some(pool) =
                pools_store.get_last(format!("{}:{}", "Pool", &log.address.to_hex()))
            {
                tx_deltas.extend(get_log_changed_balances(log, &pool))
            } else {
                continue;
            }
        }
        if !tx_deltas.is_empty() {
            balances_deltas.extend(tx_deltas);
        }
    }

    Ok(BalanceDeltas { deltas: balances_deltas })
}
