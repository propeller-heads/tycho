//! The MEVX module (`MevxStorage`).
//!
//! With a router set, the plugin's `afterSwap` asks it for a backrun, which costs gas on every
//! swap. The router's address is all the simulation reads.

use hex_literal::hex;

use crate::plugin::{modules::PluginModule, storage::StorageField};

/// `MevxStorage.NAMESPACE`, where `Layout.mevxRouter` sits.
const ROUTER_SLOT: [u8; 32] =
    hex!("0265d00335a0dde512278a67fe4cbfe91190506046edc3e7f5f678ec695c6500");

pub const MODULE: PluginModule =
    PluginModule { name: "mevx", fields: &[StorageField::new("mevx/router", ROUTER_SLOT, 0, 20)] };

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::StorageChange;

    use super::*;
    use crate::plugin::attributes::creation_attributes;

    /// `mevxRouter` of plugin 0xb0918bddea10fcc127bbe816725b2be536cebb8a on Robinhood at block
    /// 118127446.
    #[test]
    fn decodes_a_deployed_router() {
        let write = StorageChange {
            key: ROUTER_SLOT.to_vec(),
            new_value: hex!("000000000000000000000000011242bf9f5c4832f152be5a5838daae40e0405c")
                .to_vec(),
            ..Default::default()
        };

        let attributes = creation_attributes(&[write], &[&MODULE]);

        let decoded: Vec<(&str, &[u8])> = attributes
            .iter()
            .map(|attribute| (attribute.name.as_str(), attribute.value.as_slice()))
            .collect();
        assert_eq!(
            decoded,
            [("mevx/router", hex!("011242bf9f5c4832f152be5a5838daae40e0405c").as_slice())]
        );
    }
}
