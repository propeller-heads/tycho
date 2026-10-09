//! Pricing of the Biconomy PropAMM venue, ported from `PropAMMExecutor.board()` and the venue's
//! `_merge` / `_allocate` so that a quote equals `PropAMMVenue.quote` at the same block.
//!
//! Boards are decoded from the executor's raw storage words. Every arithmetic step is the
//! contract's, including where it rounds down; an operation that would revert on chain returns
//! [`PricingError::Reverts`].

use std::collections::BTreeMap;

use alloy::primitives::U256;

pub type Address = [u8; 20];

pub const PPM: u64 = 1_000_000;
const BPS: u64 = 10_000;
const MODE_PRICES: u8 = 1;
const MODE_OFFSETS: u8 = 2;
/// Levels a board can hold (`MAX_LEVELS` in the executor).
pub const MAX_LEVELS: usize = 20;
/// Storage words of one `Board` struct.
pub const BOARD_WORDS: u8 = 24;
const SLOT_LEVELS: u8 = 2;
const SLOT_CONTROLS: u8 = 22;
const SLOT_METER: u8 = 23;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PricingError {
    /// The contract would revert (overflow, underflow or division by zero).
    Reverts,
}

fn wad() -> U256 {
    U256::from(1_000_000_000_000_000_000u64)
}

fn bits(word: U256, shift: usize, width: usize) -> U256 {
    (word >> shift) & ((U256::from(1u8) << width) - U256::from(1u8))
}

fn bits_u64(word: U256, shift: usize, width: usize) -> u64 {
    bits(word, shift, width).to::<u64>()
}

/// One stored level: cumulative size, and an absolute price or an offset in ppm by mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedLevel {
    pub size: U256,
    pub value: U256,
}

/// A level as fills price it right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Level {
    pub size: U256,
    pub price: U256,
}

/// `PropAMMExecutor.Board` decoded from its 24 storage words. Missing words are zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Board {
    pub provider: Address,
    pub expires_at: u64,
    pub mode: u8,
    pub drift_ppm_per_second: u64,
    pub filled: U256,
    pub levels: Vec<PackedLevel>,
    pub block_cap: U256,
    pub widen_ppm_per_sqrt_second: u64,
    pub premium_ppm: u64,
    pub premium_blocks: u64,
    pub last_fill_block: u64,
    pub filled_in_block: U256,
    pub last_commit_block: u64,
    pub committed_at: u64,
}

impl Board {
    pub fn decode(words: &BTreeMap<u8, U256>) -> Self {
        let word = |slot: u8| {
            words
                .get(&slot)
                .copied()
                .unwrap_or_default()
        };
        let w0 = word(0);
        let w1 = word(1);
        let w22 = word(SLOT_CONTROLS);
        let w23 = word(SLOT_METER);

        let mut provider = [0u8; 20];
        provider.copy_from_slice(&w0.to_be_bytes::<32>()[12..]);
        let level_count = (bits_u64(w0, 200, 8) as usize).min(MAX_LEVELS);
        let levels = (0..level_count)
            .map(|i| {
                let w = word(SLOT_LEVELS + i as u8);
                PackedLevel { size: bits(w, 0, 128), value: bits(w, 128, 128) }
            })
            .collect();

        Self {
            provider,
            expires_at: bits_u64(w0, 160, 40),
            mode: bits_u64(w0, 208, 8) as u8,
            drift_ppm_per_second: bits_u64(w0, 216, 32),
            filled: bits(w1, 128, 128),
            levels,
            block_cap: bits(w22, 0, 128),
            widen_ppm_per_sqrt_second: bits_u64(w22, 128, 32),
            premium_ppm: bits_u64(w22, 160, 32),
            premium_blocks: bits_u64(w22, 192, 16),
            last_fill_block: bits_u64(w23, 0, 40),
            filled_in_block: bits(w23, 40, 128),
            last_commit_block: bits_u64(w23, 168, 40),
            committed_at: bits_u64(w23, 208, 40),
        }
    }

    /// Writes a fill into the board's words the way `PropAMMExecutor.fill` stores it: the
    /// version meter always, the block meter only when the board has a block cap.
    pub fn record_fill(
        words: &mut BTreeMap<u8, U256>,
        filled_after: U256,
        block_number: u64,
        filled_in_block: U256,
    ) {
        let low128 = (U256::from(1u8) << 128) - U256::from(1u8);
        let w1 = words
            .get(&1)
            .copied()
            .unwrap_or_default();
        words.insert(1, (w1 & low128) | (filled_after << 128));

        let board = Self::decode(words);
        if board.block_cap.is_zero() {
            return;
        }
        let w23 = words
            .get(&SLOT_METER)
            .copied()
            .unwrap_or_default();
        let meter: U256 = (U256::from(1u8) << 168) - U256::from(1u8);
        let keep = w23 & !meter;
        words.insert(SLOT_METER, keep | (filled_in_block << 40) | U256::from(block_number));
    }
}

