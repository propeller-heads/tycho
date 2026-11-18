//! The Montra `LaunchHook`'s per-pool storage, and the `montra_*` attributes decoded from it.
//!
//! The hook (0x6b1216a4...; its code hash, sampled at three blocks, is in the package Readme's
//! upgrade boundary) keeps `mapping(bytes32 poolId => PoolConfig)` at base slot 2, four words, and
//! `mapping(bytes32 poolId => PoolKey)` at base slot 3, three words. `PoolConfig` word 3 is the fee
//! recipient, which the simulation does not read, so this package does not watch it. The layout
//! below was read off the launch transaction 0xcfeabcb8...'s storage writes and checked against the
//! hook's `poolConfig(bytes32)` and `poolKey(bytes32)` getters at block 6002000 (tx
//! 0xcfeabcb8807834089cfbcb83192c137230074ea8492225890851081f289c0149). Byte offsets count from
//! the low (least significant) end of a word, the order Solidity packs struct members in.

use anyhow::{anyhow, Result};

use crate::{
    address_list::Address,
    storage::{is_uint_window, mapping_slot, read_uint, word_at_offset, Word},
};

/// Base slot of the hook's `PoolConfig` mapping.
const POOL_CONFIG_MAPPING_SLOT: u64 = 2;
/// Base slot of the hook's `PoolKey` mapping.
const POOL_KEY_MAPPING_SLOT: u64 = 3;
/// Words of a `PoolConfig`, the unwatched recipient word 3 included.
const POOL_CONFIG_WORDS: u8 = 4;

/// A field packed into a storage word: `width` bytes starting `low` bytes from the low end.
#[derive(Clone, Copy)]
struct Field {
    low: usize,
    width: usize,
}

impl Field {
    /// The field `width` bytes wide starting `low` bytes from the low end of a word. Panics unless
    /// [`is_uint_window`] holds for the window, the check `read_uint` makes. Every field is a
    /// constant, so the panic fails the build.
    const fn new(low: usize, width: usize) -> Self {
        assert!(is_uint_window(low, width), "a field is a window read_uint reads");
        Self { low, width }
    }

    /// The field read as an unsigned big-endian integer.
    fn unsigned(self, word: &Word) -> u64 {
        read_uint(word, self.low, self.width)
            .expect("Field::new admits only windows read_uint reads")
    }
}

/// A two's-complement field packed into a storage word, at most 4 bytes wide so that it fits an
/// `i32`.
#[derive(Clone, Copy)]
struct SignedField(Field);

impl SignedField {
    /// The signed field `width` bytes wide starting `low` bytes from the low end of a word. Panics
    /// unless the field is at most 4 bytes wide and [`Field::new`] accepts it.
    const fn new(low: usize, width: usize) -> Self {
        assert!(width <= 4, "a signed field fits an i32");
        Self(Field::new(low, width))
    }

    /// The field read as a two's-complement integer of its width, sign-extended.
    fn read(self, word: &Word) -> i32 {
        let shift = 64 - 8 * self.0.width as u32;
        ((self.0.unsigned(word) << shift) as i64 >> shift) as i32
    }
}

// PoolConfig word 0.
const TOTAL_FEE_PIPS: Field = Field::new(0, 3);
const LP_FEE_PIPS: Field = Field::new(3, 3);
const QUOTE_IS_CURRENCY0: Field = Field::new(10, 1);
const SEEDED: Field = Field::new(12, 1);
const EARLY_BUY_FEE_UNTIL: Field = Field::new(14, 5);
const STAGE: Field = Field::new(25, 1);
const LP_CROSSED_AT: Field = Field::new(26, 4);
const LP_HELD_SECONDS: Field = Field::new(30, 2);
// PoolConfig word 1.
const SEED_TICK_LOWER: SignedField = SignedField::new(26, 3);
const SEED_TICK_UPPER: SignedField = SignedField::new(29, 3);
// PoolConfig word 2.
const EARLY_BUY_FEE_START_PIPS: Field = Field::new(0, 3);
const EARLY_BUY_FEE_WINDOW_SECONDS: Field = Field::new(3, 4);

/// A watched word of a pool's `PoolConfig`; the discriminant is the word's offset in the struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum ConfigWord {
    /// Word 0: fee terms, quote side, `seeded` flag, anti-snipe deadline, stage and LP-line hold.
    Terms = 0,
    /// Word 1: the seed's tick range.
    SeedRange = 1,
    /// Word 2: the anti-snipe start rate and window.
    EarlyBuyFee = 2,
}

