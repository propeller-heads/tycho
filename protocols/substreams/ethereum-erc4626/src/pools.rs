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
    /// DCI entrypoints traced in addition to the standard ERC4626 ones, so DCI indexes storage
    /// the standard traces can miss: state a conversion reads only on some branches (e.g. an
    /// accrual rate skipped when `rho == block.timestamp`). Paired by index with
    /// `entrypoint_calldata`.
    pub entrypoint_signatures: Option<Vec<String>>,
    pub entrypoint_calldata: Option<Vec<String>>,
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

    /// Returns the `(signature, calldata)` of each configured extra entrypoint.
    pub fn extra_entrypoints(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let signatures = self
            .entrypoint_signatures
            .clone()
            .unwrap_or_default();
        let calldata = self
            .entrypoint_calldata
            .clone()
            .unwrap_or_default();
        if signatures.len() != calldata.len() {
            anyhow::bail!(
                "Vault {}: {} entrypoint signatures but {} calldata entries",
                self.address,
                signatures.len(),
                calldata.len()
            );
        }
        let mut entrypoints = Vec::with_capacity(signatures.len());
        for (signature, data) in signatures.into_iter().zip(calldata) {
            let data = hex::decode(&data).with_context(|| {
                format!("Vault {}: invalid calldata for entrypoint {signature}", self.address)
            })?;
            entrypoints.push((signature, data));
        }
        Ok(entrypoints)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test_decode_params() {
        let params = "address=28b3a8fb53b741a8fd78c0fb9a6b2393d896a43d&tx_hash=ebbeb171c33498b2ad47af8bb8bfcc17990db86dd0444b8e64754c250ae1a0b2&asset=a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48&block=23484422&static_attribute_keys[]=name&static_attribute_vals[]=spUSDC#address=e2e7a17dff93280dec073c995595155283e3c372&tx_hash=4fe047d7749d8c1fbfcad10ebd54dd34adeb296356d9c5d8a77f0a60e4c8bab2&asset=dac17f958d2ee523a2206206994597c13d831ec7&block=23484439&static_attribute_keys[]=name&static_attribute_vals[]=spUSDT#address=fe6eb3b609a7c8352a241f7f3a21cea4e9209b8f&tx_hash=e8e324769d1e79969eb2febc30dc3371a96cb428094782a8488dbeb6541dfe7f&asset=c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2&block=23484474&static_attribute_keys[]=name&static_attribute_vals[]=spETH#address=a3931d71877c0e7a3148cb7eb4463524fec27fbd&tx_hash=e1be00c4ea3c21cf536b98ac082a5bba8485cf75d6b2b94f4d6e3edd06472c00&asset=dc035d45d973e3ec169d2276ddab16f1e407384f&block=20677434&static_attribute_keys[]=name&static_attribute_vals[]=sUSDS#address=bc65ad17c5c0a2a4d159fa5a503f4992c7b545fe&tx_hash=549d8f23f755f10ac453949f7ce76f19cfd3007ca8402588fe66b0e54ada627c&asset=a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48&block=21969024&static_attribute_keys[]=name&static_attribute_vals[]=sUSDC#address=83f20f44975d03b1b09e64809b757c47f942beea&tx_hash=a2f51048265f2fe9ffaf69b94cb5a2a4113be49bdecd2040d530dd6f68facc42&asset=6b175474e89094c44da98b954eedeac495271d0f&block=16428133&static_attribute_keys[]=name&static_attribute_vals[]=sDAI";
        let pools = PoolParams::parse_params(params).unwrap();
        assert_eq!(pools.len(), 6);
    }

    #[test]
    fn test_decode_extra_entrypoints() {
        let params = "address=99cd4ec3f88a45940936f469e4bb72a2a701eeb9&tx_hash=719e&asset=dc03&block=1&entrypoint_signatures[]=str()&entrypoint_calldata[]=c15bae84&entrypoint_signatures[]=chi()&entrypoint_calldata[]=c92aecc4#address=a3931d71877c0e7a3148cb7eb4463524fec27fbd&tx_hash=e1be&asset=dc03&block=2";
        let pools = PoolParams::parse_params(params).unwrap();

        let entrypoints = pools[0].extra_entrypoints().unwrap();
        assert_eq!(
            entrypoints,
            vec![
                ("str()".to_string(), hex::decode("c15bae84").unwrap()),
                ("chi()".to_string(), hex::decode("c92aecc4").unwrap()),
            ]
        );
        assert!(pools[1]
            .extra_entrypoints()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_extra_entrypoints_reject_unpaired_entries() {
        let params = "address=99cd&tx_hash=719e&asset=dc03&block=1&entrypoint_signatures[]=str()&entrypoint_signatures[]=chi()&entrypoint_calldata[]=c15bae84";
        let pools = PoolParams::parse_params(params).unwrap();

        assert!(pools[0].extra_entrypoints().is_err());
    }

    #[test]
    fn test_extra_entrypoints_reject_invalid_calldata() {
        let params = "address=99cd&tx_hash=719e&asset=dc03&block=1&entrypoint_signatures[]=str()&entrypoint_calldata[]=0xc15bae84";
        let pools = PoolParams::parse_params(params).unwrap();

        assert!(pools[0].extra_entrypoints().is_err());
    }
}