/// `PropAMMExecutor.PackedAnchor` decoded from its storage word.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PackedAnchor {
    pub price: U256,
    pub signed_reverse: bool,
    pub timestamp_ms: u64,
    pub ttl: u64,
    pub skew0_ppm: u64,
    pub skew1_ppm: u64,
    pub commit_block: u64,
}

impl PackedAnchor {
    pub fn decode(word: U256) -> Self {
        Self {
            price: bits(word, 0, 120),
            signed_reverse: bits_u64(word, 120, 8) == 1,
            timestamp_ms: bits_u64(word, 128, 48),
            ttl: bits_u64(word, 176, 16),
            skew0_ppm: bits_u64(word, 192, 24),
            skew1_ppm: bits_u64(word, 216, 24),
            commit_block: bits_u64(word, 240, 16),
        }
    }
}

/// The pair anchor resolved for one direction (`AnchorView`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnchorView {
    pub price: U256,
    pub timestamp: u64,
    pub expires_at: u64,
    pub skew_ppm: u64,
    pub commit_block: u64,
}

/// `_anchorView`: only offset boards read the anchor. `anchor` is the word stored under the
/// sorted pair.
pub fn anchor_view(
    board: &Board,
    token_in: &Address,
    token_out: &Address,
    anchor: U256,
) -> AnchorView {
    if board.mode != MODE_OFFSETS {
        return AnchorView::default();
    }
    let a = PackedAnchor::decode(anchor);
    if a.price.is_zero() {
        return AnchorView::default();
    }
    let ask_reverse = token_in > token_out;
    let signed_side = ask_reverse == a.signed_reverse;
    let price =
        if signed_side { a.price } else { U256::from(10u8).pow(U256::from(36u8)) / a.price };
    let timestamp = a.timestamp_ms / 1000;
    AnchorView {
        price,
        timestamp,
        expires_at: timestamp + a.ttl,
        skew_ppm: if ask_reverse { a.skew1_ppm } else { a.skew0_ppm },
        commit_block: a.commit_block,
    }
}

/// The block a quote executes in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockEnv {
    pub number: u64,
    pub timestamp: u64,
}

fn is_live(board: &Board, av: &AnchorView, env: &BlockEnv) -> bool {
    if board.mode != MODE_PRICES && board.mode != MODE_OFFSETS {
        return false;
    }
    if env.timestamp > board.expires_at {
        return false;
    }
    !(board.mode == MODE_OFFSETS && (av.price.is_zero() || env.timestamp > av.expires_at))
}

/// Floor square root, as OpenZeppelin `Math.sqrt`.
fn isqrt(value: u64) -> u64 {
    if value < 2 {
        return value;
    }
    let mut x = (value as f64).sqrt() as u64;
    while x.saturating_mul(x) > value {
        x -= 1;
    }
    while (x + 1).saturating_mul(x + 1) <= value {
        x += 1;
    }
    x
}

/// `_extraDiscount`: widening with the square root of the quote's age plus the premium while
/// the board is inside its premium window.
fn extra_discount(board: &Board, av: &AnchorView, env: &BlockEnv) -> Result<U256, PricingError> {
    let mut extra = U256::ZERO;
    if board.widen_ppm_per_sqrt_second != 0 {
        let origin = if board.mode == MODE_OFFSETS { av.timestamp } else { board.committed_at };
        let age = env.timestamp.saturating_sub(origin);
        extra = U256::from(board.widen_ppm_per_sqrt_second) * U256::from(isqrt(age));
    }
    let window = board.premium_blocks;
    if window != 0 {
        let since_commit = env
            .number
            .checked_sub(board.last_commit_block)
            .ok_or(PricingError::Reverts)?;
        let mut in_window = since_commit < window;
        if !in_window && !av.price.is_zero() {
            // The anchor's commit block is kept modulo 2^16.
            in_window =
                u64::from((env.number as u16).wrapping_sub(av.commit_block as u16)) < window;
        }
        if in_window {
            extra += U256::from(board.premium_ppm);
        }
    }
    Ok(extra)
}

