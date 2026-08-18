use anyhow::anyhow;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct StringParams {
    planner: String,
    info: String,
    extra_impl1: String,
    extra_impl2: String,
}

impl StringParams {
    pub fn parse(input: &str) -> anyhow::Result<Self> {
        serde_qs::from_str(input).map_err(|e| anyhow!("Failed to parse query params: {}", e))
    }
}

pub(crate) struct Params {
    pub planner: Vec<u8>,
    pub info: Vec<u8>,
    pub extra_impl1: Vec<u8>,
    pub extra_impl2: Vec<u8>,
}

pub fn encode_addr(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

pub fn decode_addr(s: &str) -> anyhow::Result<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() != 40 {
        return Err(anyhow!("address must be 20 bytes (40 hex chars), got len={}", s.len()));
    }
    let bytes = hex::decode(s)?;
    if bytes.len() != 20 {
        return Err(anyhow!("decoded address is not 20 bytes"));
    }
    Ok(bytes)
}

pub fn encode_addrs(bytes: &[Vec<u8>]) -> String {
    bytes
        .iter()
        .map(|b| encode_addr(b))
        .collect::<Vec<_>>()
        .join(",")
}

pub fn decode_addrs(s: &str) -> anyhow::Result<Vec<Vec<u8>>> {
    let parts: Vec<&str> = s.split(',').collect();
    let mut decoded = Vec::new();
    for part in parts {
        decoded.push(decode_addr(part.trim())?);
    }
    Ok(decoded)
}

impl Params {
    pub fn parse(input: &str) -> anyhow::Result<Self> {
        let params = StringParams::parse(input)?;

        Ok(Self {
            planner: decode_addr(&params.planner)?,
            info: decode_addr(&params.info)?,
            extra_impl1: decode_addr(&params.extra_impl1)?,
            extra_impl2: decode_addr(&params.extra_impl2)?,
        })
    }
}
