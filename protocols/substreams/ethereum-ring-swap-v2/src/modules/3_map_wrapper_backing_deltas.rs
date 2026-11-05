use std::collections::HashMap;

use substreams::{
    pb::substreams::StoreDeltas,
    prelude::BigInt,
    store::{StoreAddBigInt, StoreGet, StoreGetRaw, StoreNew},
};
use substreams_ethereum::{pb::eth::v2 as eth, Event};

use crate::store_key::StoreKey;
use tycho_substreams::{abi::erc20, prelude::*};

/// Tracks underlying ERC-20 balances held by official FewToken wrappers.
///
/// Ring pairs hold FewTokens, while the executor unwraps the output FewToken into its underlying
/// ERC-20. The pair reserve therefore is not sufficient liquidity information: each wrapper's
/// underlying backing is a separate, dynamic bound on executable output.
#[substreams::handlers::map]
pub fn map_wrapper_backing_deltas(
    block: eth::Block,
    wrapper_store_deltas: StoreDeltas,
    wrapper_store: StoreGetRaw,
) -> Result<BlockBalanceDeltas, substreams::errors::Error> {
    let mut balance_deltas = Vec::new();
    let new_wrappers = newly_tracked_wrappers(wrapper_store_deltas)?;

    for raw_tx in block.transactions() {
        let transaction: Transaction = raw_tx.into();
        for (log, _) in raw_tx.logs_with_calls() {
            let token = log.address.clone();

            // A new wrapper is snapshotted at end-of-block below. Applying this block's events as
            // well would count the same backing movement twice.
            if new_wrappers.contains_key(&token) {
                continue;
            }

            let token_key = StoreKey::FewWrapper.get_unique_key(&hex::encode(&token));
            let Some(wrapper) = wrapper_store.get_last(&token_key) else {
                continue;
            };
            let Some(delta) = event_delta(log, &wrapper) else {
                continue;
            };

            balance_deltas.push(BalanceDelta {
                ord: log.ordinal,
                tx: Some(transaction.clone()),
                token,
                delta: delta.to_signed_bytes_be(),
                component_id: hex::encode(wrapper).into_bytes(),
            });
        }
    }

    if let Some(last_tx) = block
        .transaction_traces
        .last()
        .map(Transaction::from)
    {
        for (underlying, wrapper) in &new_wrappers {
            let balance = snapshot_backing_or_zero(
                erc20::functions::BalanceOf { owner: wrapper.clone() }.call(underlying.clone()),
            );
            balance_deltas.push(BalanceDelta {
                ord: u64::MAX,
                tx: Some(last_tx.clone()),
                token: underlying.clone(),
                delta: balance.to_signed_bytes_be(),
                component_id: hex::encode(wrapper).into_bytes(),
            });
        }
    }

    Ok(BlockBalanceDeltas { balance_deltas })
}

/// Returns zero backing when an underlying token cannot be queried.
fn snapshot_backing_or_zero(balance: Option<BigInt>) -> BigInt {
    balance.unwrap_or_default()
}

/// Returns the signed balance change a log applies to a wrapper's underlying balance.
fn event_delta(log: &eth::Log, wrapper: &[u8]) -> Option<BigInt> {
    if let Some(erc20::events::Transfer { from, to, value }) =
        erc20::events::Transfer::match_and_decode(log)
    {
        let mut delta = BigInt::zero();
        if from.as_slice() == wrapper {
            delta = delta - value.clone();
        }
        if to.as_slice() == wrapper {
            delta = delta + value;
        }
        return (delta != BigInt::zero()).then_some(delta);
    }
    None
}

fn newly_tracked_wrappers(
    wrapper_store_deltas: StoreDeltas,
) -> Result<HashMap<Vec<u8>, Vec<u8>>, substreams::errors::Error> {
    let prefix = format!("{}:", StoreKey::FewWrapper.unique_id());
    wrapper_store_deltas
        .deltas
        .into_iter()
        .filter(|delta| delta.old_value.is_empty())
        .map(|delta| {
            let underlying_key = delta
                .key
                .strip_prefix(&prefix)
                .ok_or_else(|| anyhow::anyhow!("Unexpected FewWrapper store key: {}", delta.key))?;
            Ok((hex::decode(underlying_key)?, delta.new_value))
        })
        .collect()
}

#[substreams::handlers::store]
pub fn store_wrapper_backings(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    tycho_substreams::balances::store_balance_changes(deltas, store);
}

#[cfg(test)]
mod tests {
    use substreams::hex;

    use super::*;

    const TRANSFER_TOPIC: [u8; 32] =
        hex!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

    fn wrapper() -> Vec<u8> {
        vec![1; 20]
    }

    fn other() -> Vec<u8> {
        vec![2; 20]
    }

    fn topic_word(address: &[u8]) -> Vec<u8> {
        let mut word = vec![0; 32];
        word[12..].copy_from_slice(address);
        word
    }

    fn amount_word(value: u64) -> Vec<u8> {
        let mut word = vec![0; 32];
        word[24..].copy_from_slice(&value.to_be_bytes());
        word
    }

    fn transfer_log(from: &[u8], to: &[u8], value: u64) -> eth::Log {
        eth::Log {
            topics: vec![TRANSFER_TOPIC.to_vec(), topic_word(from), topic_word(to)],
            data: amount_word(value),
            ..Default::default()
        }
    }

    #[test]
    fn transfer_events_track_wrapper_balance_in_both_directions() {
        assert_eq!(
            event_delta(&transfer_log(&other(), &wrapper(), 50), &wrapper()),
            Some(BigInt::from(50))
        );
        assert_eq!(
            event_delta(&transfer_log(&wrapper(), &other(), 50), &wrapper()),
            Some(BigInt::from(-50))
        );
    }

    #[test]
    fn backing_snapshot_fails_closed_when_balance_query_fails() {
        assert_eq!(snapshot_backing_or_zero(Some(BigInt::from(50))), BigInt::from(50));
        assert_eq!(snapshot_backing_or_zero(None), BigInt::zero());
    }

    #[test]
    fn self_transfer_does_not_change_backing() {
        assert_eq!(event_delta(&transfer_log(&wrapper(), &wrapper(), 50), &wrapper()), None);
    }

    #[test]
    fn unrelated_events_are_ignored() {
        assert_eq!(event_delta(&transfer_log(&other(), &other(), 50), &wrapper()), None);
    }
}
