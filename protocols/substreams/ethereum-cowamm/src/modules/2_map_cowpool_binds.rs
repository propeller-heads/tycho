use crate::{
    abi::b_cow_pool::functions::{Bind, Unbind},
    pb::cowamm::{BindingChangeType, CowPoolBind, CowPoolBinds},
};
use anyhow::Result;
use substreams_ethereum::pb::eth::v2::Block;
use substreams_helper::hex::Hexable;

const BIND_TOPIC: &str = "0xe4e1e53800000000000000000000000000000000000000000000000000000000";
const BIND_SELECTOR: &str = "e4e1e538";
const UNBIND_TOPIC: &str = "0xcf5e7bd300000000000000000000000000000000000000000000000000000000";
const UNBIND_SELECTOR: &str = "cf5e7bd3";

#[substreams::handlers::map]
pub fn map_cowpool_binds(block: Block) -> Result<CowPoolBinds> {
    Ok(extract_cowpool_bindings(block))
}

fn extract_cowpool_bindings(block: Block) -> CowPoolBinds {
    let mut binds = Vec::new();
    for tx in block.transactions() {
        for (log, call) in tx.logs_with_calls() {
            let Some(topic) = log
                .topics
                .first()
                .map(|topic| topic.to_hex())
            else {
                continue;
            };
            if topic != BIND_TOPIC && topic != UNBIND_TOPIC {
                continue;
            }

            // This map scans logs from every contract on chain, and anyone can emit a log
            // whose first topic collides with the BCoWPool bind/unbind LOG_CALL topics.
            // Genuine BCoWPool events are always emitted by the bind/unbind call itself, so
            // a log whose emitting call doesn't carry the matching selector, or whose input
            // doesn't decode, is a lookalike from an unrelated contract. Skip it: erroring
            // here would permanently halt the stream at this block.
            let selector = if topic == BIND_TOPIC { BIND_SELECTOR } else { UNBIND_SELECTOR };
            if call.call.input.len() <= 4 || hex::encode(&call.call.input[..4]) != selector {
                substreams::log::info!(
                    "skipping {} lookalike log at {} in tx {}: emitting call does not match the selector",
                    selector,
                    log.address.to_hex(),
                    tx.hash.to_hex()
                );
                continue;
            }

            let decoded = if selector == BIND_SELECTOR {
                Bind::decode(call.call).map(|bind| {
                    (
                        bind.token,
                        bind.balance.to_signed_bytes_be(),
                        bind.denorm.to_signed_bytes_be(),
                        BindingChangeType::Bind,
                    )
                })
            } else {
                Unbind::decode(call.call)
                    .map(|unbind| (unbind.token, vec![], vec![], BindingChangeType::Unbind))
            };
            let (token, amount, weight, change_type) = match decoded {
                Ok(decoded) => decoded,
                Err(error) => {
                    substreams::log::info!(
                        "skipping undecodable {} call at {} in tx {}: {}",
                        selector,
                        log.address.to_hex(),
                        tx.hash.to_hex(),
                        error
                    );
                    continue;
                }
            };
            binds.push(CowPoolBind {
                address: log.address.clone(),
                token,
                amount,
                weight,
                tx: Some(tx.into()),
                ordinal: log.ordinal,
                change_type: change_type.into(),
            });
        }
    }

    CowPoolBinds { binds }
}

#[cfg(test)]
mod tests {
    use super::*;
    use substreams::scalar::BigInt;
    use substreams_ethereum::pb::eth::v2::{Call, Log, TransactionReceipt, TransactionTrace};

    fn bind_log(address: &[u8], ordinal: u64) -> Log {
        Log {
            address: address.to_vec(),
            topics: vec![hex::decode(&BIND_TOPIC[2..]).unwrap()],
            ordinal,
            ..Default::default()
        }
    }

    fn block_with_calls(calls: Vec<Call>) -> Block {
        let logs = calls
            .iter()
            .flat_map(|call| call.logs.clone())
            .collect();
        Block {
            transaction_traces: vec![TransactionTrace {
                status: 1,
                hash: vec![1; 32],
                calls,
                receipt: Some(TransactionReceipt { logs, ..Default::default() }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn maps_each_binding_log_to_the_call_that_emitted_it() {
        let pool = hex::decode("9bd702e05b9c97e4a4a3e47df1e0fe7a0c26d2f1").unwrap();
        let token_a = hex::decode("def1ca1fb7fbcdc777520aa7f396b4e015f497ab").unwrap();
        let token_b = hex::decode("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0").unwrap();
        let unbind_topic = hex::decode(&UNBIND_TOPIC[2..]).unwrap();
        let log_unbind = Log {
            address: pool.clone(),
            topics: vec![unbind_topic],
            ordinal: 3,
            ..Default::default()
        };
        let call = |token: Vec<u8>, log: Log| Call {
            address: pool.clone(),
            input: Bind { token, balance: BigInt::from(10), denorm: BigInt::from(1) }.encode(),
            logs: vec![log],
            ..Default::default()
        };
        let block = block_with_calls(vec![
            call(token_a.clone(), bind_log(&pool, 1)),
            call(token_b.clone(), bind_log(&pool, 2)),
            Call {
                address: pool.clone(),
                input: Unbind { token: token_a.clone() }.encode(),
                logs: vec![log_unbind],
                ..Default::default()
            },
        ]);

        let changes = extract_cowpool_bindings(block);

        assert_eq!(changes.binds.len(), 3);
        assert_eq!(changes.binds[0].token, token_a);
        assert_eq!(changes.binds[1].token, token_b);
        assert_eq!(changes.binds[2].token, changes.binds[0].token);
        assert_eq!(
            BindingChangeType::from_i32(changes.binds[2].change_type),
            Some(BindingChangeType::Unbind)
        );
    }

    #[test]
    fn skips_lookalike_logs_not_emitted_by_a_binding_call() {
        let pool = hex::decode("9bd702e05b9c97e4a4a3e47df1e0fe7a0c26d2f1").unwrap();
        let lookalike = hex::decode("00000000000000000000000000000000000000ff").unwrap();
        let token = hex::decode("def1ca1fb7fbcdc777520aa7f396b4e015f497ab").unwrap();
        let block = block_with_calls(vec![
            // carries the bind topic but comes from a call with a different selector
            Call {
                address: lookalike.clone(),
                input: hex::decode("deadbeef00").unwrap(),
                logs: vec![bind_log(&lookalike, 1)],
                ..Default::default()
            },
            Call {
                address: pool.clone(),
                input: Bind {
                    token: token.clone(),
                    balance: BigInt::from(10),
                    denorm: BigInt::from(1),
                }
                .encode(),
                logs: vec![bind_log(&pool, 2)],
                ..Default::default()
            },
        ]);

        let changes = extract_cowpool_bindings(block);

        assert_eq!(changes.binds.len(), 1);
        assert_eq!(changes.binds[0].token, token);
        assert_eq!(changes.binds[0].address, pool);
    }

    #[test]
    fn skips_binding_calls_with_undecodable_input() {
        let lookalike = hex::decode("00000000000000000000000000000000000000ff").unwrap();
        // starts with the bind selector but is too short to decode as a bind call
        let block = block_with_calls(vec![Call {
            address: lookalike.clone(),
            input: hex::decode("e4e1e538ff").unwrap(),
            logs: vec![bind_log(&lookalike, 1)],
            ..Default::default()
        }]);

        let changes = extract_cowpool_bindings(block);

        assert!(changes.binds.is_empty());
    }
}
