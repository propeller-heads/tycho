use alloy_primitives::{Address, B256};
use anyhow::{Context, Result};
use serde::Deserialize;

/// Deployment-specific configuration passed through substreams params.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// DssLitePsm (DAI<->USDC).
    pub psm: Address,
    pub psm_creation_block: u64,
    pub psm_creation_tx: B256,
    /// USDC custody address of the PSM (`pocket()`, an immutable constructor param).
    pub pocket: Address,

    /// UsdsPsmWrapper (USDS<->USDC). Stateless proxy over the PSM.
    pub wrapper: Address,
    pub wrapper_creation_block: u64,
    pub wrapper_creation_tx: B256,

    /// DaiUsds converter (DAI<->USDS, 1:1, mint/burn - no reserves). Balances are
    /// the join escrows (`vat.dai[join]`), the exact convertibility bound per side.
    pub converter: Address,
    pub converter_creation_block: u64,
    pub converter_creation_tx: B256,
    /// Maker core engine, holding the joins' internal dai escrows.
    pub vat: Address,
    /// DAI adapter: its vat escrow bounds DAI -> USDS conversions.
    pub dai_join: Address,
    /// USDS adapter: its vat escrow bounds USDS -> DAI conversions.
    pub usds_join: Address,

    pub dai: Address,
    pub usdc: Address,
    pub usds: Address,
}

impl Config {
    pub fn parse(params: &str) -> Result<Self> {
        serde_qs::from_str(params).context("parsing substreams params")
    }

    pub fn psm_component_id(&self) -> String {
        format!("{:#x}", self.psm)
    }

    pub fn wrapper_component_id(&self) -> String {
        format!("{:#x}", self.wrapper)
    }

    pub fn converter_component_id(&self) -> String {
        format!("{:#x}", self.converter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_params_and_formats_component_ids() {
        let config = Config::parse(
            "psm=0xf6e72db5454dd049d0788e411b06cfaf16853042&\
             psm_creation_block=20283666&\
             psm_creation_tx=0x61e5d04f14d1fea9c505fb4dc9b6cf6e97bc83f2076b53cb7e92d0a2e88b6bbd&\
             pocket=0x37305b1cd40574e4c5ce33f8e8306be057fd7341&\
             wrapper=0xa188eec8f81263234da3622a406892f3d630f98c&\
             wrapper_creation_block=20668728&\
             wrapper_creation_tx=0x43ddae74123936f6737b78fcf785547f7f6b7b27e280fe7fbf98c81b3c018585&\
             converter=0x3225737a9bbb6473cb4a45b7244aca2befdb276a&\
             converter_creation_block=20663734&\
             converter_creation_tx=0xb63d6f4cfb9945130ab32d914aaaafbad956be3718176771467b4154f9afab61&\
             vat=0x35d1b3f3d7966a1dfe207aa4514c12a259a0492b&\
             dai_join=0x9759a6ac90977b93b58547b4a71c78317f391a28&\
             usds_join=0x3c0f895007ca717aa01c8693e59df1e8c3777feb&\
             dai=0x6b175474e89094c44da98b954eedeac495271d0f&\
             usdc=0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48&\
             usds=0xdc035d45d973e3ec169d2276ddab16f1e407384f",
        )
        .expect("valid params");
        assert_eq!(config.psm_component_id(), "0xf6e72db5454dd049d0788e411b06cfaf16853042");
        assert_eq!(config.psm_creation_block, 20283666);
        assert_eq!(
            config.usds.to_string().to_lowercase(),
            "0xdc035d45d973e3ec169d2276ddab16f1e407384f"
        );
    }
}
