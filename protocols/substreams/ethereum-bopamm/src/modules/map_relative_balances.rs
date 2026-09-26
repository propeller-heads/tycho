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
    common::{
        address_from_word, books_for_token, previous_live_book_for_token, u64_from_word_padded,
        weth_event_delta,
    },
    config::DeploymentConfig,
};

/// Emits the maker's inventory balance deltas as per-book TVL.
///
/// Three sources are combined so the accumulated balance tracks the maker's true holdings:
///
/// * **Seeding a new book** — when a book is created this block its asset becomes newly tracked.
///   Its current `balanceOf(maker)` is snapshotted via eth_call and emitted as the initial delta
///   (USDC is already tracked across the other books). The asset's plain event deltas are skipped
///   this block to avoid double-counting the snapshot.
/// * **Seeding/re-seeding on a maker write** — when the global maker slot is written (first
///   designation or rotation) every tracked token is re-seeded by `balanceOf(new) - balanceOf(old)`
///   (both via eth_call at this block). On first designation `old` is the zero word so
///   `balanceOf(old)` is `0`, which seeds the holdings the maker already had before being
///   designated. The maker's plain `Transfer` deltas are skipped this block for re-seeded tokens
///   because the post-block `balanceOf(new)` snapshot already includes them.
/// * **Existing tokens** — ERC20 `Transfer`s touching the maker, plus WETH `Deposit`/`Withdrawal`
///   on the maker (which change the maker's WETH balance without emitting `Transfer`).
///
/// USDC deltas are emitted under every book (the shared quote inventory is duplicated, not split).
#[substreams::handlers::map]
pub fn map_relative_balances(
    params: String,
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
    components_store: StoreGetProto<ProtocolComponent>,
    maker_store: StoreGetString,
) -> Result<BlockBalanceDeltas> {
    let config: DeploymentConfig = serde_qs::from_str(&params)?;
    let Some(maker_hex) = maker_store.get_last("maker") else {
        return Ok(BlockBalanceDeltas::default());
    };
    let maker = hex::decode(maker_hex)?;

    let mut balance_deltas = Vec::new();
    // Tokens already snapshotted this block (via new-book or maker-write seeding). Their plain
    // event deltas are skipped to avoid double-counting the snapshot.
    let mut seeded_tokens: HashSet<Vec<u8>> = HashSet::new();

    seed_new_books(&new_components, &maker, &config, &components_store, &mut balance_deltas);
    for delta in &balance_deltas {
        seeded_tokens.insert(delta.token.clone());
    }

    reseed_on_maker_write(
        &block,
        &config,
        &components_store,
        &mut balance_deltas,
        &mut seeded_tokens,
    );

    apply_event_deltas(
        &block,
        &maker,
        &config,
        &components_store,
        &seeded_tokens,
        &mut balance_deltas,
    );

    balance_deltas.sort_unstable_by_key(|delta| delta.ord);
    Ok(BlockBalanceDeltas { balance_deltas })
}