impl ConfigWord {
    pub(crate) const ALL: [Self; 3] = [Self::Terms, Self::SeedRange, Self::EarlyBuyFee];

    /// The `montra_*` attributes this word holds, as (name, value) pairs. A byte the simulation
    /// does not read holds none.
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
                ("montra_total_fee_pips", u64_attribute(TOTAL_FEE_PIPS)),
                ("montra_lp_fee_pips", u64_attribute(LP_FEE_PIPS)),
                (
                    "montra_quote_is_currency0",
                    vec![u8::from(QUOTE_IS_CURRENCY0.unsigned(value) != 0)],
                ),
                ("montra_early_buy_fee_until", u64_attribute(EARLY_BUY_FEE_UNTIL)),
                // A one-byte field, so the cast keeps every bit.
                ("montra_stage", vec![STAGE.unsigned(value) as u8]),
                ("montra_lp_crossed_at", u64_attribute(LP_CROSSED_AT)),
                ("montra_lp_held_seconds", u64_attribute(LP_HELD_SECONDS)),
            ],
            Self::SeedRange => vec![
                ("montra_seed_tick_lower", i32_attribute(SEED_TICK_LOWER)),
                ("montra_seed_tick_upper", i32_attribute(SEED_TICK_UPPER)),
            ],
            Self::EarlyBuyFee => vec![
                ("montra_early_buy_fee_start_pips", u64_attribute(EARLY_BUY_FEE_START_PIPS)),
                (
                    "montra_early_buy_fee_window_seconds",
                    u64_attribute(EARLY_BUY_FEE_WINDOW_SECONDS),
                ),
            ],
        }
    }
}

/// A word of a pool's `PoolKey`; the discriminant is the word's offset in the struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum KeyWord {
    Currency0 = 0,
    Currency1 = 1,
    /// `fee`, `tickSpacing` and `hooks`, packed into one word.
    FeeTickSpacingHooks = 2,
}

/// A word of a pool's hook storage this package watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HookWord {
    Config(ConfigWord),
    Key(KeyWord),
}

impl HookWord {
    const ALL: [Self; 6] = [
        Self::Config(ConfigWord::Terms),
        Self::Config(ConfigWord::SeedRange),
        Self::Config(ConfigWord::EarlyBuyFee),
        Self::Key(KeyWord::Currency0),
        Self::Key(KeyWord::Currency1),
        Self::Key(KeyWord::FeeTickSpacingHooks),
    ];

    /// The word's index in the store value, counting the hook's words of a pool across both
    /// structs: `PoolConfig` words 0 to 3, then `PoolKey` words at 4 to 6.
    fn index(self) -> u8 {
        match self {
            Self::Config(word) => word as u8,
            Self::Key(word) => POOL_CONFIG_WORDS + word as u8,
        }
    }

    fn from_index(index: u8) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|word| word.index() == index)
    }

    /// The word's slot for the pool whose `PoolConfig` and `PoolKey` start at `config` and `key`.
    fn slot(self, config: &Word, key: &Word) -> Word {
        match self {
            Self::Config(word) => word_at_offset(config, u64::from(word as u8)),
            Self::Key(word) => word_at_offset(key, u64::from(word as u8)),
        }
    }
}

/// Every watched slot of the hook's storage: `pool_id`'s `PoolConfig` words 0 to 2 and its
/// `PoolKey` words 0 to 2.
pub(crate) fn pool_slots(pool_id: &Word) -> Vec<(HookWord, Word)> {
    let config = mapping_slot(pool_id, POOL_CONFIG_MAPPING_SLOT);
    let key = mapping_slot(pool_id, POOL_KEY_MAPPING_SLOT);
    HookWord::ALL
        .into_iter()
        .map(|word| (word, word.slot(&config, &key)))
        .collect()
}

/// The `store_montra_slots` key of slot `slot` of hook `hook`. The key names the hook, so a write
/// is matched to a pool only when the contract that made it is the pool's own hook.
pub(crate) fn slot_store_key(hook: &Address, slot: &Word) -> String {
    format!("slot:0x{}:0x{}", hex::encode(hook), hex::encode(slot))
}

/// The pool and word a watched hook slot belongs to, as `store_montra_slots` holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlotOwner {
    /// The pool's component id, as `map_montra_pools_created` emits it.
    pub(crate) component_id: String,
    pub(crate) word: HookWord,
}

