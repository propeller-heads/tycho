use std::collections::HashSet;

use anyhow::Result;
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetProto, StoreGetString},
};
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::{
    abi::{erc20, weth},
    prelude::*,
};

use crate::{
    common::{address_from_word, all_books, books_for_token, is_zero, slot_key},
    config::DeploymentConfig,
};

/// Emits the treasury's inventory balance deltas as per-book TVL.
///
/// One treasury backs every book, so per-book reserves do not exist; the venue's inventory is
/// attributed per book instead (USDC — the shared quote inventory — is duplicated under every
/// book, not split: downstream consumers do not dedupe and under-reporting risks min-TVL
/// filtering). Three sources are combined so the accumulated balance tracks the treasury's
/// true holdings:
///
/// * **Seeding a new book** — `balanceOf(treasury)` is snapshotted via eth_call for the book's base
///   token (emitted under the new book) and for USDC (emitted under the new book only, giving it
///   the same USDC baseline the older books accumulated). The snapshotted tokens' plain event
///   deltas are skipped this block to avoid double-counting.
/// * **Re-seeding on a treasury rotation** — when TesseraSwap's treasury slot is written, every
///   tracked token is re-seeded by `balanceOf(new) - balanceOf(old)` fanned to its book(s). On Base
///   the treasury rotated once, at block 37,737,344.
/// * **Existing tokens** — ERC20 `Transfer`s touching the treasury, plus WETH
///   `Deposit`/`Withdrawal` on the treasury (which change its WETH balance without a `Transfer`
///   log).
#[substreams::handlers::map]
pub fn map_relative_balances(
    params: String,
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
    components_store: StoreGetProto<ProtocolComponent>,
    books_store: StoreGetString,
    treasury_store: StoreGetString,
) -> Result<BlockBalanceDeltas> {
    let config: DeploymentConfig = serde_qs::from_str(&params)?;
    let Some(treasury_hex) = treasury_store.get_last("treasury") else {
        return Ok(BlockBalanceDeltas::default());
    };
    let treasury = hex::decode(treasury_hex)?;

    let mut balance_deltas = Vec::new();
    // Tokens already snapshotted this block; their plain event deltas are skipped to avoid
    // double-counting the snapshot.
    let mut seeded_tokens: HashSet<Vec<u8>> = HashSet::new();

    seed_new_books(&new_components, &treasury, &mut balance_deltas);
    for delta in &balance_deltas {
        seeded_tokens.insert(delta.token.clone());
    }

    reseed_on_treasury_write(
        &block,
        &config,
        &components_store,
        &books_store,
        &mut balance_deltas,
        &mut seeded_tokens,
    );

    apply_event_deltas(
        &block,
        &treasury,
        &config,
        &components_store,
        &books_store,
        &seeded_tokens,
        &mut balance_deltas,
    );

    balance_deltas.sort_unstable_by_key(|delta| delta.ord);
    Ok(BlockBalanceDeltas { balance_deltas })
}

/// Snapshots `balanceOf(treasury)` for every book created this block: its base token, and its
/// own USDC baseline.
///
/// USDC is seeded under the new book only — the other books already carry their (duplicated)
/// USDC balance, and the new book must start from the same absolute value rather than from 0.
fn seed_new_books(
    new_components: &BlockTransactionProtocolComponents,
    treasury: &[u8],
    balance_deltas: &mut Vec<BalanceDelta>,
) {
    for tx_components in &new_components.tx_components {
        let Some(tx) = &tx_components.tx else { continue };
        for component in &tx_components.components {
            for token in &component.tokens {
                let balance = erc20::functions::BalanceOf { owner: treasury.to_vec() }
                    .call(token.clone())
                    .unwrap_or_else(BigInt::zero);
                balance_deltas.push(BalanceDelta {
                    ord: tx.index,
                    tx: Some(tx.clone()),
                    token: token.clone(),
                    delta: balance.to_signed_bytes_be(),
                    component_id: component.id.clone().into_bytes(),
                });
            }
        }
    }
}

