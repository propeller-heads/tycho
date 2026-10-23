use alloy_primitives::Address;
use anyhow::{Context, Result};
use serde::Deserialize;

/// Addresses and layout parameters. Slot numbers below describe the current Base deployment.
#[derive(Clone, Deserialize)]
pub struct DeploymentConfig {
    pub tesseraswap: Address,
    /// Fixed Engine deployment used for discovery and raw state indexing.
    pub engine: Address,
    pub treasury: Address,
    /// TesseraSwap slot 1: current treasury/custodian; tracked as balance_owner.
    pub treasury_slot: u64,
    /// Engine mapping base slot 8: sorted token-pair key to registered Pair address.
    pub pair_map_slot: u64,
    /// Pair slot 48: base-token address, used to identify a newly initialized Pair.
    pub pair_base_token_slot: u64,
    /// Pair slot 49: packed quote-token/decimals word; the address is in the low 20 bytes.
    pub pair_quote_token_slot: u64,
    /// Pair slot 51: pricing-library address, published as stateless_contract_addr_1.
    pub pair_lib_slot: u64,
    /// Pair slot 52: write-helper address, published as stateless_contract_addr_2.
    pub pair_write_helper_slot: u64,
}
impl DeploymentConfig {
    pub fn parse(params: &str) -> Result<Self> {
        serde_qs::from_str(params).with_context(|| format!("invalid deployment params {params:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARAMS: &str = "tesseraswap=55555522005bcae1c2424d474bfd5ed477749e3e\
        &engine=0x31e99e05fee3dce580af777c3fd63ee1b3b40c17\
        &treasury=3dbe077e7986657e95e1cc50089f17a5a4af0aae\
        &treasury_slot=1&pair_map_slot=8&pair_base_token_slot=48&pair_quote_token_slot=49\
        &pair_lib_slot=51&pair_write_helper_slot=52";

    #[test]
    fn parses_addresses_with_or_without_prefix() {
        let config = DeploymentConfig::parse(PARAMS).unwrap();
        assert_eq!(
            config.engine,
            alloy_primitives::address!("31e99e05fee3dce580af777c3fd63ee1b3b40c17")
        );
    }

    #[test]
    fn rejects_an_address_that_is_not_20_bytes() {
        let short = PARAMS.replace(
            "3dbe077e7986657e95e1cc50089f17a5a4af0aae",
            "3dbe077e7986657e95e1cc50089f17a5a4af0a",
        );
        assert!(DeploymentConfig::parse(&short).is_err());
    }
}
