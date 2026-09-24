use anyhow::{ensure, Result};
use serde::Deserialize;

#[derive(Clone, Deserialize)]
pub struct DeploymentConfig {
    #[serde(with = "hex::serde")]
    pub tesseraswap: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub engine: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub treasury: Vec<u8>,
    pub treasury_slot: u64,
    pub pair_map_slot: u64,
    pub pair_base_token_slot: u64,
    pub pair_quote_token_slot: u64,
    pub pair_lib_slot: u64,
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