/// `_effectiveLevels`: the levels fills price at in this block.
fn effective_levels(
    board: &Board,
    av: &AnchorView,
    env: &BlockEnv,
) -> Result<Vec<Level>, PricingError> {
    let ppm = U256::from(PPM);
    let extra = extra_discount(board, av, env)?;
    if board.mode == MODE_PRICES {
        if extra >= ppm {
            return Ok(Vec::new());
        }
        return Ok(board
            .levels
            .iter()
            .map(|level| Level {
                size: level.size,
                price: if extra.is_zero() {
                    level.value
                } else {
                    level.value * (ppm - extra) / ppm
                },
            })
            .collect());
    }

    let age = env
        .timestamp
        .saturating_sub(av.timestamp);
    let drift =
        U256::from(board.drift_ppm_per_second) * U256::from(age) + extra + U256::from(av.skew_ppm);
    let mut levels = Vec::with_capacity(board.levels.len());
    for level in &board.levels {
        let discount = level.value + drift;
        if discount >= ppm {
            break;
        }
        levels.push(Level { size: level.size, price: av.price * (ppm - discount) / ppm });
    }
    Ok(levels)
}

/// `_inventoryCapacity`: the most tokenIn the board takes from `filled` before its swept output
/// exceeds `avail_out`.
fn inventory_capacity(
    levels: &[Level],
    filled: U256,
    avail_out: U256,
) -> Result<U256, PricingError> {
    if avail_out >= U256::from(u128::MAX) {
        return Ok(U256::MAX);
    }
    let mut cursor = filled;
    let mut out_left = avail_out;
    let mut cap_in = U256::ZERO;
    for level in levels {
        if cursor < level.size {
            let take = level.size - cursor;
            let seg_out = take * level.price / wad();
            if seg_out > out_left {
                if level.price.is_zero() {
                    return Err(PricingError::Reverts);
                }
                return Ok(
                    cap_in + ((out_left + U256::from(1u8)) * wad() - U256::from(1u8)) / level.price
                );
            }
            cap_in += take;
            out_left -= seg_out;
            cursor = level.size;
        }
    }
    Ok(cap_in)
}

/// One maker's board for a direction, as `PropAMMExecutor.board()` reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoardView {
    pub levels: Vec<Level>,
    pub filled: U256,
    pub remaining: U256,
}

/// Everything a board's pricing reads, resolved by the caller from the indexed state.
pub struct BoardInputs<'a> {
    pub board: &'a Board,
    pub anchor: U256,
    pub paused: bool,
    /// `provider.available(tokenOut)`; `None` when the indexer has not reported it.
    pub available_out: Option<U256>,
}

/// `PropAMMExecutor.board()` for one maker and direction.
pub fn board_view(
    inputs: &BoardInputs<'_>,
    token_in: &Address,
    token_out: &Address,
    env: &BlockEnv,
) -> Result<BoardView, PricingError> {
    let board = inputs.board;
    let av = anchor_view(board, token_in, token_out, inputs.anchor);
    let filled = board.filled;
    if inputs.paused || !is_live(board, &av, env) {
        return Ok(BoardView { levels: Vec::new(), filled, remaining: U256::ZERO });
    }

    let levels = effective_levels(board, &av, env)?;
    let mut remaining = match levels.last() {
        Some(top) if top.size > filled => top.size - filled,
        _ => U256::ZERO,
    };
    let cap_left = if board.block_cap.is_zero() {
        U256::MAX
    } else {
        let in_block =
            if board.last_fill_block == env.number { board.filled_in_block } else { U256::ZERO };
        if in_block >= board.block_cap {
            U256::ZERO
        } else {
            board.block_cap - in_block
        }
    };
    remaining = remaining.min(cap_left);
    if !remaining.is_zero() {
        // A provider with no indexed inventory is treated as empty.
        let available = inputs.available_out.unwrap_or_default();
        remaining = remaining.min(inventory_capacity(&levels, filled, available)?);
    }
    Ok(BoardView { levels, filled, remaining })
}

/// The venue's merge across makers for one size (`_merge`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Input routed to each maker, in registry order.
    pub alloc: Vec<U256>,
    /// Each maker's floored output for its allocation.
    pub out_by_maker: Vec<U256>,
    pub out: U256,
    pub covered: U256,
}

/// `_merge`: allocate best price first, ties to the earlier maker; drop any maker whose
/// allocation pays out zero and allocate again over the rest.
pub fn merge(boards: &[BoardView], amount_in: U256) -> Plan {
    let mut excluded: Vec<bool> = boards
        .iter()
        .map(|b| b.remaining.is_zero())
        .collect();
    loop {
        let plan = allocate(boards, &excluded, amount_in);
        let mut redo = false;
        for (i, (alloc, out)) in plan
            .alloc
            .iter()
            .zip(&plan.out_by_maker)
            .enumerate()
        {
            if !alloc.is_zero() && out.is_zero() {
                excluded[i] = true;
                redo = true;
            }
        }
        if !redo {
            return plan;
        }
    }
}

