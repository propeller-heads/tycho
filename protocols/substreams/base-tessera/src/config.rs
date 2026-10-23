use alloy_primitives::Address;
use anyhow::{Context, Result};
use serde::Deserialize;

/// The contracts of one Tessera deployment.
#[derive(Clone, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;

    const PARAMS: &str = "tesseraswap=55555522005bcae1c2424d474bfd5ed477749e3e\
        &engine=0x31e99e05fee3dce580af777c3fd63ee1b3b40c17\
        &treasury=3dbe077e7986657e95e1cc50089f17a5a4af0aae";

    #[test]
    fn parses_addresses_with_or_without_prefix() {
        let config = DeploymentConfig::parse(PARAMS).unwrap();
        assert_eq!(
            config.engine,
            alloy_primitives::address!("31e99e05fee3dce580af777c3fd63ee1b3b40c17")
        );
    }

    #[test]
    fn rejects_a_layout_param() {
        assert!(DeploymentConfig::parse(&format!("{PARAMS}&treasury_slot=1")).is_err());
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
