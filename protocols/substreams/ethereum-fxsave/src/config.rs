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
    /// Block and transaction the component is created in. DCI traces its entrypoints there.
    pub creation_block: u64,
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
    use super::*;

    pub(crate) const PARAMS: &str = "fxsave=0x7743e50f534a7f9f1791dde7dcd89f7783eefc39&\
         fx_base_pool=0x65c9a641afceb9c0e6034e558a319488fa0fa3be&\
         fxusd=0x085780639cc2cacd35e474e71f4d000e2405d8f6&\
         usdc=0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48&\
         creation_block=26349181&\
         creation_tx=0x0763603bd2699b43a9ecdf3de8ceed345a98c5571cc4a98375cc4ce0c266b84d";

    #[test]
    fn parses_params_and_formats_component_id() {
        let config = Config::parse(PARAMS).expect("valid params");

        assert_eq!(config.component_id(), "0x7743e50f534a7f9f1791dde7dcd89f7783eefc39");
        assert_eq!(config.creation_block, 26_349_181);
    }

    #[test]
    fn rejects_params_missing_a_field() {
        assert!(Config::parse("fxsave=0x7743e50f534a7f9f1791dde7dcd89f7783eefc39").is_err());
    }
}
