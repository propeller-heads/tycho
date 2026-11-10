use anyhow::{Context, Result};
use serde::Deserialize;

const PARAMS_SEPERATOR: &str = "#";

#[derive(Debug, Deserialize, PartialEq)]
pub struct PoolParams {
    pub address: String,
    pub block: u64,
    pub tx_hash: String,
    pub asset: String,
    pub static_attribute_keys: Option<Vec<String>>,
    pub static_attribute_vals: Option<Vec<String>>,
    /// DCI entrypoints traced in addition to the standard ERC4626 ones. The Readme's
    /// "Entrypoints" section says when a vault needs one.
    #[serde(default)]
    pub extra_entrypoints: Vec<ExtraEntrypoint>,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct ExtraEntrypoint {
    pub signature: String,
    #[serde(with = "hex")]
    pub calldata: Vec<u8>,
}

impl PoolParams {
    pub fn parse_params(params: &str) -> Result<Vec<PoolParams>> {
        let mut result = Vec::new();
        for param in params.split(PARAMS_SEPERATOR) {
            let pool: PoolParams = serde_qs::from_str(param)
                .with_context(|| format!("Failed to parse pool params: {param}"))?;
            result.push(pool);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const SUBSTREAMS_YAML: &str = include_str!("../substreams.yaml");

    #[test]
    fn test_decode_configured_params() {
        let params = SUBSTREAMS_YAML
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix("map_protocol_components: &vaults '")
            })
            .and_then(|line| line.strip_suffix('\''))
            .expect("substreams.yaml anchors the vault params on map_protocol_components");

        let pools = PoolParams::parse_params(params).unwrap();

        assert_eq!(pools.len(), 7);
        let stusds = pools
            .iter()
            .find(|pool| pool.address == "99cd4ec3f88a45940936f469e4bb72a2a701eeb9")
            .expect("stUSDS is configured");
        assert_eq!(
            stusds.extra_entrypoints,
            vec![ExtraEntrypoint {
                signature: "str()".to_string(),
                calldata: hex::decode("c15bae84").unwrap()
            }]
        );
    }

    #[test]
    fn test_decode_extra_entrypoints_in_order() {
        let params = "address=99cd&tx_hash=719e&asset=dc03&block=1&extra_entrypoints[0][signature]=str()&extra_entrypoints[0][calldata]=c15bae84&extra_entrypoints[1][signature]=chi()&extra_entrypoints[1][calldata]=c92aecc4";

        let pools = PoolParams::parse_params(params).unwrap();

        assert_eq!(
            pools[0].extra_entrypoints,
            vec![
                ExtraEntrypoint {
                    signature: "str()".to_string(),
                    calldata: hex::decode("c15bae84").unwrap()
                },
                ExtraEntrypoint {
                    signature: "chi()".to_string(),
                    calldata: hex::decode("c92aecc4").unwrap()
                },
            ]
        );
    }

    #[test]
    fn test_extra_entrypoints_default_to_none() {
        let pools =
            PoolParams::parse_params("address=a393&tx_hash=e1be&asset=dc03&block=2").unwrap();

        assert!(pools[0].extra_entrypoints.is_empty());
    }

    #[test]
    fn test_extra_entrypoints_reject_entry_without_calldata() {
        let params =
            "address=99cd&tx_hash=719e&asset=dc03&block=1&extra_entrypoints[0][signature]=str()";

        let err = PoolParams::parse_params(params).unwrap_err();

        assert!(format!("{err:#}").contains("calldata"), "{err:#}");
    }

    #[test]
    fn test_extra_entrypoints_reject_invalid_calldata() {
        let params = "address=99cd&tx_hash=719e&asset=dc03&block=1&extra_entrypoints[0][signature]=str()&extra_entrypoints[0][calldata]=0xc15bae84";

        let err = PoolParams::parse_params(params).unwrap_err();

        assert!(format!("{err:#}").contains("Invalid character"), "{err:#}");
    }
}
