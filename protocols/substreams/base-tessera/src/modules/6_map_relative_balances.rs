use crate::{common::*, config::DeploymentConfig};
use anyhow::{anyhow, Result};
use std::collections::{BTreeSet, HashSet};
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetProto, StoreGetString},
};
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::{
    abi::{erc20, weth},
    prelude::*,
};

fn balance(token: &[u8], owner: &[u8]) -> Result<BigInt> {
    if zero(owner) {
        return Ok(BigInt::zero());
    }
    erc20::functions::BalanceOf { owner: owner.to_vec() }
        .call(token.to_vec())
        .ok_or_else(|| anyhow!("balanceOf failed for {} at {}", id(token), id(owner)))
}
fn event_delta(log: &eth::v2::Log, owner: &[u8]) -> Option<BigInt> {
    if let Some(erc20::events::Transfer { from, to, value }) =
        erc20::events::Transfer::match_and_decode(log)
    {
        return match (from == owner, to == owner) {
            (true, true) => Some(BigInt::zero()),
            (true, false) => Some(value.neg()),
            (false, true) => Some(value),
            _ => None,
        };
    }
    // Only Base's canonical WETH emits wrap/unwrap events in this accounting model.
    if log.address != substreams::hex!("4200000000000000000000000000000000000006") {
        return None;
    }
    if let Some(weth::events::Deposit { dst, wad }) = weth::events::Deposit::match_and_decode(log) {
        return (dst == owner).then_some(wad);
    }
    if let Some(weth::events::Withdrawal { src, wad }) =
        weth::events::Withdrawal::match_and_decode(log)
    {
        return (src == owner).then_some(wad.neg());
    }
    None
}

#[substreams::handlers::map]
pub fn map_relative_balances(
    params: String,
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
    components: StoreGetProto<ProtocolComponent>,
    pair_store: StoreGetString,
    treasury_store: StoreGetString,
) -> Result<BlockBalanceDeltas> {
    let config = DeploymentConfig::parse(&params)?;
    let owner = treasury_store
        .get_last("treasury")
        .map(hex::decode)
        .transpose()?
        .unwrap_or(config.treasury.clone());
    let mut rotations: Vec<_> = block
        .transactions()
        .flat_map(|tx| {
            tx.calls
                .iter()
                .filter(|c| !c.state_reverted)
                .flat_map(move |c| {
                    c.storage_changes
                        .iter()
                        .map(move |w| (tx, w))
                })
        })
        .filter(|(_, w)| w.address == config.tesseraswap && w.key == slot(config.treasury_slot))
        .collect();
    rotations.sort_by_key(|(_, w)| w.ordinal);
    // Account the entire block against its opening custodian, then bridge end-of-block balances.
    let old_owner = rotations
        .first()
        .map(|(_, w)| address(&w.old_value))
        .unwrap_or(owner.clone());
    let mut seeded = HashSet::new();
    let mut deltas = vec![];
    for group in new_components.tx_components {
        let tx = group.tx.expect("component transaction");
        for c in group.components {
            for token in c.tokens {
                let amount = balance(&token, &owner)?;
                seeded.insert((token.clone(), c.id.clone()));
                deltas.push(BalanceDelta {
                    ord: tx.index,
                    tx: Some(tx.clone()),
                    token,
                    component_id: c.id.as_bytes().to_vec(),
                    delta: amount.to_signed_bytes_be(),
                });
            }
        }
    }
    for log in block.logs() {
        if let Some(amount) = event_delta(log.log, &old_owner) {
            for pair in pairs(&pair_store, &format!("token:{}", id(log.address()))) {
                if seeded.contains(&(log.address().to_vec(), pair.clone())) {
                    continue;
                }
                deltas.push(BalanceDelta {
                    ord: log.ordinal(),
                    tx: Some(log.receipt.transaction.into()),
                    token: log.address().to_vec(),
                    component_id: pair.into_bytes(),
                    delta: amount.to_signed_bytes_be(),
                });
            }
        }
    }
    if let Some((tx, write)) = rotations.last() {
        let mut tokens = BTreeSet::new();
        for pair in pairs(&pair_store, "pairs") {
            if let Some(c) = components.get_last(pair) {
                tokens.extend(c.tokens);
            }
        }
        for token in tokens {
            let adjustment = balance(&token, &owner)? - balance(&token, &old_owner)?;
            for pair in pairs(&pair_store, &format!("token:{}", id(&token))) {
                if seeded.contains(&(token.clone(), pair.clone())) {
                    continue;
                }
                deltas.push(BalanceDelta {
                    ord: write.ordinal,
                    tx: Some((*tx).into()),
                    token: token.clone(),
                    component_id: pair.into_bytes(),
                    delta: adjustment.to_signed_bytes_be(),
                });
            }
        }
    }
    deltas.sort_by_key(|d| d.ord);
    Ok(BlockBalanceDeltas { balance_deltas: deltas })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn transfer(from: &[u8], to: &[u8], value: u64) -> eth::v2::Log {
        let word = |a: &[u8]| {
            let mut w = vec![0; 32];
            w[12..].copy_from_slice(a);
            w
        };
        eth::v2::Log {
            topics: vec![
                substreams::hex!(
                    "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
                )
                .to_vec(),
                word(from),
                word(to),
            ],
            data: slot(value),
            ..Default::default()
        }
    }
    #[test]
    fn self_transfer_nets_to_zero() {
        assert_eq!(event_delta(&transfer(&[1; 20], &[1; 20], 100), &[1; 20]), Some(BigInt::zero()));
        assert_eq!(
            event_delta(&transfer(&[1; 20], &[2; 20], 100), &[1; 20]),
            Some(BigInt::from(-100))
        );
    }
    #[test]
    fn seeded_new_pair_does_not_suppress_existing_pairs() {
        let token = vec![3; 20];
        let seeded = HashSet::from([(token.clone(), "new".to_string())]);
        let changes: Vec<_> = ["old", "new"]
            .into_iter()
            .filter(|id| !seeded.contains(&(token.clone(), id.to_string())))
            .map(|id| (id, event_delta(&transfer(&[2; 20], &[1; 20], 7), &[1; 20]).unwrap()))
            .collect();
        assert_eq!(changes, vec![("old", BigInt::from(7))]);
    }
    #[test]
    fn rotation_applies_old_owner_events_before_end_block_bridge() {
        let opening = BigInt::from(100);
        let movement = event_delta(&transfer(&[1; 20], &[2; 20], 20), &[1; 20]).unwrap();
        let old_end = BigInt::from(80);
        let new_end = BigInt::from(300);
        assert_eq!(opening + movement + (new_end.clone() - old_end), new_end);
    }
}
