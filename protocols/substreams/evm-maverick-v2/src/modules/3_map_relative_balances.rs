use crate::{events::get_log_changed_balances, pb::maverick::v2::Pool};
use anyhow::{Ok, Result};
use substreams::{prelude::StoreGetProto, store::StoreGet};
use substreams_ethereum::pb::eth::v2::Block;
use substreams_helper::hex::Hexable;
use tycho_substreams::prelude::*;

#[substreams::handlers::map]
pub fn map_relative_balances(
    block: Block,
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
            if let Some(pool) = pools_store.get_last(format!("Pool:{}", &log.address.to_hex())) {
                tx_deltas.extend(get_log_changed_balances(&tx, log, &pool));
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