impl SlotOwner {
    /// The store value: `<component id>:<word index>`.
    pub(crate) fn to_store_value(&self) -> String {
        format!("{}:{}", self.component_id, self.word.index())
    }

    /// Reads a value [`SlotOwner::to_store_value`] wrote. Fails when `value` has no `:`, or when
    /// the part after the last `:` is not the index of a [`HookWord`].
    pub(crate) fn from_store_value(value: &str) -> Result<Self> {
        let (component_id, index) = value
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("store_montra_slots value `{value}` has no word index"))?;
        let word = index
            .parse::<u8>()
            .ok()
            .and_then(HookWord::from_index)
            .ok_or_else(|| anyhow!("store_montra_slots value `{value}` has no valid word index"))?;
        Ok(Self { component_id: component_id.to_string(), word })
    }
}

/// Whether the transaction that took `PoolConfig` word 0 from `old_word_0` to `new_word_0` set
/// the pool's `seeded` flag, which `seed` sets and nothing clears.
pub(crate) fn sets_seeded_flag(old_word_0: &Word, new_word_0: &Word) -> bool {
    SEEDED.unsigned(old_word_0) == 0 && SEEDED.unsigned(new_word_0) != 0
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;
    use rstest::rstest;

    use super::*;

    /// Pool 0x35bd0e6a...'s `PoolConfig` at block 1037445, read with `eth_getStorageAt` on the
    /// hook: total 10000, LP 1000, quote currency0, stage 3 since 1793906710, held 327 s.
    const STAGE_3_WORD_0: Word =
        hex!("01476aecd8160301d58802292000000000000001000101000fa00003e8002710");
    /// The same word at block 1037446, after the transaction that switched the LP fee on.
    const STAGE_4_WORD_0: Word =
        hex!("01476aecd8160401d58802292000000000000001000101000fa00003e8002710");
    /// Pool 0x35bd0e6a...'s word 1 from its launch at 1029110: seed range [-887200, 169800].
    const SEED_RANGE_WORD_1: Word =
        hex!("029748f276600000000000000000000000000000000000000000002710002710");
    /// Pool 0x69fee062...'s words at its launch, block 648528: an anti-snipe window of 30 s from
    /// 990000 pips, ending at 1793841826.
    const ANTI_SNIPE_WORD_0: Word =
        hex!("0000000000000001d588022920006aebdaa20101000101000fa00003e8002710");
    const ANTI_SNIPE_WORD_2: Word =
        hex!("0000000000000000000000000000002b89a04b995f1542499d0000001e0f1b30");

    fn u64_bytes(value: u64) -> Vec<u8> {
        value.to_be_bytes().to_vec()
    }

    #[test]
    fn test_word_0_decodes_the_fee_terms_and_the_stage_machine() {
        assert_eq!(
            ConfigWord::Terms.attributes(&STAGE_3_WORD_0),
            vec![
                ("montra_total_fee_pips", u64_bytes(10000)),
                ("montra_lp_fee_pips", u64_bytes(1000)),
                ("montra_quote_is_currency0", vec![1]),
                ("montra_early_buy_fee_until", u64_bytes(0)),
                ("montra_stage", vec![3]),
                ("montra_lp_crossed_at", u64_bytes(1793906710)),
                ("montra_lp_held_seconds", u64_bytes(327)),
            ]
        );
    }

    #[test]
    fn test_word_0_decodes_the_anti_snipe_deadline() {
        let attributes = ConfigWord::Terms.attributes(&ANTI_SNIPE_WORD_0);

        assert_eq!(attributes[3], ("montra_early_buy_fee_until", u64_bytes(1793841826)));
        assert_eq!(attributes[4], ("montra_stage", vec![0]));
    }

    #[test]
    fn test_word_1_decodes_the_seed_range_sign_extended() {
        assert_eq!(
            ConfigWord::SeedRange.attributes(&SEED_RANGE_WORD_1),
            vec![
                ("montra_seed_tick_lower", hex!("fff27660").to_vec()),
                ("montra_seed_tick_upper", hex!("00029748").to_vec()),
            ]
        );
    }

    #[test]
    fn test_word_2_decodes_the_anti_snipe_terms() {
        assert_eq!(
            ConfigWord::EarlyBuyFee.attributes(&ANTI_SNIPE_WORD_2),
            vec![
                ("montra_early_buy_fee_start_pips", u64_bytes(990000)),
                ("montra_early_buy_fee_window_seconds", u64_bytes(30)),
            ]
        );
    }

    #[test]
    fn test_seeded_flag_is_byte_12_of_word_0() {
        // Launch tx 0xcfeabcb8807834089cfbcb83192c137230074ea8492225890851081f289c0149 replayed
        // call by call on an anvil fork of Elysium at 6024400: word 0 after `register`, then
        // after `seed`.
        let registered = hex!("0000000000000000c1c0006e2800000000000000000101000fa0000bb8004e20");
        let seeded = hex!("0000000000000001d58802292000000000000001000101000fa0000bb8004e20");

        assert!(sets_seeded_flag(&[0u8; 32], &seeded));
        assert!(sets_seeded_flag(&registered, &seeded));
        assert!(!sets_seeded_flag(&[0u8; 32], &registered));
        assert!(!sets_seeded_flag(&STAGE_3_WORD_0, &STAGE_4_WORD_0));
    }

    #[test]
    fn test_pool_slots_are_the_config_and_key_words_of_the_pool() {
        let pool_id = hex!("1f3bfa7c7d85bb77019e40018f2d179c04e413d7d853d9c77017a221ac4c2e6a");

        assert_eq!(
            pool_slots(&pool_id),
            vec![
                (
                    HookWord::Config(ConfigWord::Terms),
                    hex!("306fdf70b142ad82d1dc56d41dbe6c099ce69473a61006b145ded747bd98c505")
                ),
                (
                    HookWord::Config(ConfigWord::SeedRange),
                    hex!("306fdf70b142ad82d1dc56d41dbe6c099ce69473a61006b145ded747bd98c506")
                ),
                (
                    HookWord::Config(ConfigWord::EarlyBuyFee),
                    hex!("306fdf70b142ad82d1dc56d41dbe6c099ce69473a61006b145ded747bd98c507")
                ),
                (
                    HookWord::Key(KeyWord::Currency0),
                    hex!("518db8df6301ffbfe5428c38edb1859b34339c4608b623820c352f1b99dfbc06")
                ),
                (
                    HookWord::Key(KeyWord::Currency1),
                    hex!("518db8df6301ffbfe5428c38edb1859b34339c4608b623820c352f1b99dfbc07")
                ),
                (
                    HookWord::Key(KeyWord::FeeTickSpacingHooks),
                    hex!("518db8df6301ffbfe5428c38edb1859b34339c4608b623820c352f1b99dfbc08")
                ),
            ]
        );
    }

    #[rstest]
    #[case::first_config_word(HookWord::Config(ConfigWord::Terms), "0x1f3b:0")]
    #[case::last_config_word(HookWord::Config(ConfigWord::EarlyBuyFee), "0x1f3b:2")]
    #[case::first_key_word(HookWord::Key(KeyWord::Currency0), "0x1f3b:4")]
    #[case::last_key_word(HookWord::Key(KeyWord::FeeTickSpacingHooks), "0x1f3b:6")]
    fn test_slot_owner_store_value_names_the_word_index(
        #[case] word: HookWord,
        #[case] value: &str,
    ) {
        let owner = SlotOwner { component_id: "0x1f3b".to_string(), word };

        assert_eq!(owner.to_store_value(), value);
        assert_eq!(SlotOwner::from_store_value(value).unwrap(), owner);
    }

    #[rstest]
    #[case::no_index("0x1f3b", "store_montra_slots value `0x1f3b` has no word index")]
    #[case::recipient_word(
        "0x1f3b:3",
        "store_montra_slots value `0x1f3b:3` has no valid word index"
    )]
    #[case::index_past_the_key(
        "0x1f3b:7",
        "store_montra_slots value `0x1f3b:7` has no valid word index"
    )]
    #[case::index_not_a_number(
        "0x1f3b:x",
        "store_montra_slots value `0x1f3b:x` has no valid word index"
    )]
    fn test_slot_owner_rejects_a_value_without_a_valid_word(
        #[case] value: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(
            SlotOwner::from_store_value(value)
                .unwrap_err()
                .to_string(),
            expected
        );
    }

    #[test]
    fn test_slot_store_key_is_the_prefixed_lowercase_hook_and_slot() {
        assert_eq!(
            slot_store_key(
                &hex!("6B1216A40B65C83E9C9E18A562C446697B8CF8EC"),
                &hex!("306FDF70B142AD82D1DC56D41DBE6C099CE69473A61006B145DED747BD98C505")
            ),
            "slot:0x6b1216a40b65c83e9c9e18a562c446697b8cf8ec:\
             0x306fdf70b142ad82d1dc56d41dbe6c099ce69473a61006b145ded747bd98c505"
        );
    }
}
