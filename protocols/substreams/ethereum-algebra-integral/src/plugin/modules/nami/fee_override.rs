//! Nami's fee override module (`FeeOverrideStorage`).
//!
//! The first word holds the bounds, the per-direction premiums, the keeper's live override and
//! the fee source flag. The fee source's address and the fee per source segment follow in the
//! next two words.

use hex_literal::hex;

use crate::plugin::{modules::PluginModule, storage::StorageField};

/// `FeeOverrideStorage.NAMESPACE`, the first word of `Layout`.
const CONFIG_SLOT: [u8; 32] =
    hex!("4e6a83909802af262770f859c245d2a01fddbd871c0bec861e01a093143bd600");
/// `Layout.source`, the word after.
const SOURCE_SLOT: [u8; 32] =
    hex!("4e6a83909802af262770f859c245d2a01fddbd871c0bec861e01a093143bd601");
/// `Layout.segmentFees`: sixteen `uint16`, segment `id` at bit `16 * id`.
const SEGMENT_FEES_SLOT: [u8; 32] =
    hex!("4e6a83909802af262770f859c245d2a01fddbd871c0bec861e01a093143bd602");

pub const MODULE: PluginModule = PluginModule {
    name: "fee_override",
    fields: &[
        // `Layout`'s first word, in declaration order from the low end.
        StorageField::new("fee_override/floor", CONFIG_SLOT, 0, 2),
        StorageField::new("fee_override/cap", CONFIG_SLOT, 2, 2),
        StorageField::new("fee_override/premium_zero_for_one", CONFIG_SLOT, 4, 2),
        StorageField::new("fee_override/premium_one_for_zero", CONFIG_SLOT, 6, 2),
        StorageField::new("fee_override/decay_window", CONFIG_SLOT, 8, 4),
        StorageField::new("fee_override/fee_zero_for_one", CONFIG_SLOT, 12, 2),
        StorageField::new("fee_override/fee_one_for_zero", CONFIG_SLOT, 14, 2),
        // uint40.
        StorageField::new("fee_override/expires_at", CONFIG_SLOT, 16, 5),
        // bool.
        StorageField::new("fee_override/source_enabled", CONFIG_SLOT, 21, 1),
        StorageField::new("fee_override/max_discount_bps", CONFIG_SLOT, 22, 2),
        StorageField::new("fee_override/source", SOURCE_SLOT, 0, 20),
        StorageField::new("fee_override/segment_fee/0", SEGMENT_FEES_SLOT, 0, 2),
        StorageField::new("fee_override/segment_fee/1", SEGMENT_FEES_SLOT, 2, 2),
        StorageField::new("fee_override/segment_fee/2", SEGMENT_FEES_SLOT, 4, 2),
        StorageField::new("fee_override/segment_fee/3", SEGMENT_FEES_SLOT, 6, 2),
        StorageField::new("fee_override/segment_fee/4", SEGMENT_FEES_SLOT, 8, 2),
        StorageField::new("fee_override/segment_fee/5", SEGMENT_FEES_SLOT, 10, 2),
        StorageField::new("fee_override/segment_fee/6", SEGMENT_FEES_SLOT, 12, 2),
        StorageField::new("fee_override/segment_fee/7", SEGMENT_FEES_SLOT, 14, 2),
        StorageField::new("fee_override/segment_fee/8", SEGMENT_FEES_SLOT, 16, 2),
        StorageField::new("fee_override/segment_fee/9", SEGMENT_FEES_SLOT, 18, 2),
        StorageField::new("fee_override/segment_fee/10", SEGMENT_FEES_SLOT, 20, 2),
        StorageField::new("fee_override/segment_fee/11", SEGMENT_FEES_SLOT, 22, 2),
        StorageField::new("fee_override/segment_fee/12", SEGMENT_FEES_SLOT, 24, 2),
        StorageField::new("fee_override/segment_fee/13", SEGMENT_FEES_SLOT, 26, 2),
        StorageField::new("fee_override/segment_fee/14", SEGMENT_FEES_SLOT, 28, 2),
        StorageField::new("fee_override/segment_fee/15", SEGMENT_FEES_SLOT, 30, 2),
    ],
};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use substreams_ethereum::pb::eth::v2::StorageChange;

    use super::*;
    use crate::plugin::attributes::creation_attributes;

    fn write(slot: [u8; 32], word: [u8; 32]) -> StorageChange {
        StorageChange { key: slot.to_vec(), new_value: word.to_vec(), ..Default::default() }
    }

    /// The three words of plugin 0xb0918bddea10fcc127bbe816725b2be536cebb8a on Robinhood at
    /// block 118127446: floor 1000, cap 20000, a four hour decay window, the NYSE fee source
    /// enabled and a fee for each of its eight segments.
    #[test]
    fn decodes_a_deployed_fee_override() {
        let writes = [
            write(
                CONFIG_SLOT,
                hex!("000000000000000000000100000000000000000000003840000000004e2003e8"),
            ),
            write(
                SOURCE_SLOT,
                hex!("000000000000000000000000622b1971f9792a2d410b14b8108d5481c1d2c362"),
            ),
            write(
                SEGMENT_FEES_SLOT,
                hex!("000000000000000000000000000000000bb804b006400bb809600578044c03e8"),
            ),
        ];

        let attributes = creation_attributes(&writes, &[&MODULE]);

        let decoded: HashMap<&str, &[u8]> = attributes
            .iter()
            .map(|attribute| (attribute.name.as_str(), attribute.value.as_slice()))
            .collect();
        assert_eq!(decoded.len(), MODULE.fields.len());
        let expected: [(&str, &[u8]); 27] = [
            ("fee_override/floor", &hex!("03e8")),
            ("fee_override/cap", &hex!("4e20")),
            ("fee_override/premium_zero_for_one", &hex!("0000")),
            ("fee_override/premium_one_for_zero", &hex!("0000")),
            ("fee_override/decay_window", &hex!("00003840")),
            ("fee_override/fee_zero_for_one", &hex!("0000")),
            ("fee_override/fee_one_for_zero", &hex!("0000")),
            ("fee_override/expires_at", &hex!("0000000000")),
            ("fee_override/source_enabled", &hex!("01")),
            ("fee_override/max_discount_bps", &hex!("0000")),
            ("fee_override/source", &hex!("622b1971f9792a2d410b14b8108d5481c1d2c362")),
            ("fee_override/segment_fee/0", &hex!("03e8")),
            ("fee_override/segment_fee/1", &hex!("044c")),
            ("fee_override/segment_fee/2", &hex!("0578")),
            ("fee_override/segment_fee/3", &hex!("0960")),
            ("fee_override/segment_fee/4", &hex!("0bb8")),
            ("fee_override/segment_fee/5", &hex!("0640")),
            ("fee_override/segment_fee/6", &hex!("04b0")),
            ("fee_override/segment_fee/7", &hex!("0bb8")),
            ("fee_override/segment_fee/8", &hex!("0000")),
            ("fee_override/segment_fee/9", &hex!("0000")),
            ("fee_override/segment_fee/10", &hex!("0000")),
            ("fee_override/segment_fee/11", &hex!("0000")),
            ("fee_override/segment_fee/12", &hex!("0000")),
            ("fee_override/segment_fee/13", &hex!("0000")),
            ("fee_override/segment_fee/14", &hex!("0000")),
            ("fee_override/segment_fee/15", &hex!("0000")),
        ];
        for (name, value) in expected {
            assert_eq!(decoded.get(name), Some(&value), "{name}");
        }
    }
}