/// Re-seeds every tracked token on a write to TesseraSwap's treasury slot.
///
/// For each currently-tracked token (USDC plus every book's base) emits
/// `balanceOf(new) - balanceOf(old)` fanned to the appropriate components, and records the
/// token as seeded so its plain `Transfer` deltas are skipped this block.
fn reseed_on_treasury_write(
    block: &eth::v2::Block,
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    books_store: &StoreGetString,
    balance_deltas: &mut Vec<BalanceDelta>,
    seeded_tokens: &mut HashSet<Vec<u8>>,
) {
    let treasury_slot = slot_key(config.treasury_slot);
    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.storage_changes {
                if change.address != config.tesseraswap || change.key != treasury_slot {
                    continue;
                }
                let new_treasury = address_from_word(&change.new_value);
                if is_zero(&new_treasury) {
                    continue;
                }
                let old_treasury = address_from_word(&change.old_value);
                let transaction: Transaction = tx.into();
                for token in tracked_tokens(config, components_store, books_store) {
                    // A token already snapshotted by `seed_new_books` this block (a book
                    // created in the same block the treasury rotates) must not also be
                    // re-seeded here, or its balance would be double-counted.
                    if seeded_tokens.contains(&token) {
                        continue;
                    }
                    let new_balance = erc20::functions::BalanceOf { owner: new_treasury.clone() }
                        .call(token.clone())
                        .unwrap_or_else(BigInt::zero);
                    let old_balance = if is_zero(&old_treasury) {
                        BigInt::zero()
                    } else {
                        erc20::functions::BalanceOf { owner: old_treasury.clone() }
                            .call(token.clone())
                            .unwrap_or_else(BigInt::zero)
                    };
                    let delta = new_balance - old_balance;
                    for comp_id in
                        books_for_token(&token, &config.usdc, components_store, books_store)
                    {
                        balance_deltas.push(BalanceDelta {
                            ord: change.ordinal,
                            tx: Some(transaction.clone()),
                            token: token.clone(),
                            delta: delta.to_signed_bytes_be(),
                            component_id: comp_id.into_bytes(),
                        });
                    }
                    seeded_tokens.insert(token);
                }
            }
        }
    }
}

/// Applies `Transfer` and WETH `Deposit`/`Withdrawal` deltas for tokens that were not seeded
/// this block.
fn apply_event_deltas(
    block: &eth::v2::Block,
    treasury: &[u8],
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    books_store: &StoreGetString,
    seeded_tokens: &HashSet<Vec<u8>>,
    balance_deltas: &mut Vec<BalanceDelta>,
) {
    for log in block.logs() {
        let token = log.address().to_vec();
        if seeded_tokens.contains(&token) {
            continue;
        }
        let Some(delta) = event_delta(log.log, treasury) else { continue };
        let books = books_for_token(&token, &config.usdc, components_store, books_store);
        if books.is_empty() {
            continue;
        }
        for comp_id in books {
            balance_deltas.push(BalanceDelta {
                ord: log.ordinal(),
                tx: Some(log.receipt.transaction.into()),
                token: token.clone(),
                delta: delta.to_signed_bytes_be(),
                component_id: comp_id.into_bytes(),
            });
        }
    }
}

/// The signed balance delta a single log applies to the treasury, or `None` if the log does
/// not move the treasury's balance.
///
/// Handles ERC20 `Transfer` (in/out of the treasury) and WETH `Deposit`/`Withdrawal` (the
/// treasury wrapping/unwrapping ETH, which changes its WETH balance without a `Transfer` log).
fn event_delta(log: &eth::v2::Log, treasury: &[u8]) -> Option<BigInt> {
    if let Some(erc20::events::Transfer { from, to, value }) =
        erc20::events::Transfer::match_and_decode(log)
    {
        if to == treasury {
            return Some(value);
        }
        if from == treasury {
            return Some(value.neg());
        }
        return None;
    }
    if let Some(weth::events::Deposit { dst, wad }) = weth::events::Deposit::match_and_decode(log) {
        return (dst == treasury).then_some(wad);
    }
    if let Some(weth::events::Withdrawal { src, wad }) =
        weth::events::Withdrawal::match_and_decode(log)
    {
        return (src == treasury).then_some(wad.neg());
    }
    None
}

