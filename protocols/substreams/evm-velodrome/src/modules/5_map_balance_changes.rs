use crate::{events::get_log_changed_balances, pb::tycho::evm::velodrome::Pool};
use anyhow::Ok;
use substreams::store::{StoreGet, StoreGetProto};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::hex::Hexable;
use tycho_substreams::models::{BlockBalanceDeltas, Transaction};

#[substreams::handlers::map]
pub fn map_balance_changes(
    block: eth::Block,
    pools_store: StoreGetProto<Pool>,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    let mut balance_deltas = Vec::new();
    for trx in block.transactions() {
        let mut tx_deltas = Vec::new();
        let tx = Transaction {
            to: trx.to.clone(),
            from: trx.from.clone(),
            hash: trx.hash.clone(),
            index: trx.index.into(),
        };
        for log in trx
            .calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(|call| &call.logs)
        {
            // Skip if the log is not from a known slipstreams pool.
            if let Some(pool) =
                pools_store.get_last(format!("{}:{}", "Pool", &log.address.to_hex()))
            {
                tx_deltas.extend(get_log_changed_balances(&tx, log, &pool))
            } else {
                continue;
            }
        }
        if !tx_deltas.is_empty() {
            balance_deltas.extend(tx_deltas);
        }
    }

    Ok(BlockBalanceDeltas { balance_deltas })
}
