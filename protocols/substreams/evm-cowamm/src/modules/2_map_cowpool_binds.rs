use crate::{
    abi::b_cow_pool::functions::Bind,
    pb::cowamm::{CowPoolBind, CowPoolBinds},
};
use anyhow::{Ok, Result};
use substreams_ethereum::pb::eth::v2::Block;
use substreams_helper::hex::Hexable;

#[substreams::handlers::map]
pub fn map_cowpool_binds(block: Block) -> Result<CowPoolBinds> {
    const BIND_TOPIC: &str = "0xe4e1e53800000000000000000000000000000000000000000000000000000000";
    const BIND_SELECTOR: &str = "e4e1e538";

    let binds = block
        .transaction_traces
        .iter()
        // extract (tx, receipt) pairs; skip tx without receipts
        .filter_map(|tx| {
            tx.receipt
                .as_ref()
                .map(|receipt| (tx, receipt))
        })
        // for each (tx, receipt) emit all the matching binds
        .flat_map(|(tx, receipt)| {
            receipt
                .logs
                .iter()
                // topic match
                .filter(|log| {
                    log.topics.first().map(|t| t.to_hex()) == Some(BIND_TOPIC.to_string())
                })
                // validate log data and map to CowPoolBind
                .filter_map(move |log| {
                    // Find the call that contains this log by matching addresses and checking calls
                    let call = tx.calls.iter().find(|call| {
                        call.address == log.address
                        && !call.state_reverted
                        && call.input.len() > 4 //checks if theres data after the function selector, if not then theres no data to decode
                        && hex::encode(&call.input[..4]) == BIND_SELECTOR //check if its the the
                                                                          // right function
                                                                          // selection selector
                    })?;
                    let bind = Bind::decode(call).expect("failed to decode bind");
                    let token = bind.token;
                    let amount = bind.balance.to_signed_bytes_be();
                    let weight = bind.denorm.to_signed_bytes_be();
                    Some(CowPoolBind {
                        address: log.address.clone(),
                        token,
                        amount,
                        weight,
                        tx: Some(tx.into()), // full TransactionTrace
                        ordinal: log.ordinal,
                    })
                })
        })
        .collect::<Vec<_>>();

    Ok(CowPoolBinds { binds })
}
