//! Ascend launchpad pools on Elysium: Uniswap V4 pools of the Elysium `PoolManager` whose
//! `PoolKey.hooks` is an Ascend launchpad hook (`AscendLaunchpadV5`, 0xe0def422...), indexed by the
//! Elysium manifest (`elysium-uniswap-v4-with-hooks.yaml`) next to the Montra pools.
//!
//! The pool math is stock V4, so the shared V4 pipeline indexes ticks, liquidity, price and
//! balances. The Ascend hook adds a fee, a price observation (an EMA and a discount) and a bid
//! inventory, which the simulation reads. This package decodes them from the hook's storage writes
//! into `ascend_*` state attributes, with the same modules as the Montra pools
//! (`map_montra_pools_created`, `store_montra_slots`, `map_montra_enriched_block_changes`).
//!
//! The hook's source is not verified on Blockscout. Its layout was established from its bytecode
//! and traces (spec facts S8, S9 and S21): `mapping(bytes32 poolId => PoolData)` at base slot 4,
//! four words, and `mapping(bytes32 poolId => Bid)` at base slot 2, three words. The hook is not
//! upgradeable (spec S3), so the layout cannot move. Byte offsets count from the low end of a word,
//! the order Solidity packs struct members in.
//!
//! The package watches five slots per pool, the ones that price a swap:
//!
//! | slot        | content                                                        |
//! |-------------|----------------------------------------------------------------|
//! | `PoolData+2` | creator, `tokenIs0`, `launchedAt`, `startTick`, `fee`          |
//! | `PoolData+3` | `emaSqrtPriceX96`, `emaUpdatedAt`, `observedTick`, `discount`  |
//! | `Bid+0`     | `inventory`, the quote the hook holds to fill sells            |
//! | `Bid+1`     | `filledInWindow`                                               |
//! | `Bid+2`     | `windowStart`, `maxClip`                                       |
//!
//! `PoolData+0` (token, builder fee) and `PoolData+1` (quote) are not watched: the swap does not
//! charge the builder fee, and the tokens come from the `PoolKey`.

use crate::{
    montra::pool_config::{Field, HookWord, SignedField},
    storage::{mapping_slot, word_at_offset, Word},
};

/// Base slot of the hook's `mapping(bytes32 => PoolData)`.
const POOL_DATA_MAPPING_SLOT: u64 = 4;
/// Base slot of the hook's `mapping(bytes32 => Bid)`.
const BID_MAPPING_SLOT: u64 = 2;

// PoolData word 2.
const TOKEN_IS_CURRENCY0: Field = Field::new(20, 1);
const START_TICK: SignedField = SignedField::new(26, 3);
const FEE_PIPS: Field = Field::new(29, 3);
// PoolData word 3.
const EMA_UPDATED_AT: Field = Field::new(20, 5);
const OBSERVED_TICK: SignedField = SignedField::new(25, 3);
const DISCOUNT: Field = Field::new(28, 4);
// Bid word 2.
const WINDOW_START: Field = Field::new(0, 5);

/// The EMA, a `uint160` at the low end of `PoolData` word 3.
const EMA_SQRT_PRICE: BytesField = BytesField::new(0, 20);
/// `maxClip`, a `uint128` right above `windowStart` in `Bid` word 2.
const MAX_CLIP: BytesField = BytesField::new(5, 16);

/// A field wider than 8 bytes, kept as its big-endian bytes.
struct BytesField {
    low: usize,
    width: usize,
}

impl BytesField {
    /// The field `width` bytes wide starting `low` bytes from the low end of a word. Panics unless
    /// it ends within the word; every field is a constant, so the panic fails the build.
    const fn new(low: usize, width: usize) -> Self {
        assert!(width >= 1 && low + width <= 32, "a field lies within a word");
        Self { low, width }
    }

    fn bytes(&self, word: &Word) -> Vec<u8> {
        word[32 - self.low - self.width..32 - self.low].to_vec()
    }
}

/// A watched word of an Ascend pool's hook storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AscendWord {
    /// `PoolData` word 2: orientation, start tick and fee.
    Terms,
    /// `PoolData` word 3: the observation.
    Observation,
    /// `Bid` word 0: the bid inventory.
    Inventory,
    /// `Bid` word 1: the quote filled in the current window.
    Filled,
    /// `Bid` word 2: the window start and the clip size.
    Window,
}

impl AscendWord {
    pub(crate) const ALL: [Self; 5] =
        [Self::Terms, Self::Observation, Self::Inventory, Self::Filled, Self::Window];

    /// The word's index in the `store_montra_slots` value. Montra's words use 0 to 6, so these
    /// follow at 7 to 11.
    pub(crate) fn index(self) -> u8 {
        7 + self as u8
    }

