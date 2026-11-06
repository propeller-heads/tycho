//! Block index over the addresses a block executed.
//!
//! `map_trades`, `map_fee_config_events` and `map_subsidy_events` only ever look at a handful of
//! addresses. Between
//! 0.02% of blocks (unichain) and 2.8% (ethereum) carry a router call, so the index lets the
//! server skip almost every block without running the decoders. It is chain-agnostic: a new
//! router changes the filter of the consumer, not this module, so an index that is already built
//! stays valid.
use std::collections::BTreeSet;

use anyhow::Result;
use substreams::pb::sf::substreams::index::v1::Keys;
use substreams_ethereum::pb::eth::v2::Block;

use super::{fee_config::is_fee_config_log, subsidy::is_subsidy_log};

/// Key set once per block that carries a log the fee-config decoders recognise.
const FEE_CONFIG_KEY: &str = "fee_cfg";
/// Key set once per block that carries a SubsidyPayer event.
const SUBSIDY_KEY: &str = "subsidy";

/// Emits `addr:0x<address>` for every call frame of the block, reverted calls included,
/// `fee_cfg` for a block that carries a fee-config event, and `subsidy` for one that carries a
/// SubsidyPayer event.
///
/// All are supersets of what the consumers select on. `map_trades` matches a call whose target
/// is a router. `map_fee_config_events` decodes a fee-config log from any emitter, so the key it
/// filters on comes from those same decoders rather than from a list of addresses; nearly half
/// of the fee-config events stored so far come from an emitter no manifest configures.
/// `map_subsidy_events` reads one payer, but a chain without one has no key to filter on, so it
/// too filters on a key its own decoders set: on such a chain the module never runs.
#[substreams::handlers::map]
pub fn index_router_activity(block: Block) -> Result<Keys> {
    Ok(Keys { keys: router_activity_keys(&block) })
}

fn router_activity_keys(block: &Block) -> Vec<String> {
    let mut keys = BTreeSet::new();
    for tx in &block.transaction_traces {
        for call in &tx.calls {
            keys.insert(format!("addr:0x{}", hex::encode(&call.address)));
        }
        for log in tx
            .receipt
            .as_ref()
            .map(|r| r.logs.as_slice())
            .unwrap_or_default()
        {
            if is_fee_config_log(log) {
                keys.insert(FEE_CONFIG_KEY.to_string());
            } else if is_subsidy_log(log) {
                keys.insert(SUBSIDY_KEY.to_string());
            }
        }
    }
    keys.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::{Call, Log, TransactionReceipt, TransactionTrace};

    use super::*;

    /// keccak256("PositiveSlippageToggled(bool)"), one of the events the decoders recognise.
    const POSITIVE_SLIPPAGE_TOGGLED: &str =
        "42c6edc66d79ee64faff07b53737a48815c91942bb566b13c5f3f1856299226f";

    fn block(calls: &[[u8; 20]]) -> Block {
        Block {
            transaction_traces: vec![TransactionTrace {
                calls: calls
                    .iter()
                    .map(|address| Call { address: address.to_vec(), ..Default::default() })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn emits_one_key_per_call_target() {
        assert_eq!(
            router_activity_keys(&block(&[[0xaa; 20], [0xbb; 20]])),
            [format!("addr:0x{}", "aa".repeat(20)), format!("addr:0x{}", "bb".repeat(20))]
        );
    }

    #[test]
    fn repeated_target_gives_one_key() {
        assert_eq!(
            router_activity_keys(&block(&[[0xaa; 20], [0xaa; 20]])),
            [format!("addr:0x{}", "aa".repeat(20))]
        );
    }

    #[test]
    fn empty_block_has_no_keys() {
        assert!(router_activity_keys(&Block::default()).is_empty());
    }

    fn block_with_log(topic: Vec<u8>, data: Vec<u8>) -> Block {
        Block {
            transaction_traces: vec![TransactionTrace {
                receipt: Some(TransactionReceipt {
                    logs: vec![Log { topics: vec![topic], data, ..Default::default() }],
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn fee_config_log_sets_the_fee_key() {
        let topic = hex::decode(POSITIVE_SLIPPAGE_TOGGLED).unwrap();
        let mut data = vec![0u8; 32];
        data[31] = 1;
        assert_eq!(router_activity_keys(&block_with_log(topic, data)), [FEE_CONFIG_KEY]);
    }

    #[test]
    fn subsidy_log_sets_the_subsidy_key() {
        // keccak256("SubsidySkipped(uint256,uint8)") and the indexed nonce, from any emitter.
        let topic = hex::decode("b1e3823a20c3731a75cfe9552adcc003c6cf790cf9e9eada2e7f5edcaead934e")
            .unwrap();
        let mut data = vec![0u8; 32];
        data[31] = 7;
        let mut block = block_with_log(topic, data);
        let receipt = block.transaction_traces[0]
            .receipt
            .as_mut()
            .unwrap();
        receipt.logs[0]
            .topics
            .push(vec![0x11; 32]);
        assert_eq!(router_activity_keys(&block), [SUBSIDY_KEY]);
    }

    #[test]
    fn unrelated_log_sets_no_key() {
        assert!(router_activity_keys(&block_with_log(vec![0xcc; 32], Vec::new())).is_empty());
    }
}
