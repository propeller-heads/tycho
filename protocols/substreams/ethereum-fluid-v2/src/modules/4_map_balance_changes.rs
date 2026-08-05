use crate::{
    events::get_log_changed_balances, modules::utils::Params, pb::tycho::evm::fluid_v2::Pool,
};
use anyhow::Ok;
use substreams::store::{StoreGet, StoreGetProto};
use substreams_ethereum::pb::eth::v2::{self as eth};
use tycho_substreams::models::{BlockBalanceDeltas, Transaction};

#[substreams::handlers::map]
pub fn map_balance_changes(
    params: String,
    block: eth::Block,
    pools_store: StoreGetProto<Pool>,
) -> Result<BlockBalanceDeltas, anyhow::Error> {
    let params = Params::parse_from_query(&params)?;
    let dex_v2_address = hex::decode(&params.dex_v2_address).expect("Invalid dex_v2_address");

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
            tx_deltas.extend(get_log_changed_balances(&tx, log, &dex_v2_address, &pools_store));
        }
        if !tx_deltas.is_empty() {
            balance_deltas.extend(tx_deltas);
        }
    }

    Ok(BlockBalanceDeltas { balance_deltas })
}
