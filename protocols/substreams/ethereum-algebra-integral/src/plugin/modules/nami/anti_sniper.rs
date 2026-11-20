//! Nami's AntiSniper module (`AntiSniperStorage`).
//!
//! Only what decides when the module stops charging is indexed: its starting fee, the length of
//! its period and when the period started. The fee's curve, its receiver and the exempt senders
//! are not, because the simulation does not quote a pool while the module charges.

use hex_literal::hex;

use crate::plugin::{modules::PluginModule, storage::StorageField};

/// `AntiSniperStorage.NAMESPACE`, where `Layout.config` sits: `feeReceiver` in the low 20
/// bytes, then `period`, `startingFee`, `disabledPriorStart`, `easeIn` and `easeOut`.
const CONFIG_SLOT: [u8; 32] =
    hex!("357b3a127c4f79e0ebf0d5870644df9091bce82e8475ae045079b03838a13b00");
/// `Layout.startTimestamp`, the word after.
const START_SLOT: [u8; 32] =
    hex!("357b3a127c4f79e0ebf0d5870644df9091bce82e8475ae045079b03838a13b01");

pub const MODULE: PluginModule = PluginModule {
    name: "anti_sniper",
    fields: &[
        // uint32, in seconds.
        StorageField::new("anti_sniper/period", CONFIG_SLOT, 20, 4),
        // uint24, in hundredths of a bip; zero disables the module.
        StorageField::new("anti_sniper/starting_fee", CONFIG_SLOT, 24, 3),
        // uint64; zero until the period is started.
        StorageField::new("anti_sniper/start_timestamp", START_SLOT, 0, 8),
    ],
};

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::StorageChange;

    use super::*;
    use crate::plugin::attributes::creation_attributes;

    /// The two words of plugin 0x53ce1d109b8ffe35010baaac91a3d839f4b05fea on Base at block
    /// 59132137: a 30 minute period from 12 November 2026 16:00 UTC, starting at 83.2986%.
    #[test]
    fn decodes_a_deployed_anti_sniper() {
        let writes = [
            StorageChange {
                key: CONFIG_SLOT.to_vec(),
                new_value: hex!("076c0bb8010cb5da000007087b244bcbce1ca6241263a28cbd186df10135f0b1")
                    .to_vec(),
                ..Default::default()
            },
            StorageChange {
                key: START_SLOT.to_vec(),
                new_value: hex!("000000000000000000000000000000000000000000000000000000006af5e280")
                    .to_vec(),
                ..Default::default()
            },
        ];

        let attributes = creation_attributes(&writes, &[&MODULE]);

        let decoded: Vec<(&str, &[u8])> = attributes
            .iter()
            .map(|attribute| (attribute.name.as_str(), attribute.value.as_slice()))
            .collect();
        assert_eq!(
            decoded,
            [
                ("anti_sniper/period", hex!("00000708").as_slice()),
                ("anti_sniper/starting_fee", hex!("0cb5da").as_slice()),
                ("anti_sniper/start_timestamp", hex!("000000006af5e280").as_slice()),
            ]
        );
    }
}
