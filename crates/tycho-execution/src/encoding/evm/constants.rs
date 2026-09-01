use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
    time::Duration,
};

use tycho_common::{models::Chain, Bytes};

use crate::encoding::errors::EncodingError;

pub(crate) const DEFAULT_EXECUTORS_JSON: &str =
    include_str!("../../../config/executor_addresses.json");
pub(crate) const DEFAULT_ROUTERS_JSON: &str = include_str!("../../../config/router_addresses.json");
pub(crate) const PROTOCOL_SPECIFIC_CONFIG: &str =
    include_str!("../../../config/protocol_specific_addresses.json");

/// Default router addresses keyed by chain, parsed from `config/router_addresses.json`.
pub static DEFAULT_ROUTER_ADDRESSES: LazyLock<HashMap<Chain, Bytes>> = LazyLock::new(|| {
    serde_json::from_str(DEFAULT_ROUTERS_JSON).expect("valid router_addresses.json")
});

/// Returns the default Tycho router address for `chain`, or an error if none is configured.
pub fn get_router_address(chain: &Chain) -> Result<&'static Bytes, EncodingError> {
    DEFAULT_ROUTER_ADDRESSES
        .get(chain)
        .ok_or_else(|| {
            EncodingError::FatalError(format!(
                "No default router address found for chain {chain:?}"
            ))
        })
}

/// The address used by the TychoRouterV3 to represent native ETH.
///
/// Callers must use this address (not `address(0)`) for the `tokenIn` / `tokenOut`
/// parameters when ABI-encoding router function calls that involve native ETH.
/// The encoding pipeline's `EncodedSolution` only contains the inner swap bytes;
/// the outer function arguments — including the token addresses — are the caller's
/// responsibility.
pub static ROUTER_ETH_ADDRESS: LazyLock<Bytes> = LazyLock::new(|| {
    Bytes::from(alloy::primitives::hex!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE").to_vec())
});

/// The number of blocks in the future for which to fetch Angstrom Attestations
///
/// It is important to note that fetching more blocks will send more attestations to the
/// Tycho Router, resulting in a higher gas usage. Fetching fewer blocks may result in attestations
/// expiring if the transaction is not sent fast enough.
pub const ANGSTROM_DEFAULT_BLOCKS_IN_FUTURE: u64 = 10;

/// The endpoint serving Angstrom pool unlock attestations.
pub(crate) const ANGSTROM_DEFAULT_API_URL: &str =
    "https://attestations.angstrom.xyz/getAttestations";

/// The size of a single Angstrom attestation, without its block number prefix.
///
/// The Uniswap V4 executor rejects attestation data that is not a whole number of
/// `8 + ANGSTROM_ATTESTATION_SIZE` byte entries.
pub(crate) const ANGSTROM_ATTESTATION_SIZE: usize = 85;

/// The shortest time Ethereum can take to produce a block, which both the refresh interval and
/// the maximum window age derive from.
///
/// Ethereum proposes at most one block every 12 seconds, and a skipped proposal only makes the
/// gap longer. Treating 12 seconds as one block therefore always overestimates how many blocks
/// have elapsed, which is the safe direction for both constants below.
const ETHEREUM_MIN_BLOCK_TIME_SECS: u64 = 12;

/// How many times per block the background prefetcher refreshes the attestation window.
///
/// The window's contents only change when a block is produced, so refreshing more than once per
/// block fetches nothing new. It is still more than once because the refresher has no block feed
/// to align to: sampling twice a block bounds how long it keeps serving the previous block's
/// window after a new one becomes available, without polling the API for the sake of it.
const ANGSTROM_ATTESTATION_REFRESHES_PER_BLOCK: u64 = 2;

/// How long the background prefetcher waits between Angstrom attestation refreshes.
pub(crate) const ANGSTROM_ATTESTATION_REFRESH_INTERVAL: Duration =
    Duration::from_secs(ETHEREUM_MIN_BLOCK_TIME_SECS / ANGSTROM_ATTESTATION_REFRESHES_PER_BLOCK);

/// How many of the fetched window's blocks may elapse before the cache refetches while encoding.
///
/// A window fetched during block `N` covers `N` through `N + ANGSTROM_BLOCKS_IN_FUTURE`. Every
/// block that elapses before encoding spends one of those: it removes a block the transaction
/// could still have landed in, and adds an attestation the executor will skip. Keeping this at a
/// single block preserves all but one block of the caller's slack, at the price of refetching
/// inline sooner when the background refresh stalls.
const ANGSTROM_ATTESTATION_MAX_AGE_BLOCKS: u64 = 1;

/// How old a cached Angstrom attestation window may be before it is refetched while encoding.
///
/// Only reached when the background refresh has stopped keeping up: a healthy refresher replaces
/// the window every `ANGSTROM_ATTESTATION_REFRESH_INTERVAL`.
pub(crate) const ANGSTROM_ATTESTATION_MAX_AGE: Duration =
    Duration::from_secs(ETHEREUM_MIN_BLOCK_TIME_SECS * ANGSTROM_ATTESTATION_MAX_AGE_BLOCKS);

/// How long a single request to the Angstrom API may take before it is aborted.
///
/// Half the refresh interval, so one timed-out refresh cannot reach the encoding path: the next
/// refresh still replaces the window within `ANGSTROM_ATTESTATION_MAX_AGE` of the previous one
/// (3s aborted + 6s sleep + at most 3s for the retry). The slowest read measured against the
/// live API was 902ms, including DNS and TLS on a cold connection.
pub(crate) const ANGSTROM_API_TIMEOUT: Duration =
    Duration::from_secs(ANGSTROM_ATTESTATION_REFRESH_INTERVAL.as_secs() / 2);

/// These protocols support the optimization of grouping swaps.
///
/// This requires special encoding to send call data of multiple swaps to a single executor,
/// as if it were a single swap. The protocol likely uses flash accounting to save gas on token
/// transfers.
pub static GROUPABLE_PROTOCOLS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut set = HashSet::new();
    set.insert("uniswap_v4");
    set.insert("uniswap_v4_hooks");
    set.insert("vm:balancer_v3");
    set.insert("ekubo_v2");
    set.insert("ekubo_v3");
    set
});

