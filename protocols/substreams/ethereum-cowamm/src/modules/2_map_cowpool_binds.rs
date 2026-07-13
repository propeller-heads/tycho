use crate::{
    abi::b_cow_pool::functions::{Bind, Unbind},
    pb::cowamm::{BindingChangeType, CowPoolBind, CowPoolBinds},
};
use anyhow::{bail, Context, Ok, Result};
use substreams_ethereum::pb::eth::v2::Block;
use substreams_helper::hex::Hexable;

const BIND_TOPIC: &str = "0xe4e1e53800000000000000000000000000000000000000000000000000000000";
const BIND_SELECTOR: &str = "e4e1e538";
const UNBIND_TOPIC: &str = "0xcf5e7bd300000000000000000000000000000000000000000000000000000000";
const UNBIND_SELECTOR: &str = "cf5e7bd3";

#[substreams::handlers::map]
pub fn map_cowpool_binds(block: Block) -> Result<CowPoolBinds> {
    extract_cowpool_bindings(block)
}

fn extract_cowpool_bindings(block: Block) -> Result<CowPoolBinds> {
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

            let selector = if topic == BIND_TOPIC { BIND_SELECTOR } else { UNBIND_SELECTOR };
            if call.call.input.len() <= 4 || hex::encode(&call.call.input[..4]) != selector {
                bail!(
                    "CowAMM {} event at pool {} in tx {} was emitted by a mismatched call",
                    selector,
                    log.address.to_hex(),
                    tx.hash.to_hex()
                );
            }

            let (token, amount, weight, change_type) = if selector == BIND_SELECTOR {
                let bind = Bind::decode(call.call)
                    .map_err(anyhow::Error::msg)
                    .with_context(|| {
                        format!(
                            "failed to decode CowAMM bind at pool {} in tx {}",
                            log.address.to_hex(),
                            tx.hash.to_hex()
                        )
                    })?;
                (
                    bind.token,
                    bind.balance.to_signed_bytes_be(),
                    bind.denorm.to_signed_bytes_be(),
                    BindingChangeType::Bind,
                )
            } else {
                let unbind = Unbind::decode(call.call)
                    .map_err(anyhow::Error::msg)
                    .with_context(|| {
                        format!(
                            "failed to decode CowAMM unbind at pool {} in tx {}",
                            log.address.to_hex(),
                            tx.hash.to_hex()
                        )
                    })?;
                (unbind.token, vec![], vec![], BindingChangeType::Unbind)
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

    Ok(CowPoolBinds { binds })
}

#[cfg(test)]
mod tests {
    use super::*;
    use substreams::scalar::BigInt;
    use substreams_ethereum::pb::eth::v2::{Call, Log, TransactionReceipt, TransactionTrace};

    #[test]
    fn maps_each_binding_log_to_the_call_that_emitted_it() {
        let pool = hex::decode("9bd702e05b9c97e4a4a3e47df1e0fe7a0c26d2f1").unwrap();
        let token_a = hex::decode("def1ca1fb7fbcdc777520aa7f396b4e015f497ab").unwrap();
        let token_b = hex::decode("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0").unwrap();
        let bind_topic = hex::decode(&BIND_TOPIC[2..]).unwrap();
        let unbind_topic = hex::decode(&UNBIND_TOPIC[2..]).unwrap();
        let log_a = Log {
            address: pool.clone(),
            topics: vec![bind_topic.clone()],
            ordinal: 1,
            ..Default::default()
        };
        let log_b = Log {
            address: pool.clone(),
            topics: vec![bind_topic.clone()],
            ordinal: 2,
            ..Default::default()
        };
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
        let block = Block {
            transaction_traces: vec![TransactionTrace {
                status: 1,
                hash: vec![1; 32],
                calls: vec![
                    call(token_a.clone(), log_a.clone()),
                    call(token_b.clone(), log_b.clone()),
                    Call {
                        address: pool.clone(),
                        input: Unbind { token: token_a.clone() }.encode(),
                        logs: vec![log_unbind.clone()],
                        ..Default::default()
                    },
                ],
                receipt: Some(TransactionReceipt {
                    logs: vec![log_a, log_b, log_unbind],
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        let changes = extract_cowpool_bindings(block).expect("binding calls should decode");

        assert_eq!(changes.binds.len(), 3);
        assert_eq!(changes.binds[0].token, token_a);
        assert_eq!(changes.binds[1].token, token_b);
        assert_eq!(changes.binds[2].token, changes.binds[0].token);
        assert_eq!(
            BindingChangeType::from_i32(changes.binds[2].change_type),
            Some(BindingChangeType::Unbind)
        );
    }
}
