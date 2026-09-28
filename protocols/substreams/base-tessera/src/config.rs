use anyhow::{ensure, Result};
use serde::Deserialize;

/// Addresses and layout parameters. Slot numbers below describe the current Base deployment.
#[derive(Clone, Deserialize)]
pub struct DeploymentConfig {
    #[serde(with = "hex::serde")]
    pub tesseraswap: Vec<u8>,
    #[serde(with = "hex::serde")]
    /// Fixed Engine deployment used for discovery and raw state indexing.
    pub engine: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub treasury: Vec<u8>,
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
        let config: Self = serde_qs::from_str(params)?;
        ensure!(
            [&config.tesseraswap, &config.engine, &config.treasury]
                .iter()
                .all(|a| a.len() == 20),
            "addresses must be 20 bytes"
        );
        Ok(config)
    }
}