/// All currently-tracked tokens: USDC plus every known book's base side.
fn tracked_tokens(
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    books_store: &StoreGetString,
) -> Vec<Vec<u8>> {
    let mut tokens = vec![config.usdc.clone()];
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    seen.insert(config.usdc.clone());
    for component in all_books(components_store, books_store) {
        for token in component.tokens {
            if seen.insert(token.clone()) {
                tokens.push(token);
            }
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use substreams::hex;

    use super::*;

    const TRANSFER_TOPIC: [u8; 32] =
        hex!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
    const DEPOSIT_TOPIC: [u8; 32] =
        hex!("e1fffcc4923d04b559f4d29a8bfc6cda04eb5b0d3c460751c2402c5c5cc9109c");
    const WITHDRAWAL_TOPIC: [u8; 32] =
        hex!("7fcf532c15f0a6db0bd6d0e038bea71d30d808c7d98cb3bf7268a95bf5081b65");

    fn treasury() -> Vec<u8> {
        hex::decode("3dbe077e7986657e95e1cc50089f17a5a4af0aae").unwrap()
    }

    fn other() -> Vec<u8> {
        hex::decode("67d03631fe51b741c0c00c4e16eb662ac84381df").unwrap()
    }

    fn topic_word(address: &[u8]) -> Vec<u8> {
        let mut word = vec![0u8; 32];
        word[12..].copy_from_slice(address);
        word
    }

    fn amount_word(value: u64) -> Vec<u8> {
        let mut word = vec![0u8; 32];
        word[24..].copy_from_slice(&value.to_be_bytes());
        word
    }

    fn transfer_log(from: &[u8], to: &[u8], value: u64) -> eth::v2::Log {
        eth::v2::Log {
            topics: vec![TRANSFER_TOPIC.to_vec(), topic_word(from), topic_word(to)],
            data: amount_word(value),
            ..Default::default()
        }
    }

    fn deposit_log(dst: &[u8], wad: u64) -> eth::v2::Log {
        eth::v2::Log {
            topics: vec![DEPOSIT_TOPIC.to_vec(), topic_word(dst)],
            data: amount_word(wad),
            ..Default::default()
        }
    }

    fn withdrawal_log(src: &[u8], wad: u64) -> eth::v2::Log {
        eth::v2::Log {
            topics: vec![WITHDRAWAL_TOPIC.to_vec(), topic_word(src)],
            data: amount_word(wad),
            ..Default::default()
        }
    }

    #[test]
    fn transfer_to_treasury_is_positive() {
        assert_eq!(
            event_delta(&transfer_log(&other(), &treasury(), 50), &treasury()),
            Some(50.into())
        );
    }

    #[test]
    fn transfer_from_treasury_is_negative() {
        assert_eq!(
            event_delta(&transfer_log(&treasury(), &other(), 50), &treasury()),
            Some(BigInt::from(50).neg())
        );
    }

    #[test]
    fn transfer_not_touching_treasury_is_ignored() {
        assert_eq!(event_delta(&transfer_log(&other(), &other(), 50), &treasury()), None);
    }

    #[test]
    fn weth_deposit_to_treasury_increases_balance() {
        // WETH `deposit()` (wrap) credits the treasury without a `Transfer` log.
        assert_eq!(event_delta(&deposit_log(&treasury(), 70), &treasury()), Some(70.into()));
    }

    #[test]
    fn weth_withdrawal_from_treasury_decreases_balance() {
        assert_eq!(
            event_delta(&withdrawal_log(&treasury(), 70), &treasury()),
            Some(BigInt::from(70).neg())
        );
    }

    #[test]
    fn weth_events_for_other_wallets_are_ignored() {
        assert_eq!(event_delta(&deposit_log(&other(), 70), &treasury()), None);
        assert_eq!(event_delta(&withdrawal_log(&other(), 70), &treasury()), None);
    }
}
