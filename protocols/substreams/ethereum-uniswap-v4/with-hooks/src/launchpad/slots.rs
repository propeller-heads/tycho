//! The slots `store_launchpad_slots` watches and the values it keeps for them: which pool and
//! which word of its hook's storage a slot holds, for both hook layouts.

use anyhow::{anyhow, Result};

use crate::{
    address_list::Address,
    ascend::{self, AscendWord},
    launchpad::Layout,
    montra::pool_config::{self, ConfigWord, KeyWord, POOL_CONFIG_WORDS},
    storage::{StateWord, Word},
};

/// A word of a pool's hook storage this package watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HookWord {
    Config(ConfigWord),
    Key(KeyWord),
    /// A word of an Ascend hook's per-pool storage ([`crate::ascend`]).
    Ascend(AscendWord),
}

/// The index of the first Ascend word in the store value, after `PoolConfig` words 0 to 3 and the
/// `PoolKey` words.
const ASCEND_WORD_BASE: u8 = POOL_CONFIG_WORDS + KeyWord::ALL.len() as u8;

impl HookWord {
    /// The word's index in the store value, counting the hook's words of a pool across its
    /// structs: `PoolConfig` words 0 to 3, then `PoolKey` words at 4 to 6, then an Ascend pool's
    /// words at 7 to 11.
    fn index(self) -> u8 {
        match self {
            Self::Config(word) => word as u8,
            Self::Key(word) => POOL_CONFIG_WORDS + word as u8,
            Self::Ascend(word) => ASCEND_WORD_BASE + word as u8,
        }
    }

    fn from_index(index: u8) -> Option<Self> {
        ConfigWord::ALL
            .iter()
            .copied()
            .map(Self::Config)
            .chain(KeyWord::ALL.map(Self::Key))
            .chain(
                AscendWord::ALL
                    .iter()
                    .copied()
                    .map(Self::Ascend),
            )
            .find(|word| word.index() == index)
    }
}

/// Every watched slot of `pool_id` in the storage of a hook of `layout`, each with the word it
/// holds.
pub(crate) fn pool_slots(layout: Layout, pool_id: &Word) -> Vec<(HookWord, Word)> {
    match layout {
        Layout::Montra => pool_config::pool_slots(pool_id),
        Layout::Ascend => ascend::pool_slots(pool_id)
            .into_iter()
            .map(|(word, slot)| (HookWord::Ascend(word), slot))
            .collect(),
    }
}

/// The `store_launchpad_slots` key of slot `slot` of hook `hook`. The key names the hook, so a
/// write is matched to a pool only when the contract that made it is the pool's own hook.
pub(crate) fn slot_store_key(hook: &Address, slot: &Word) -> String {
    format!("slot:0x{}:0x{}", hex::encode(hook), hex::encode(slot))
}

/// The pool and word a watched hook slot belongs to, as `store_launchpad_slots` holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlotOwner {
    /// The pool's component id, as `map_launchpad_pools_created` emits it.
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
            .ok_or_else(|| anyhow!("store_launchpad_slots value `{value}` has no word index"))?;
        let word = index
            .parse::<u8>()
            .ok()
            .and_then(HookWord::from_index)
            .ok_or_else(|| {
                anyhow!("store_launchpad_slots value `{value}` has no valid word index")
            })?;
        Ok(Self { component_id: component_id.to_string(), word })
    }
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::first_config_word(HookWord::Config(ConfigWord::Terms), "0x1f3b:0")]
    #[case::last_config_word(HookWord::Config(ConfigWord::EarlyBuyFee), "0x1f3b:2")]
    #[case::first_key_word(HookWord::Key(KeyWord::Currency0), "0x1f3b:4")]
    #[case::last_key_word(HookWord::Key(KeyWord::FeeTickSpacingHooks), "0x1f3b:6")]
    #[case::first_ascend_word(HookWord::Ascend(AscendWord::Terms), "0x1f3b:7")]
    #[case::last_ascend_word(HookWord::Ascend(AscendWord::Window), "0x1f3b:11")]
    fn test_slot_owner_store_value_names_the_word_index(
        #[case] word: HookWord,
        #[case] value: &str,
    ) {
        let owner = SlotOwner { component_id: "0x1f3b".to_string(), word };

        assert_eq!(owner.to_store_value(), value);
        assert_eq!(SlotOwner::from_store_value(value).unwrap(), owner);
    }

    #[rstest]
    #[case::no_index("0x1f3b", "store_launchpad_slots value `0x1f3b` has no word index")]
    #[case::recipient_word(
        "0x1f3b:3",
        "store_launchpad_slots value `0x1f3b:3` has no valid word index"
    )]
    #[case::index_past_the_ascend_words(
        "0x1f3b:12",
        "store_launchpad_slots value `0x1f3b:12` has no valid word index"
    )]
    #[case::index_not_a_number(
        "0x1f3b:x",
        "store_launchpad_slots value `0x1f3b:x` has no valid word index"
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
