use serde::Deserialize;

/// Per-chain deployment addresses, hex without the 0x prefix.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Params {
    /// `BinPoolManager`, the emitter of `Initialize`.
    pool_manager: String,
    /// Holds every pool's funds, so it is `balance_owner` on every component.
    vault: String,
}

/// Decodes a 20-byte address given as hex without the `0x` prefix.
pub(crate) fn decode_address(
    name: &str,
    value: &str,
) -> Result<Vec<u8>, substreams::errors::Error> {
    let bytes =
        hex::decode(value).map_err(|err| anyhow::anyhow!("Invalid {name} {value:?}: {err}"))?;
    if bytes.len() != 20 {
        return Err(anyhow::anyhow!(
            "Invalid {name} {value:?}: expected 20 bytes, got {}",
            bytes.len()
        ));
    }
    Ok(bytes)
}
