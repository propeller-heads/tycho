use anyhow::{anyhow, Result};
use serde::Deserialize;

/// Module parameters, given in the manifest as a query string: `factory=<hex address>`.
#[derive(Debug, Deserialize)]
struct Params {
    factory: String,
}

/// Parses the module parameters and returns the AlgebraFactory address they name.
///
/// Errors when the parameters are not a query string with a `factory` key, or when its value
/// is not a 20-byte hex address (a `0x` prefix is accepted).
pub fn parse_factory(params: &str) -> Result<[u8; 20]> {
    let params: Params = serde_qs::from_str(params)
        .map_err(|e| anyhow!("failed to parse module params {params:?}: {e}"))?;
    let bytes = hex::decode(params.factory.trim_start_matches("0x"))
        .map_err(|e| anyhow!("factory param {:?} is not hex: {e}", params.factory))?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| {
            anyhow!("factory param {:?} must be 20 bytes, got {}", params.factory, bytes.len())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_factory_without_prefix() {
        let factory = parse_factory("factory=1a3c9b1d2f0529d97f2afc5136cc23e58f1fd35b").unwrap();
        assert_eq!(hex::encode(factory), "1a3c9b1d2f0529d97f2afc5136cc23e58f1fd35b");
    }

    #[test]
    fn parses_factory_with_prefix() {
        let factory = parse_factory("factory=0x1a3c9B1d2F0529D97f2afC5136Cc23e58f1FD35B").unwrap();
        assert_eq!(hex::encode(factory), "1a3c9b1d2f0529d97f2afc5136cc23e58f1fd35b");
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(parse_factory("factory=1a3c9b").is_err());
    }

    #[test]
    fn rejects_missing_key() {
        assert!(parse_factory("pool=1a3c9b1d2f0529d97f2afc5136cc23e58f1fd35b").is_err());
    }
}
