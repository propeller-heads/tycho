use std::str::FromStr;

use num_bigint::BigUint;
use tycho_common::Bytes;

/// Protocol system family name of components sourced from the pAMM price level stream.
///
/// The full protocol system of a component is `pricelevelstream:{pamm}`, where `{pamm}` is the
/// configured venue name (e.g. `pricelevelstream:fermiswap`) or, for auto-detected venues, the
/// venue address (e.g. `pricelevelstream:0x5979…`); see the [module documentation](super) for
/// details.
pub const PRICE_LEVEL_STREAM_FAMILY: &str = "pricelevelstream";

/// Protocol system family of components executed through Titan's PropAMMRouter instead of the
/// venue directly, so a stale maker quote falls back to a single-hop Uniswap V3 pool instead of
/// reverting the route.
///
/// Must match `tycho-execution`'s `PROPAMM_FALLBACK_KEY`.
pub const PROPAMM_FALLBACK_FAMILY: &str = "propammfallback";

/// Configuration of a single pAMM to be served from the price level stream.
#[derive(Debug, Clone)]
pub struct PriceLevelStreamConfig {
    /// Bare pAMM name (e.g. `fermiswap`); the emitted components carry
    /// `pricelevelstream:{protocol}` as their protocol system.
    pub protocol: String,
    /// The pAMM venue address under which Titan streams its quotes.
    pub address: Bytes,
    /// Constant per-swap gas cost estimate reported for every quote of this pAMM.
    pub gas_cost: BigUint,
}

impl PriceLevelStreamConfig {
    pub fn new(protocol: impl Into<String>, address: Bytes, gas_cost: BigUint) -> Self {
        Self { protocol: protocol.into(), address, gas_cost }
    }

    /// The configuration an auto-detected pAMM (streamed by Titan but not otherwise configured)
    /// is served under: named by its full lowercase hex address, with the given per-swap gas
    /// cost.
    pub(super) fn auto_detected(address: Bytes, gas_cost: BigUint) -> Self {
        let protocol = address.to_string();
        Self::new(protocol, address, gas_cost)
    }

    /// The protocol system identifier of components emitted for this pAMM.
    pub fn protocol_system(&self) -> String {
        format!("{PRICE_LEVEL_STREAM_FAMILY}:{}", self.protocol)
    }

    /// The protocol system identifier when this pAMM executes through Titan's PropAMMRouter.
    pub fn fallback_protocol_system(&self) -> String {
        format!("{PROPAMM_FALLBACK_FAMILY}:{}", self.protocol)
    }
}

/// Per-swap gas estimate for auto-detected pAMMs whose venue has not been measured: the maximum
/// over the known venue profiles (see [`default_served_pamms`]), as the conservative choice.
/// Overridable per stream via
/// [`auto_detected_gas_cost`](super::stream::PriceLevelStreamBuilder::auto_detected_gas_cost).
pub const DEFAULT_AUTO_DETECTED_GAS_COST: u64 = 335_000;

/// The pAMMs known to be served by the Titan price level stream (as of 2026-08-13): FermiSwap,
/// Kipseli, Metric, Bebop, and TaurusFi.
///
/// Registered on a builder via
/// [`with_known_pamms`](super::stream::PriceLevelStreamBuilder::with_known_pamms), so their
/// components carry the venue name instead of the raw address; an
/// [`add_pamm`](super::stream::PriceLevelStreamBuilder::add_pamm) call for one of these
/// addresses overrides the corresponding entry.
///
/// Only the venues' router addresses are registered — the keys the price level stream has been
/// observed to use — because the streamed key doubles as the execution target
/// ([`PAMM_ADDRESS_ATTRIBUTE`](super::stream::PAMM_ADDRESS_ATTRIBUTE)): unlike the state-override
/// stream, which also publishes frames under non-executable oracle aliases, an entry here must
/// be an address a swap can be sent to.
pub fn default_served_pamms() -> Vec<PriceLevelStreamConfig> {
    // The venues' `IPropAMM::swap` gas, calibrated by replaying real fills on the live venues at
    // fresh-oracle blocks via `debug_traceCall`, plus a small headroom. Deliberately excludes
    // router-level overhead (user/input/fee transfers): tycho-execution's gas estimator accounts
    // for those on top of this per-swap value.
    let pamms = [
        // The FermiSwapper router. Measured ~177k-182k (2026-08-18).
        ("fermiswap", "0x5979458912f80b96d30d4220af8e2e4925a33320", 185_000u64),
        // The KipseliPropAMMWrapper router. Measured ~308k-329k (2026-08-18). Titan's venue docs
        // list a newer Kipseli router (0x342b8458…), but the stream still keys Kipseli
        // quotes by this address and the newer one has no activity.
        ("kipseli", "0x71e790dd841c8a9061487cb3e78c288e75ce0b3d", 335_000u64),
        // The Metric router (unverified; identified via its pools' pricing reads of the Metric
        // oracle 0x28d9cced…). Measured ~225k (2026-08-18).
        ("metric", "0xe715dc29d2c273d0fc5a03e5cca9ccb0abb1dcdb", 230_000u64),
        // The BopAMM (Bebop) router, per Titan's venue docs. Measured ~133k-136k (2026-08-18).
        ("bebop", "0xb09aaa5614916d7aeb59c295c52c92ca82addd76", 140_000u64),
        // The TaurusFi router, per Titan's venue docs. Measured ~105k (2026-08-18).
        ("taurusfi", "0x217d58931a8549ca539426aa8152e33dafc3d95a", 110_000u64),
    ];
    pamms
        .into_iter()
        .map(|(protocol, address, gas_cost)| {
            PriceLevelStreamConfig::new(
                protocol,
                Bytes::from_str(address).expect("hardcoded pAMM address must parse"),
                BigUint::from(gas_cost),
            )
        })
        .collect()
}

/// The streamed venues known NOT to be executable through the generic executor, excluded from
/// auto-detection via
/// [`with_known_pamms`](super::stream::PriceLevelStreamBuilder::with_known_pamms): quoting them
/// would advertise liquidity every routed swap reverts on. An
/// [`add_pamm`](super::stream::PriceLevelStreamBuilder::add_pamm) entry for one of these
/// addresses overrides the denial.
pub fn default_denied_pamms() -> Vec<Bytes> {
    // Tempest, per Titan's venue docs (unverified contract). Its `swap` enforces a taker
    // allowlist: replays of real fills (2026-08-11) revert with `TakerNotAllowed()` (0xf774ea08)
    // for arbitrary callers regardless of recipient and succeed only from allowlisted takers, so
    // swaps sent by the executor would revert.
    ["0x00000003f1ec2379e79f58e12ec6c4f51ee92149"]
        .into_iter()
        .map(|address| Bytes::from_str(address).expect("hardcoded pAMM address must parse"))
        .collect()
}