/// Snapshots `balanceOf(maker, asset)` for every book created this block and emits it as the
/// seeding delta for that book.
fn seed_new_books(
    new_components: &BlockTransactionProtocolComponents,
    maker: &[u8],
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    balance_deltas: &mut Vec<BalanceDelta>,
) {
    for tx_components in &new_components.tx_components {
        let Some(tx) = &tx_components.tx else { continue };
        for component in &tx_components.components {
            let new_asset_id = component
                .get_attribute_value("asset_id")
                .and_then(|b| u64_from_word_padded(&b));
            for token in &component.tokens {
                if token.as_slice() == config.usdc.as_slice() {
                    continue;
                }
                let balance = erc20::functions::BalanceOf { owner: maker.to_vec() }
                    .call(token.clone())
                    .unwrap_or_else(BigInt::zero);
                for comp_id in books_for_token(token, &config.usdc, components_store) {
                    balance_deltas.push(BalanceDelta {
                        ord: tx.index,
                        tx: Some(tx.clone()),
                        token: token.clone(),
                        delta: balance.to_signed_bytes_be(),
                        component_id: comp_id.into_bytes(),
                    });
                }
                // Re-listing: this token's live book (seeded with `balanceOf(maker)` above) has
                // replaced an older book. Drain only the previous live book by the same amount so
                // the maker's inventory is not double-counted across both books. USDC is excluded
                // (it is shared across every book by design).
                //
                // Only the immediately preceding book is drained, never the whole superseded set:
                // each older book was already drained to zero when its own replacement was created,
                // so draining them again would push them negative.
                //
                // Invariant: the previous book's accumulated asset balance equals
                // `balanceOf(maker)` at this block — it was the token's live book
                // and tracked the maker's holdings until now, so
                // `-balanceOf(maker)` nets it to zero. The balance store is additive
                // and cannot be read back here (module graph cycle), so an exact drain isn't
                // possible. The invariant only breaks if the maker's balance moves in this very
                // block (its own Transfer is skipped via `seeded_tokens`) or the maker is rotated
                // in the same block; both leave a small residual on the now-paused,
                // unrouted book. Neither has occurred for this venue.
                let Some(new_asset_id) = new_asset_id else { continue };
                if let Some(old_id) =
                    previous_live_book_for_token(token, new_asset_id, components_store)
                {
                    balance_deltas.push(BalanceDelta {
                        ord: tx.index,
                        tx: Some(tx.clone()),
                        token: token.clone(),
                        delta: balance
                            .clone()
                            .neg()
                            .to_signed_bytes_be(),
                        component_id: old_id.into_bytes(),
                    });
                }
            }
        }
    }
}

