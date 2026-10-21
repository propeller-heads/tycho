use anyhow::{anyhow, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Params {
    pub factory: String,
    pub quoter: String,
}

impl Params {
    pub fn parse_from_query(input: &str) -> Result<Self> {
        serde_qs::from_str(input).map_err(|e| anyhow!("Failed to parse query params: {}", e))
    }

    pub fn decode_addresses(&self) -> Result<([u8; 20], [u8; 20])> {
        let factory =
            hex::decode(&self.factory).map_err(|e| anyhow!("Invalid factory hex: {}", e))?;
        let quoter = hex::decode(&self.quoter).map_err(|e| anyhow!("Invalid quoter hex: {}", e))?;

        if factory.len() != 20 || quoter.len() != 20 {
            return Err(anyhow!("Addresses must be 20 bytes"));
        }

        Ok((factory.try_into().unwrap(), quoter.try_into().unwrap()))
    }
}