    /// The `ascend_*` attributes this word holds, as (name, value) pairs, in the formats the
    /// simulation decoder reads.
    pub(crate) fn attributes(self, value: &Word) -> Vec<(&'static str, Vec<u8>)> {
        let u64_attribute = |field: Field| {
            field
                .unsigned(value)
                .to_be_bytes()
                .to_vec()
        };
        let i32_attribute = |field: SignedField| field.read(value).to_be_bytes().to_vec();
        match self {
            Self::Terms => vec![
                ("ascend_fee_pips", u64_attribute(FEE_PIPS)),
                (
                    "ascend_token_is_currency0",
                    vec![u8::from(TOKEN_IS_CURRENCY0.unsigned(value) != 0)],
                ),
                ("ascend_start_tick", i32_attribute(START_TICK)),
            ],
            Self::Observation => vec![
                ("ascend_ema_sqrt_price_x96", EMA_SQRT_PRICE.bytes(value)),
                ("ascend_ema_updated_at", u64_attribute(EMA_UPDATED_AT)),
                ("ascend_observed_tick", i32_attribute(OBSERVED_TICK)),
                ("ascend_discount", u64_attribute(DISCOUNT)),
            ],
            Self::Inventory => vec![("ascend_bid_inventory", value.to_vec())],
            Self::Filled => vec![("ascend_bid_filled_in_window", value.to_vec())],
            Self::Window => vec![
                ("ascend_bid_window_start", u64_attribute(WINDOW_START)),
                ("ascend_bid_max_clip", MAX_CLIP.bytes(value)),
            ],
        }
    }
}

