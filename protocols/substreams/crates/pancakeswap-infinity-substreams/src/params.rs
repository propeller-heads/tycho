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

/// The two Infinity contracts a package indexes against.
#[derive(Debug, PartialEq, Eq)]
pub struct Deployment {
    /// The pool manager, the emitter of `Initialize`.
    pub pool_manager: [u8; 20],
    /// Holds every pool's funds, so it is `balance_owner` on every component.
    pub vault: [u8; 20],
}

impl Deployment {
    /// Parses `pool_manager=<hex>&vault=<hex>`. Every error names `module`, the handler the
    /// string was passed to: a missing or unknown key, or an address that is not 20 bytes of hex.
    pub fn parse(module: &str, params: &str) -> Result<Self, anyhow::Error> {
        let invalid = |err: anyhow::Error| anyhow!("Invalid {module} params {params:?}: {err}");
        let raw: RawDeployment = serde_qs::from_str(params).map_err(|err| invalid(anyhow!(err)))?;
        Ok(Self {
            pool_manager: decode_address("pool_manager", &raw.pool_manager).map_err(invalid)?,
            vault: decode_address("vault", &raw.vault).map_err(invalid)?,
        })
    }
}

/// Decodes a 20-byte address given as hex without the `0x` prefix; `name` is for the error.
pub fn decode_address(name: &str, value: &str) -> Result<[u8; 20], anyhow::Error> {
    let bytes = hex::decode(value).map_err(|err| anyhow!("Invalid {name} {value:?}: {err}"))?;
    <[u8; 20]>::try_from(bytes.as_slice())
        .map_err(|_| anyhow!("Invalid {name} {value:?}: expected 20 bytes, got {}", bytes.len()))
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
                pool_manager: hex::decode(POOL_MANAGER)
                    .unwrap()
                    .try_into()
                    .unwrap(),
                vault: hex::decode(VAULT)
                    .unwrap()
                    .try_into()
                    .unwrap(),
            }
        );
    }

    #[rstest::rstest]
    #[case::bare_address(POOL_MANAGER, "unknown field `a0Ff")]
    #[case::missing_vault(&format!("pool_manager={POOL_MANAGER}"), "missing field `vault`")]
    #[case::unknown_key(
        &format!("pool_manager={POOL_MANAGER}&vault={VAULT}&hooks=00"),
        "unknown field `hooks`"
    )]
    #[case::short_address(
        &format!("pool_manager=1234&vault={VAULT}"),
        "Invalid pool_manager \"1234\": expected 20 bytes, got 2"
    )]
    #[case::prefixed_address(
        &format!("pool_manager={POOL_MANAGER}&vault=0x{VAULT}"),
        "Invalid vault \"0x"
    )]
    fn rejects_malformed_params(#[case] params: &str, #[case] reason: &str) {
        let err = Deployment::parse("map_pools_created", params)
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("Invalid map_pools_created params "), "{err}");
        assert!(err.contains(reason), "{err}");
    }
}