/// One pass of `_allocate`.
fn allocate(boards: &[BoardView], excluded: &[bool], amount_in: U256) -> Plan {
    let k = boards.len();
    let mut plan = Plan {
        alloc: vec![U256::ZERO; k],
        out_by_maker: vec![U256::ZERO; k],
        out: U256::ZERO,
        covered: U256::ZERO,
    };
    let mut cursor = vec![U256::ZERO; k];
    let mut left = vec![U256::ZERO; k];
    let mut next: Vec<Option<usize>> = vec![None; k];
    for i in 0..k {
        if excluded[i] {
            continue;
        }
        cursor[i] = boards[i].filled;
        left[i] = boards[i].remaining;
        let mut j = 0;
        while j < boards[i].levels.len() && boards[i].levels[j].size <= boards[i].filled {
            j += 1;
        }
        next[i] = Some(j);
    }

    let mut remaining_in = amount_in;
    while !remaining_in.is_zero() {
        let mut best: Option<(usize, U256)> = None;
        for i in 0..k {
            let Some(j) = next[i] else { continue };
            let Some(level) = boards[i].levels.get(j) else { continue };
            if best.is_none_or(|(_, price)| level.price > price) {
                best = Some((i, level.price));
            }
        }
        let Some((best, best_price)) = best else { break };
        let level_index = next[best].expect("best maker has a next level");
        let top = boards[best].levels[level_index].size;
        let take = (top - cursor[best])
            .min(remaining_in)
            .min(left[best]);
        let seg_out = take * best_price / wad();
        plan.alloc[best] += take;
        plan.out_by_maker[best] += seg_out;
        plan.out += seg_out;
        plan.covered += take;
        remaining_in -= take;
        cursor[best] += take;
        left[best] -= take;
        if left[best].is_zero() {
            next[best] = None;
        } else if cursor[best] == top {
            next[best] = Some(level_index + 1);
        }
    }
    plan
}

