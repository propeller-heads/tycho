use alloy_primitives::Address;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Params {
    ve33_address: Option<Address>,
    signed_exclusive_swap_address: Option<Address>,
}

fn parse(params: &str) -> Params {
    serde_qs::from_str(params)
        .unwrap_or_else(|err| panic!("invalid module params {params:?}: {err}"))
}

/// Parses the deployment-specific Ve33 extension address from the module
/// params (`ve33_address=0x...`). Empty params disable Ve33 handling.
pub fn ve33_address(params: &str) -> Option<Address> {
    parse(params).ve33_address
}

/// Parses the deployment-specific SignedExclusiveSwap extension address from the
/// module params (`signed_exclusive_swap_address=0x...`). Omitting it means no
/// pool gets the `is_exclusive` attribute.
pub fn signed_exclusive_swap_address(params: &str) -> Option<Address> {
    parse(params).signed_exclusive_swap_address
}
