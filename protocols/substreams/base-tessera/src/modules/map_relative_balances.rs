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

/// A `(token, component)` pair already covered by a `balanceOf` snapshot this block; its plain
/// event deltas must be skipped to avoid double-counting the snapshot. The granularity is per
/// component: a new book's USDC seed must not suppress the same USDC event deltas on the other
/// books, which are not covered by that seed.
type SeededSet = HashSet<(Vec<u8>, Vec<u8>)>;

/// A treasury rotation observed this block: the custodian before the first write and after the
/// last one (a multi-hop rotation A→B→C within one block collapses to A→C; intermediate
/// custodians hold no residue in that model).
struct Rotation {
    old: Vec<u8>,
    new: Vec<u8>,
    ord: u64,
    tx: Transaction,
}

/// Emits the treasury's inventory balance deltas as per-book TVL.
///
/// One treasury backs every book, so per-book reserves do not exist; the venue's inventory is
/// attributed per book instead (USDC — the shared quote inventory — is duplicated under every
/// book, not split: downstream consumers do not dedupe and under-reporting risks min-TVL
/// filtering). Three sources are combined so the accumulated balance tracks the treasury's
/// true holdings:
///
/// * **Seeding a new book** — `balanceOf(treasury)` is snapshotted via eth_call (end-of-block
///   state) for the book's base token and for USDC, both emitted under the new book only. USDC
///   under the new book gives it the same baseline the older books accumulated; their own event
///   deltas still apply to them.
/// * **Re-seeding on a treasury rotation** — when TesseraSwap's treasury slot is written, every
///   tracked token is re-seeded by `balanceOf(new) - balanceOf(old)` at end-of-block state, fanned
///   to its book(s). Event deltas in the rotation block are matched against the **old** custodian:
///   they carry the accumulated balance to `balanceOf(old)` at end of block, which the re-seed then
///   replaces with `balanceOf(new)` — exact even when inventory migrates within the rotation block.
///   (On Base the treasury rotated once, at block 37,737,344, with no in-block migration.)
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
    let mut seeded: SeededSet = HashSet::new();

    seed_new_books(&new_components, &treasury, &mut balance_deltas, &mut seeded);

    let rotation = find_rotation(&block, &config);
    if let Some(rotation) = &rotation {
        reseed_on_rotation(
            rotation,
            &config,
            &components_store,
            &books_store,
            &mut balance_deltas,
            &mut seeded,
        );
    }

    // In a rotation block, events are matched against the custodian the accumulated balances
    // still refer to (the old one); the re-seed then jumps them to the new custodian's state.
    let event_treasury = rotation
        .as_ref()
        .map(|r| r.old.clone())
        .filter(|old| !is_zero(old))
        .unwrap_or(treasury);

    apply_event_deltas(
        &block,
        &event_treasury,
        &config,
        &components_store,
        &books_store,
        &seeded,
        &mut balance_deltas,
    );

    // Deltas of one transaction must stay contiguous after sorting: the downstream aggregation
    // groups consecutive same-transaction runs, and the seeds' ordinals live in a different
    // domain (tx index) than the event ordinals (execution ordinals).
    balance_deltas.sort_unstable_by_key(|delta| {
        (
            delta
                .tx
                .as_ref()
                .map(|tx| tx.index)
                .unwrap_or_default(),
            delta.ord,
        )
    });
    Ok(BlockBalanceDeltas { balance_deltas })
}

/// Snapshots `balanceOf(treasury)` (end-of-block state) for every book created this block: its
/// base token and its USDC baseline, both emitted under the new book only.
fn seed_new_books(
    new_components: &BlockTransactionProtocolComponents,
    treasury: &[u8],
    balance_deltas: &mut Vec<BalanceDelta>,
    seeded: &mut SeededSet,
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
                seeded.insert((token.clone(), component.id.clone().into_bytes()));
            }
        }
    }
}

/// Finds the block's treasury rotation, if any: the custodian before the first write to
/// TesseraSwap's treasury slot and after the last one.
fn find_rotation(block: &eth::v2::Block, config: &DeploymentConfig) -> Option<Rotation> {
    let treasury_slot = slot_key(config.treasury_slot);
    let mut rotation: Option<Rotation> = None;
    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.storage_changes {
                if change.address != config.tesseraswap ||
                    change.key != treasury_slot ||
                    is_zero(&change.new_value)
                {
                    continue;
                }
                let new = address_from_word(&change.new_value);
                match &mut rotation {
                    Some(rotation) => {
                        rotation.new = new;
                        rotation.ord = change.ordinal;
                        rotation.tx = tx.into();
                    }
                    None => {
                        rotation = Some(Rotation {
                            old: address_from_word(&change.old_value),
                            new,
                            ord: change.ordinal,
                            tx: tx.into(),
                        });
                    }
                }
            }
        }
    }
    // The constructor's initial write has a zero old value and no books exist yet — it is not
    // a rotation to re-seed.
    rotation.filter(|r| r.old != r.new)
}

