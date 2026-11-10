use alloy_primitives::{Address, B256};
use anyhow::{Context, Result};
use serde::Deserialize;

/// Deployment-specific configuration passed through substreams params.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// fxSAVE (`SavingFxUSD`), the ERC4626 vault over fxSP. Its address is the component id.
    pub fxsave: Address,
    /// fxSP (`FxUSDBasePool`), the stability pool share fxSAVE holds.
    pub fx_base_pool: Address,
    /// fxSP's yield token, deposited at $1.
    pub fxusd: Address,
    /// fxSP's stable token, deposited at its Chainlink price.
    pub usdc: Address,
    /// Block the component is created in. Its balances are seeded there by `eth_call`, and storage
    /// writes only update them in later blocks.
    pub creation_block: u64,
    /// Transaction in `creation_block` that carries the component, its entrypoints and its seeded
    /// balances.
    pub creation_tx: B256,
}

impl Config {
    pub fn parse(params: &str) -> Result<Self> {
        serde_qs::from_str(params).context("parsing substreams params")
    }

    pub fn component_id(&self) -> String {
        format!("{:#x}", self.fxsave)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use alloy_primitives::{address, b256};
    use itertools::Itertools;
    use rstest::rstest;

    use super::*;

    pub(crate) const PARAMS: &str = "fxsave=0x7743e50f534a7f9f1791dde7dcd89f7783eefc39&\
         fx_base_pool=0x65c9a641afceb9c0e6034e558a319488fa0fa3be&\
         fxusd=0x085780639cc2cacd35e474e71f4d000e2405d8f6&\
         usdc=0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48&\
         creation_block=26349181&\
         creation_tx=0x0763603bd2699b43a9ecdf3de8ceed345a98c5571cc4a98375cc4ce0c266b84d";

    #[test]
    fn test_parses_every_field() {
        let config = Config::parse(PARAMS).expect("valid params");

        assert_eq!(config.fxsave, address!("7743e50f534a7f9f1791dde7dcd89f7783eefc39"));
        assert_eq!(config.fx_base_pool, address!("65c9a641afceb9c0e6034e558a319488fa0fa3be"));
        assert_eq!(config.fxusd, address!("085780639cc2cacd35e474e71f4d000e2405d8f6"));
        assert_eq!(config.usdc, address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"));
        assert_eq!(config.creation_block, 26_349_181);
        assert_eq!(
            config.creation_tx,
            b256!("0763603bd2699b43a9ecdf3de8ceed345a98c5571cc4a98375cc4ce0c266b84d")
        );
        assert_eq!(config.component_id(), "0x7743e50f534a7f9f1791dde7dcd89f7783eefc39");
    }

    #[rstest]
    #[case::fxsave("fxsave")]
    #[case::fx_base_pool("fx_base_pool")]
    #[case::fxusd("fxusd")]
    #[case::usdc("usdc")]
    #[case::creation_block("creation_block")]
    #[case::creation_tx("creation_tx")]
    fn test_missing_field_is_named(#[case] field: &str) {
        let params = PARAMS
            .split('&')
            .filter(|pair| !pair.starts_with(&format!("{field}=")))
            .join("&");

        let error = Config::parse(&params).unwrap_err();

        assert!(format!("{error:#}").contains(&format!("missing field `{field}`")), "{error:#}");
    }
}