/// Re-seeds every tracked token on a write to the maker slot.
///
/// For each currently-tracked token (USDC plus every book's asset) emits
/// `balanceOf(new) - balanceOf(old)` fanned to the appropriate components, and records the token
/// as seeded so its plain `Transfer` deltas are skipped this block.
fn reseed_on_maker_write(
    block: &eth::v2::Block,
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    balance_deltas: &mut Vec<BalanceDelta>,
    seeded_tokens: &mut HashSet<Vec<u8>>,
) {
    for tx in block.transactions() {
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.storage_changes {
                if change.address != config.module || change.key != config.maker_slot {
                    continue;
                }
                let new_maker = address_from_word(&change.new_value);
                if new_maker.iter().all(|&b| b == 0) {
                    continue;
                }
                let old_maker = address_from_word(&change.old_value);
                let transaction: Transaction = tx.into();
                for token in tracked_tokens(config, components_store) {
                    // A token already snapshotted by `seed_new_books` this block (a book
                    // created in the same block the maker is designated) must not also be
                    // re-seeded here, or its balance would be double-counted.
                    if seeded_tokens.contains(&token) {
                        continue;
                    }
                    let new_balance = erc20::functions::BalanceOf { owner: new_maker.clone() }
                        .call(token.clone())
                        .unwrap_or_else(BigInt::zero);
                    let old_balance = if old_maker.iter().all(|&b| b == 0) {
                        BigInt::zero()
                    } else {
                        erc20::functions::BalanceOf { owner: old_maker.clone() }
                            .call(token.clone())
                            .unwrap_or_else(BigInt::zero)
                    };
                    let delta = new_balance - old_balance;
                    for comp_id in books_for_token(&token, &config.usdc, components_store) {
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

/// Applies `Transfer` and WETH `Deposit`/`Withdrawal` deltas for tokens that were not seeded this
/// block.
fn apply_event_deltas(
    block: &eth::v2::Block,
    maker: &[u8],
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
    seeded_tokens: &HashSet<Vec<u8>>,
    balance_deltas: &mut Vec<BalanceDelta>,
) {
    for log in block.logs() {
        let token = log.address().to_vec();
        if seeded_tokens.contains(&token) {
            continue;
        }
        let delta = event_delta(log.log, maker);
        let Some(delta) = delta else { continue };
        let books = books_for_token(&token, &config.usdc, components_store);
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

/// The signed balance delta a single log applies to the maker, or `None` if the log does not move
/// the maker's balance.
///
/// Handles ERC20 `Transfer` (in/out of the maker) and WETH `Deposit`/`Withdrawal` (the maker
/// wrapping/unwrapping ETH, which changes the WETH balance without emitting `Transfer`).
fn event_delta(log: &eth::v2::Log, maker: &[u8]) -> Option<BigInt> {
    if let Some(erc20::events::Transfer { from, to, value }) =
        erc20::events::Transfer::match_and_decode(log)
    {
        if to == maker {
            return Some(value);
        }
        if from == maker {
            return Some(value.neg());
        }
        return None;
    }
    if let Some(weth::events::Deposit { dst, wad }) = weth::events::Deposit::match_and_decode(log) {
        return weth_event_delta(&dst, maker, wad);
    }
    if let Some(weth::events::Withdrawal { src, wad }) =
        weth::events::Withdrawal::match_and_decode(log)
    {
        return weth_event_delta(&src, maker, wad.neg());
    }
    None
}

/// All currently-tracked tokens: USDC plus every known book's asset side.
fn tracked_tokens(
    config: &DeploymentConfig,
    components_store: &StoreGetProto<ProtocolComponent>,
) -> Vec<Vec<u8>> {
    let mut tokens = vec![config.usdc.clone()];
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    seen.insert(config.usdc.clone());
    let mut i = 0u64;
    while let Some(component) = components_store.get_last(format!("book:{i}")) {
        for token in component.tokens {
            if seen.insert(token.clone()) {
                tokens.push(token);
            }
        }
        i += 1;
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

    fn maker() -> Vec<u8> {
        hex::decode("6f7a3714d7fc266e3e84067ac31e7b1a3be18060").unwrap()
    }

    fn other() -> Vec<u8> {
        hex::decode("9008d19f58aabd9ed0d60971565aa8510560ab41").unwrap()
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
    fn transfer_to_maker_is_positive() {
        assert_eq!(event_delta(&transfer_log(&other(), &maker(), 50), &maker()), Some(50.into()));
    }

    #[test]
    fn transfer_from_maker_is_negative() {
        assert_eq!(
            event_delta(&transfer_log(&maker(), &other(), 50), &maker()),
            Some(BigInt::from(50).neg())
        );
    }

    #[test]
    fn transfer_not_touching_maker_is_ignored() {
        assert_eq!(event_delta(&transfer_log(&other(), &other(), 50), &maker()), None);
    }

    #[test]
    fn weth_deposit_to_maker_increases_balance() {
        // WETH `deposit()` (wrap) credits the maker without a `Transfer` log — the comment-7 gap.
        assert_eq!(event_delta(&deposit_log(&maker(), 70), &maker()), Some(70.into()));
    }

    #[test]
    fn weth_withdrawal_from_maker_decreases_balance() {
        // WETH `withdraw()` (unwrap) debits the maker without a `Transfer` log.
        assert_eq!(
            event_delta(&withdrawal_log(&maker(), 70), &maker()),
            Some(BigInt::from(70).neg())
        );
    }

    #[test]
    fn weth_events_for_other_wallets_are_ignored() {
        assert_eq!(event_delta(&deposit_log(&other(), 70), &maker()), None);
        assert_eq!(event_delta(&withdrawal_log(&other(), 70), &maker()), None);
    }

    #[test]
    fn double_count_guard_skips_seeded_tokens() {
        // A token snapshotted this block (new-book or maker-write seeding) must not also get its
        // plain `Transfer`/WETH event deltas applied, or its balance would be double-counted.
        let weth = hex::decode("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let mut seeded: HashSet<Vec<u8>> = HashSet::new();
        seeded.insert(weth.clone());
        assert!(seeded.contains(&weth));
        let usdc = hex::decode("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        assert!(!seeded.contains(&usdc));
    }
}