/// These groupable protocols use simple concatenation instead of PLE when forming swap groups.
pub static NON_PLE_ENCODED_PROTOCOLS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut set = HashSet::new();
    set.insert("ekubo_v2");
    set.insert("ekubo_v3");
    set
});

/// Protocol system prefix carried by components sourced from the pAMM price level stream. The
/// venue suffix is either a configured name (e.g. `pricelevelstream:fermiswap`) or, for
/// auto-detected pAMMs, the venue address (e.g. `pricelevelstream:0x5979…`); every such protocol
/// maps to the generic `PropAMMSwapEncoder`.
pub const PRICE_LEVEL_STREAM_PREFIX: &str = "pricelevelstream:";

/// The executor-config key serving the whole price-level-stream protocol family: any
/// `pricelevelstream:{venue}` protocol without an exact entry of its own falls back to this one,
/// so a single configured executor address covers every pAMM, including auto-detected ones.
pub const PRICE_LEVEL_STREAM_KEY: &str = "pricelevelstream";

/// Protocol system prefix for pAMM components executed through the PropAMMRouter, so a stale maker
/// quote retries on Uniswap V3 instead of reverting the route. Venue suffixes follow
/// `PRICE_LEVEL_STREAM_PREFIX`; only whitelisted venues may use it. Calldata matches the direct
/// path, so both prefixes share `PropAMMSwapEncoder` and differ only in the executor.
pub const PROPAMM_FALLBACK_PREFIX: &str = "propammfallback:";

/// The executor-config key serving the whole PropAMMRouter protocol family, mirroring
/// `PRICE_LEVEL_STREAM_KEY`.
pub const PROPAMM_FALLBACK_KEY: &str = "propammfallback";

#[cfg(test)]
mod tests {
    use super::*;

    /// `get_encoder` matches on the prefix but looks the executor up under the key, so the two
    /// must name the same family.
    #[test]
    fn test_family_keys_and_prefixes_agree() {
        assert_eq!(format!("{PRICE_LEVEL_STREAM_KEY}:"), PRICE_LEVEL_STREAM_PREFIX);
        assert_eq!(format!("{PROPAMM_FALLBACK_KEY}:"), PROPAMM_FALLBACK_PREFIX);
    }

    /// The timings only keep inline fetches off the encoding path while a timed-out refresh plus
    /// the retry that follows it still fit inside the maximum window age.
    #[test]
    fn test_one_timed_out_refresh_cannot_stale_the_window() {
        let slowest_recovery =
            ANGSTROM_API_TIMEOUT + ANGSTROM_ATTESTATION_REFRESH_INTERVAL + ANGSTROM_API_TIMEOUT;

        assert!(
            slowest_recovery <= ANGSTROM_ATTESTATION_MAX_AGE,
            "a single timed-out refresh leaves the window stale for {slowest_recovery:?}, past \
             the {ANGSTROM_ATTESTATION_MAX_AGE:?} maximum age"
        );
    }
}
