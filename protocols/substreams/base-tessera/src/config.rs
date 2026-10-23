use alloy_primitives::Address;
use anyhow::{Context, Result};
use serde::Deserialize;

/// The contracts of one Tessera deployment.
#[derive(Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentConfig {
    /// The entrypoint that settles every swap.
    pub tesseraswap: Address,
    /// The Engine that discovery and state indexing follow.
    pub engine: Address,
    /// The treasury before TesseraSwap's first treasury write.
    pub treasury: Address,
}
impl DeploymentConfig {
    pub fn parse(params: &str) -> Result<Self> {
        serde_qs::from_str(params).with_context(|| format!("invalid deployment params {params:?}"))
    }
}

/// A deployment of distinct placeholder addresses, for tests.
#[cfg(test)]
pub(crate) const TEST_CONFIG: DeploymentConfig = DeploymentConfig {
    tesseraswap: Address::repeat_byte(0x11),
    engine: Address::repeat_byte(0x22),
    treasury: Address::repeat_byte(0x33),
};

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const PARAMS: &str = "tesseraswap=55555522005bcae1c2424d474bfd5ed477749e3e\
        &engine=0x31e99e05fee3dce580af777c3fd63ee1b3b40c17\
        &treasury=3dbe077e7986657e95e1cc50089f17a5a4af0aae";

    #[test]
    fn parses_each_address_with_or_without_prefix() {
        assert_eq!(
            DeploymentConfig::parse(PARAMS).unwrap(),
            DeploymentConfig {
                tesseraswap: address!("55555522005bcae1c2424d474bfd5ed477749e3e"),
                engine: address!("31e99e05fee3dce580af777c3fd63ee1b3b40c17"),
                treasury: address!("3dbe077e7986657e95e1cc50089f17a5a4af0aae"),
            }
        );
    }

    #[test]
    fn rejects_an_address_that_is_not_20_bytes() {
        let short = PARAMS.replace(
            "&treasury=3dbe077e7986657e95e1cc50089f17a5a4af0aae",
            "&treasury=3dbe077e7986657e95e1cc50089f17a5a4af0a",
        );
        assert!(DeploymentConfig::parse(&short).is_err());
    }

    #[test]
    fn rejects_a_missing_address() {
        let missing = PARAMS.replace("&treasury=3dbe077e7986657e95e1cc50089f17a5a4af0aae", "");
        assert!(DeploymentConfig::parse(&missing).is_err());
    }

    #[test]
    fn rejects_a_layout_param() {
        assert!(DeploymentConfig::parse(&format!("{PARAMS}&treasury_slot=1")).is_err());
    }
}
