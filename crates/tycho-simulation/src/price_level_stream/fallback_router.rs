//! Reads the venue whitelist of Titan's PropAMMRouter.
//!
//! Venues on the whitelist may be served under the `propammfallback:` protocol family, which
//! executes their swaps through the router instead of the venue directly, so a stale maker
//! quote falls back to a single-hop Uniswap V3 pool instead of reverting the route.

use alloy::{
    network::Ethereum,
    primitives::{address, Address, TxKind},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::TransactionRequest,
    sol,
    sol_types::SolCall,
};
use tycho_common::Bytes;

/// Titan's PropAMMRouter deployment on Ethereum mainnet: written by LambdaClass, behind a UUPS
/// proxy so upgrades keep the address.
///
/// Must match `tycho-execution`'s `PropAMMFallbackExecutor.PROPAMM_ROUTER`.
/// <https://github.com/lambdaclass/propamm-router-contracts>
pub const FALLBACK_ROUTER_ADDRESS: Address = address!("4DdF368080CD7946db5b459aD591c350158175e1");

sol! {
    /// The whitelist accessor of the PropAMMRouter. The swap surface the executor uses lives in
    /// `tycho-execution`'s `IPropAMMRouter.sol`.
    function getWhitelistedVenues() external view returns (address[] memory venues);
}

/// Error reading the PropAMMRouter's venue whitelist.
#[derive(Debug, thiserror::Error)]
pub enum FetchVenuesError {
    /// The RPC URL could not be parsed.
    #[error("invalid RPC URL {url:?}: {reason}")]
    InvalidUrl {
        /// The URL that failed to parse.
        url: String,
        /// The parse error.
        reason: String,
    },
    /// The `eth_call` failed or returned undecodable data.
    #[error("getWhitelistedVenues call to the PropAMMRouter failed: {reason}")]
    Call {
        /// Underlying transport or ABI decoding error.
        reason: String,
    },
}

/// Reads the router's whitelisted pAMM venues via `eth_call` on the node at `rpc_url`.
///
/// Read once at startup: the whitelist is governance-gated and changes rarely, and renaming a
/// running component's protocol system would churn every consumer's component set.
///
/// # Errors
///
/// Returns [`FetchVenuesError::InvalidUrl`] if `rpc_url` does not parse, and
/// [`FetchVenuesError::Call`] if the `eth_call` fails or returns undecodable data.
pub async fn fetch_fallback_router_venues(rpc_url: &str) -> Result<Vec<Bytes>, FetchVenuesError> {
    let url: reqwest::Url = rpc_url
        .parse()
        .map_err(|e| FetchVenuesError::InvalidUrl {
            url: rpc_url.to_string(),
            reason: format!("{e}"),
        })?;
    let provider: RootProvider<Ethereum> = ProviderBuilder::default().connect_http(url);
    let response = provider
        .call(TransactionRequest {
            to: Some(TxKind::Call(FALLBACK_ROUTER_ADDRESS)),
            input: getWhitelistedVenuesCall {}
                .abi_encode()
                .into(),
            ..Default::default()
        })
        .await
        .map_err(|e| FetchVenuesError::Call { reason: e.to_string() })?;
    let venues = getWhitelistedVenuesCall::abi_decode_returns(&response).map_err(|e| {
        FetchVenuesError::Call { reason: format!("failed to decode response: {e}") }
    })?;
    Ok(venues
        .into_iter()
        .map(|venue| Bytes::from(venue.as_slice().to_vec()))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[tokio::test]
    #[ignore = "Requires RPC_URL to be set in environment variables or .env file"]
    async fn test_fetch_fallback_router_venues_against_mainnet() {
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set for network tests");

        let venues = fetch_fallback_router_venues(&rpc_url)
            .await
            .expect("whitelist read should succeed");

        // FermiSwap is whitelisted on the live router.
        let fermiswap =
            Bytes::from_str("0x5979458912f80b96d30d4220af8e2e4925a33320").expect("valid address");
        assert!(venues.contains(&fermiswap), "expected FermiSwap in {venues:?}");
    }

    #[tokio::test]
    async fn test_fetch_fallback_router_venues_invalid_url() {
        let result = fetch_fallback_router_venues("not a url").await;
        assert!(matches!(result, Err(FetchVenuesError::InvalidUrl { .. })));
    }

    /// Reading the whitelist from a different router than the executor calls would let a venue
    /// be served under `propammfallback:` that the executed router rejects.
    #[test]
    fn test_router_address_matches_the_executor() {
        let executor = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tycho-execution/contracts/src/executors/PropAMMFallbackExecutor.sol");
        let source = std::fs::read_to_string(&executor)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", executor.display()));

        let address = FALLBACK_ROUTER_ADDRESS.to_string();
        assert!(source.contains(&address), "PropAMMFallbackExecutor.sol does not use {address}");
    }
}
