use anyhow::{anyhow, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Params {
    pub factory_address: String,
}

impl Params {
    pub fn parse_from_query(input: &str) -> Result<Self> {
        serde_qs::from_str(input).map_err(|e| anyhow!("Failed to parse query params: {}", e))
    }

    pub fn decode_addresses(&self) -> Result<[u8; 20]> {
        let factory_address = hex::decode(&self.factory_address)
            .map_err(|e| anyhow!("Invalid factory address hex: {}", e))?;

        if factory_address.len() != 20 {
            return Err(anyhow!("factory address must be 20 bytes"));
        }

        Ok(factory_address.try_into().unwrap())
    }
}