/// The venue's protocol fee: `out - out * feeBps / 10000`.
pub fn after_protocol_fee(out: U256, fee_bps: u16) -> U256 {
    out - out * U256::from(fee_bps) / U256::from(BPS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(pairs: &[(u8, U256)]) -> BTreeMap<u8, U256> {
        pairs.iter().copied().collect()
    }

    fn header(provider: u8, expires_at: u64, level_count: u64, mode: u64, drift: u64) -> U256 {
        U256::from(provider) |
            (U256::from(expires_at) << 160) |
            (U256::from(level_count) << 200) |
            (U256::from(mode) << 208) |
            (U256::from(drift) << 216)
    }

    fn level(size: u128, value: u128) -> U256 {
        U256::from(size) | (U256::from(value) << 128)
    }

    fn price_board(levels: &[(u128, u128)], filled: u128) -> Board {
        let mut w = vec![
            (0u8, header(7, 1_000, levels.len() as u64, 1, 0)),
            (1, U256::from(filled) << 128),
        ];
        for (i, (size, price)) in levels.iter().enumerate() {
            w.push((SLOT_LEVELS + i as u8, level(*size, *price)));
        }
        Board::decode(&words(&w))
    }

    fn env() -> BlockEnv {
        BlockEnv { number: 100, timestamp: 500 }
    }

    fn view(board: &Board, available: Option<U256>) -> BoardView {
        let inputs =
            BoardInputs { board, anchor: U256::ZERO, paused: false, available_out: available };
        board_view(&inputs, &[1; 20], &[2; 20], &env()).unwrap()
    }

    #[test]
    fn decodes_packed_board_fields() {
        let board = Board::decode(&words(&[
            (0, header(9, 1_234, 1, 2, 77)),
            (1, U256::from(5u8) | (U256::from(42u8) << 128)),
            (SLOT_LEVELS, level(1_000, 300)),
            (
                SLOT_CONTROLS,
                U256::from(500u16) |
                    (U256::from(11u8) << 128) |
                    (U256::from(22u8) << 160) |
                    (U256::from(3u8) << 192),
            ),
            (
                SLOT_METER,
                U256::from(90u8) |
                    (U256::from(60u8) << 40) |
                    (U256::from(88u8) << 168) |
                    (U256::from(450u16) << 208),
            ),
        ]));

        assert_eq!(board.provider[19], 9);
        assert_eq!(board.expires_at, 1_234);
        assert_eq!(board.mode, MODE_OFFSETS);
        assert_eq!(board.drift_ppm_per_second, 77);
        assert_eq!(board.filled, U256::from(42u8));
        assert_eq!(
            board.levels,
            vec![PackedLevel { size: U256::from(1_000u16), value: U256::from(300u16) }]
        );
        assert_eq!(board.block_cap, U256::from(500u16));
        assert_eq!(board.widen_ppm_per_sqrt_second, 11);
        assert_eq!(board.premium_ppm, 22);
        assert_eq!(board.premium_blocks, 3);
        assert_eq!(board.last_fill_block, 90);
        assert_eq!(board.filled_in_block, U256::from(60u8));
        assert_eq!(board.last_commit_block, 88);
        assert_eq!(board.committed_at, 450);
    }

    #[test]
    fn remaining_is_capped_by_inventory() {
        // 1e18 wad prices: level 1 pays 2 out per in up to 100 in, level 2 pays 1 per in up to 300.
        let board =
            price_board(&[(100, 2_000_000_000_000_000_000), (300, 1_000_000_000_000_000_000)], 0);

        assert_eq!(view(&board, Some(U256::MAX)).remaining, U256::from(300u16));
        // 250 out covers the first level (200 out) and 50 more input on the second.
        assert_eq!(view(&board, Some(U256::from(250u16))).remaining, U256::from(150u16));
        assert_eq!(view(&board, None).remaining, U256::ZERO);
    }

    #[test]
    fn merge_takes_best_price_first_with_registry_tie_break() {
        let a = view(&price_board(&[(100, 2_000_000_000_000_000_000)], 0), Some(U256::MAX));
        let b = view(
            &price_board(&[(100, 3_000_000_000_000_000_000), (200, 2_000_000_000_000_000_000)], 0),
            Some(U256::MAX),
        );

        let plan = merge(&[a, b], U256::from(250u16));

        // 100 at 3 on b, then the tie at 2 goes to a first (100), then 50 more on b.
        assert_eq!(plan.alloc, vec![U256::from(100u8), U256::from(150u8)]);
        assert_eq!(plan.out, U256::from(300 + 200 + 100u16));
        assert_eq!(plan.covered, U256::from(250u16));
    }

    #[test]
    fn merge_drops_makers_whose_allocation_pays_nothing() {
        // A sub-unit price floors one wei of input to zero output.
        let dust = view(&price_board(&[(10, 500_000_000_000_000_000)], 0), Some(U256::MAX));
        let plan = merge(&[dust], U256::from(1u8));

        assert_eq!(plan.covered, U256::ZERO);
        assert_eq!(plan.out, U256::ZERO);
    }

    #[test]
    fn offset_board_prices_against_the_anchor_with_drift() {
        let token_in = [1u8; 20];
        let token_out = [2u8; 20];
        let mut w = vec![(0u8, header(7, 1_000, 1, 2, 10)), (1, U256::ZERO)];
        w.push((SLOT_LEVELS, level(1_000, 500)));
        let board = Board::decode(&words(&w));
        // Anchor 2e18 signed for token_in -> token_out at t = 490 s, ttl 60 s.
        let anchor = U256::from(2_000_000_000_000_000_000u64) |
            (U256::from(490_000u64) << 128) |
            (U256::from(60u8) << 176);
        let inputs =
            BoardInputs { board: &board, anchor, paused: false, available_out: Some(U256::MAX) };

        let view = board_view(&inputs, &token_in, &token_out, &env()).unwrap();

        // Age 10 s at 10 ppm/s plus a 500 ppm offset: 2e18 * (1e6 - 600) / 1e6.
        assert_eq!(view.levels[0].price, U256::from(1_998_800_000_000_000_000u64));
        assert_eq!(view.remaining, U256::from(1_000u16));
    }

    #[test]
    fn records_fills_in_both_meters() {
        let mut w = words(&[
            (0, header(7, 1_000, 1, 1, 0)),
            (1, U256::from(3u8)),
            (SLOT_CONTROLS, U256::from(50u8)),
            (SLOT_METER, U256::from(99u8) << 208),
        ]);

        Board::record_fill(&mut w, U256::from(40u8), 100, U256::from(40u8));
        let board = Board::decode(&w);

        assert_eq!(board.filled, U256::from(40u8));
        assert_eq!(w[&1] & U256::from(u128::MAX), U256::from(3u8));
        assert_eq!(board.last_fill_block, 100);
        assert_eq!(board.filled_in_block, U256::from(40u8));
        assert_eq!(board.committed_at, 99);
    }
}
