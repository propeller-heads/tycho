use alloy_primitives::Address;

/// Parses the optional Ve33 extension address passed as module params.
///
/// The Ve33 extension is deployed at a chain-specific address, or not at all
/// (see the v3.2.0 release notes of EkuboProtocol/evm-contracts). Deployments
/// indexing a chain with Ve33 pass the address as the module's params;
/// chains without it (e.g. Ethereum) leave the params empty, which disables
/// Ve33 handling.
pub fn ve33_address(params: &str) -> Option<Address> {
    let params = params.trim();
    (!params.is_empty()).then(|| {
        params
            .parse()
            .unwrap_or_else(|err| panic!("invalid Ve33 address param {params:?}: {err}"))
    })
}
