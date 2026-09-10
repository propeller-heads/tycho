use alloy_primitives::Address;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Params {
    ve33_address: Option<Address>,
}

/// Parses the deployment-specific Ve33 extension address from the module
/// params (`ve33_address=0x...`). Empty params disable Ve33 handling.
pub fn ve33_address(params: &str) -> Option<Address> {
    let params: Params = serde_qs::from_str(params)
        .unwrap_or_else(|err| panic!("invalid module params {params:?}: {err}"));
    params.ve33_address
}