/// The five watched slots of the hook's storage for `pool_id`, each with the word it holds.
pub(crate) fn pool_slots(pool_id: &Word) -> Vec<(HookWord, Word)> {
    let pool_data = mapping_slot(pool_id, POOL_DATA_MAPPING_SLOT);
    let bid = mapping_slot(pool_id, BID_MAPPING_SLOT);
    vec![
        (HookWord::Ascend(AscendWord::Terms), word_at_offset(&pool_data, 2)),
        (HookWord::Ascend(AscendWord::Observation), word_at_offset(&pool_data, 3)),
        (HookWord::Ascend(AscendWord::Inventory), word_at_offset(&bid, 0)),
        (HookWord::Ascend(AscendWord::Filled), word_at_offset(&bid, 1)),
        (HookWord::Ascend(AscendWord::Window), word_at_offset(&bid, 2)),
    ]
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;

    use super::*;

    fn u64_bytes(value: u64) -> Vec<u8> {
        value.to_be_bytes().to_vec()
    }

    /// Pool 0xa138ea3b...'s `PoolData` word 2 at block 8365800 (spec S8): fee 10000 (0x002710),
    /// startTick -168600 (0xfd6d68), launchedAt 1794307976 (0x6af2f788), tokenIs0 0, then the
    /// creator, whose bytes here are arbitrary.
    const POOL_A138_WORD_2: Word =
        hex!("002710fd6d68006af2f78800aabbccddeeff00112233445566778899aabbccdd");

    /// A `PoolData` word 3 as launch writes it (spec S20): d = 0, observedTick = start - 200 =
    /// -400800 (0xf9e260) for a pool starting at -400600, emaUpdatedAt = 1794362659
    /// (0x6afecd23), and an arbitrary 20-byte EMA.
    const LAUNCH_WORD_3: Word =
        hex!("00000000f9e260006afecd2300000000000000000883956ab34ccb0338ab12cd");

    #[test]
    fn test_word_2_decodes_fee_orientation_and_start_tick() {
        assert_eq!(
            AscendWord::Terms.attributes(&POOL_A138_WORD_2),
            vec![
                ("ascend_fee_pips", u64_bytes(10000)),
                ("ascend_token_is_currency0", vec![0]),
                ("ascend_start_tick", hex!("fffd6d68").to_vec()),
            ]
        );
    }

    /// Pool 0x21e12450...'s three launch words as `cast storage` read them on the Elysium node at
    /// block 3437284, its launch block (tokenIs0 1, fee 20000, start tick -168600), at the slots
    /// `cast index bytes32 <id> 4` plus 2 and 3, and `... 2` plus 2.
    #[test]
    fn test_launch_words_of_a_chain_pool_decode() {
        let word_2 = hex!("004e20fd6d68006af21470019838640a0465cbeebac773a23ac1fb840c1f6aa8");
        let word_3 = hex!("00000000fd6ca0006af214700000000000000000000e2a3ff8d70d5dd274e6ba");
        let bid_2 = hex!("00000000000000000000000000000000000000a49e728e240f3a9d0000000000");

        assert_eq!(
            AscendWord::Terms.attributes(&word_2),
            vec![
                ("ascend_fee_pips", u64_bytes(20000)),
                ("ascend_token_is_currency0", vec![1]),
                ("ascend_start_tick", hex!("fffd6d68").to_vec()),
            ]
        );
        assert_eq!(
            AscendWord::Observation.attributes(&word_3),
            vec![
                (
                    "ascend_ema_sqrt_price_x96",
                    hex!("0000000000000000000e2a3ff8d70d5dd274e6ba").to_vec()
                ),
                ("ascend_ema_updated_at", u64_bytes(0x6af21470)),
                // start tick - 200, the observation a launch writes for tokenIs0
                ("ascend_observed_tick", hex!("fffd6ca0").to_vec()),
                ("ascend_discount", u64_bytes(0)),
            ]
        );
        assert_eq!(
            AscendWord::Window.attributes(&bid_2),
            vec![
                ("ascend_bid_window_start", u64_bytes(0)),
                ("ascend_bid_max_clip", hex!("0000000000000000a49e728e240f3a9d").to_vec()),
            ]
        );
    }

    #[test]
    fn test_word_2_reads_token_is_currency0_from_byte_20() {
        let mut word = POOL_A138_WORD_2;
        word[11] = 1;

        assert_eq!(AscendWord::Terms.attributes(&word)[1], ("ascend_token_is_currency0", vec![1]));
    }

    #[test]
    fn test_word_3_decodes_the_observation() {
        assert_eq!(
            AscendWord::Observation.attributes(&LAUNCH_WORD_3),
            vec![
                (
                    "ascend_ema_sqrt_price_x96",
                    hex!("00000000000000000883956ab34ccb0338ab12cd").to_vec()
                ),
                ("ascend_ema_updated_at", u64_bytes(0x6afecd23)),
                ("ascend_observed_tick", hex!("fff9e260").to_vec()),
                ("ascend_discount", u64_bytes(0)),
            ]
        );
    }

    #[test]
    fn test_word_3_decodes_a_discount_in_the_top_four_bytes() {
        // d = 0xf0000000 as in the spec's S32 (a) quotes.
        let mut word = LAUNCH_WORD_3;
        word[..4].copy_from_slice(&hex!("f0000000"));

        assert_eq!(
            AscendWord::Observation.attributes(&word)[3],
            ("ascend_discount", u64_bytes(0xf0000000))
        );
    }

    /// Pool 0xa138ea3b... `Bid+2` at block 8365800 (spec S9): `...a49e728e240f3a9d006aff186a`
    /// holds maxClip 11862044423356365469 (0xa49e728e240f3a9d) above windowStart 0x006aff186a.
    #[test]
    fn test_bid_word_2_decodes_window_start_and_max_clip() {
        // The spec prints the word without its leading zeros: right-align it.
        let printed = hex!("a49e728e240f3a9d006aff186a");
        let mut word = [0u8; 32];
        word[32 - printed.len()..].copy_from_slice(&printed);

        assert_eq!(
            AscendWord::Window.attributes(&word),
            vec![
                ("ascend_bid_window_start", u64_bytes(0x6aff186a)),
                ("ascend_bid_max_clip", hex!("0000000000000000a49e728e240f3a9d").to_vec()),
            ]
        );
        assert_eq!(0xa49e728e240f3a9du64, 11862044423356365469);
    }

    #[test]
    fn test_bid_words_0_and_1_are_whole_words() {
        let inventory = hex!("00000000000000000000000000000000000000000000000000000000000000ff");

        assert_eq!(
            AscendWord::Inventory.attributes(&inventory),
            vec![("ascend_bid_inventory", inventory.to_vec())]
        );
        assert_eq!(
            AscendWord::Filled.attributes(&inventory),
            vec![("ascend_bid_filled_in_window", inventory.to_vec())]
        );
    }

    /// `cast index bytes32 0xa138ea3b...01a5 4` and `... 2` plus the word offsets, the slots spec
    /// S8 and S9 read with `cast storage`.
    #[test]
    fn test_pool_slots_are_the_pool_data_and_bid_words() {
        let pool_id = hex!("a138ea3bc220ee819a81be441ad1fd95f84e5ff228a6c1d862376c66e60a01a5");
        let pool_data = hex!("eed4aedf482dc31cd5db10dafcce24e9cd678d08586286766e595713052566");
        let bid = hex!("3fc5cc6e735d08aa6f82fe1e4f7818d4372f3a52e5f4ac7f60a89c2c4a4d4f");
        let slot = |prefix: [u8; 31], last: u8| {
            let mut slot = [0u8; 32];
            slot[..31].copy_from_slice(&prefix);
            slot[31] = last;
            slot
        };

        assert_eq!(
            pool_slots(&pool_id),
            vec![
                (HookWord::Ascend(AscendWord::Terms), slot(pool_data, 0xe1)),
                (HookWord::Ascend(AscendWord::Observation), slot(pool_data, 0xe2)),
                (HookWord::Ascend(AscendWord::Inventory), slot(bid, 0xfd)),
                (HookWord::Ascend(AscendWord::Filled), slot(bid, 0xfe)),
                (HookWord::Ascend(AscendWord::Window), slot(bid, 0xff)),
            ]
        );
    }
}
