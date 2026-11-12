//! The `params` string a package's pool-created module takes from its manifest.

use anyhow::anyhow;
use serde::Deserialize;

/// Per-chain deployment addresses, hex without the `0x` prefix, as the manifest passes them.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct RawDeployment {
    pool_manager: String,
    vault: String,
}

/// The two Infinity contracts a package indexes against, as 20-byte addresses.
#[derive(Debug, PartialEq, Eq)]
pub struct Deployment {
    /// The pool manager, the emitter of `Initialize`.
    pub pool_manager: Vec<u8>,
    /// Holds every pool's funds, so it is `balance_owner` on every component.
    pub vault: Vec<u8>,
}

impl Deployment {
    /// Parses `pool_manager=<hex>&vault=<hex>`. Errors name `module`, the handler the string was
    /// passed to, on a missing or unknown key or an address that is not 20 bytes of hex.
    pub fn parse(module: &str, params: &str) -> Result<Self, anyhow::Error> {
        let raw: RawDeployment = serde_qs::from_str(params)
            .map_err(|err| anyhow!("Invalid {module} params {params:?}: {err}"))?;
        Ok(Self {
            pool_manager: decode_address("pool_manager", &raw.pool_manager)?,
            vault: decode_address("vault", &raw.vault)?,
        })
    }
}

/// Decodes a 20-byte address given as hex without the `0x` prefix; `name` is for the error.
pub fn decode_address(name: &str, value: &str) -> Result<Vec<u8>, anyhow::Error> {
    let bytes = hex::decode(value).map_err(|err| anyhow!("Invalid {name} {value:?}: {err}"))?;
    if bytes.len() != 20 {
        return Err(anyhow!("Invalid {name} {value:?}: expected 20 bytes, got {}", bytes.len()));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const POOL_MANAGER: &str = "a0FfB9c1CE1Fe56963B0321B32E7A0302114058b";
    const VAULT: &str = "238a358808379702088667322f80aC48bAd5e6c4";

    #[test]
    fn parses_pool_manager_and_vault() {
        let deployment = Deployment::parse(
            "map_pools_created",
            &format!("pool_manager={POOL_MANAGER}&vault={VAULT}"),
        )
        .unwrap();
        assert_eq!(
            deployment,
            Deployment {
                pool_manager: hex::decode(POOL_MANAGER).unwrap(),
                vault: hex::decode(VAULT).unwrap(),
            }
        );
    }

    #[rstest::rstest]
    #[case::bare_address(POOL_MANAGER)]
    #[case::missing_vault(&format!("pool_manager={POOL_MANAGER}"))]
    #[case::unknown_key(&format!("pool_manager={POOL_MANAGER}&vault={VAULT}&hooks=00"))]
    #[case::short_address(&format!("pool_manager=1234&vault={VAULT}"))]
    #[case::prefixed_address(&format!("pool_manager=0x{POOL_MANAGER}&vault={VAULT}"))]
    fn rejects_malformed_params(#[case] params: &str) {
        let err = Deployment::parse("map_pools_created", params).unwrap_err();
        assert!(err.to_string().starts_with("Invalid "), "{err}");
    }
}