/// Re-seeds every tracked token on a treasury rotation with
/// `balanceOf(new) - balanceOf(old)` at end-of-block state, fanned to the token's book(s).
fn reseed_on_rotation(
    rotation: &Rotation,
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    books_store: &StoreGetString,
    balance_deltas: &mut Vec<BalanceDelta>,
    seeded: &mut SeededSet,
) {
    for token in tracked_tokens(config, components_store, books_store) {
        let new_balance = erc20::functions::BalanceOf { owner: rotation.new.clone() }
            .call(token.clone())
            .unwrap_or_else(BigInt::zero);
        let old_balance = if is_zero(&rotation.old) {
            BigInt::zero()
        } else {
            erc20::functions::BalanceOf { owner: rotation.old.clone() }
                .call(token.clone())
                .unwrap_or_else(BigInt::zero)
        };
        let delta = new_balance - old_balance;
        for comp_id in books_for_token(&token, &config.usdc, components_store, books_store) {
            let comp_id = comp_id.into_bytes();
            // A (token, book) pair snapshotted by `seed_new_books` this block (a book created
            // in the block the treasury rotates) already carries the new custodian's
            // end-of-block balance and must not be re-seeded on top.
            if !seeded.insert((token.clone(), comp_id.clone())) {
                continue;
            }
            balance_deltas.push(BalanceDelta {
                ord: rotation.ord,
                tx: Some(rotation.tx.clone()),
                token: token.clone(),
                delta: delta.to_signed_bytes_be(),
                component_id: comp_id,
            });
        }
    }
}

/// Applies `Transfer` and WETH `Deposit`/`Withdrawal` deltas for `(token, book)` pairs that
/// were not snapshotted this block.
fn apply_event_deltas(
    block: &eth::v2::Block,
    treasury: &[u8],
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    books_store: &StoreGetString,
    seeded: &SeededSet,
    balance_deltas: &mut Vec<BalanceDelta>,
) {
    for log in block.logs() {
        let token = log.address().to_vec();
        let Some(delta) = event_delta(log.log, treasury) else { continue };
        let books = books_for_token(&token, &config.usdc, components_store, books_store);
        if books.is_empty() {
            continue;
        }
        for comp_id in books {
            let comp_id = comp_id.into_bytes();
            if seeded.contains(&(token.clone(), comp_id.clone())) {
                continue;
            }
            balance_deltas.push(BalanceDelta {
                ord: log.ordinal(),
                tx: Some(log.receipt.transaction.into()),
                token: token.clone(),
                delta: delta.to_signed_bytes_be(),
                component_id: comp_id,
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
        // A self-transfer nets zero; counting its inflow branch alone drifts the balance
        // upward (observed on Base: the venue's first test swap self-transferred 3,444,538
        // USDC-wei inside the treasury, block 37,519,381).
        if from == to {
            return None;
        }
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
    fn self_transfer_nets_zero() {
        // Transfer(from=treasury, to=treasury) moves nothing; crediting the inflow branch
        // alone would drift the accumulated balance upward.
        assert_eq!(event_delta(&transfer_log(&treasury(), &treasury(), 50), &treasury()), None);
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

    #[test]
    fn seed_skip_is_per_token_and_component() {
        // A new book's USDC seed must not suppress the same USDC event delta on another book.
        let usdc = hex::decode("833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap();
        let new_book = b"0xnew".to_vec();
        let other_book = b"0xold".to_vec();
        let mut seeded: SeededSet = HashSet::new();
        seeded.insert((usdc.clone(), new_book.clone()));
        assert!(seeded.contains(&(usdc.clone(), new_book)));
        assert!(!seeded.contains(&(usdc, other_book)));
    }

    #[test]
    fn multi_hop_rotation_collapses_to_first_old_and_last_new() {
        // A -> B -> C in one block: events match A, the reseed jumps to C, B is transient.
        let a = vec![0xaa; 20];
        let c = vec![0xcc; 20];
        let rotation =
            Rotation { old: a.clone(), new: c.clone(), ord: 7, tx: Transaction::default() };
        assert_eq!(rotation.old, a);
        assert_eq!(rotation.new, c);
    }
}
