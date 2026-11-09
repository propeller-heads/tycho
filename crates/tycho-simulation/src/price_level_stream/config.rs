use std::{collections::HashMap, str::FromStr, sync::LazyLock};

use num_bigint::BigUint;
use tycho_common::Bytes;

/// Protocol system family name of components sourced from the pAMM price level stream.
///
/// The full protocol system of a component is `pricelevelstream:{pamm}`, where `{pamm}` is the
/// configured venue name (e.g. `pricelevelstream:fermiswap`) or, for auto-detected venues, the
/// venue address (e.g. `pricelevelstream:0x5979…`); see the [module documentation](super) for
/// details.
pub const PRICE_LEVEL_STREAM_FAMILY: &str = "pricelevelstream";

/// Protocol system family of components executed through Tycho's `TychoFallbackRouter`, which
/// retries a reverted pAMM swap on the fallback pool the solver names in the swap's `user_data`.
///
/// Must match `tycho-execution`'s `FALLBACK_KEY`.
pub const FALLBACK_FAMILY: &str = "fallback";

/// Configuration of a single pAMM to be served from the price level stream.
#[derive(Debug, Clone)]
pub struct PriceLevelStreamConfig {
    /// Bare pAMM name (e.g. `fermiswap`); the emitted components carry
    /// `pricelevelstream:{protocol}` as their protocol system.
    pub protocol: String,
    /// The venue's canonical public identity: the address its swaps are sent to, which the
    /// emitted components carry. Either of a venue's addresses may be given here — see
    /// [`PAMM_ADDRESS_ALIASES`].
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

    /// The protocol system identifier when this pAMM executes through `TychoFallbackRouter`.
    pub fn fallback_protocol_system(&self) -> String {
        format!("{FALLBACK_FAMILY}:{}", self.protocol)
    }
}

/// Per-swap gas estimate for auto-detected pAMMs whose venue has not been measured: the maximum
/// over the known venue profiles (see [`default_served_pamms`]), as the conservative choice.
/// Overridable per stream via
/// [`auto_detected_gas_cost`](super::stream::PriceLevelStreamBuilder::auto_detected_gas_cost).
pub const DEFAULT_AUTO_DETECTED_GAS_COST: u64 = 335_000;

/// The pAMMs known to be served by the Titan price level stream (as of 2026-10-19): FermiSwap,
/// Kipseli, Metric, Bebop, TaurusFi, Tempest, Goliath, Stelaxis, and ElZorro.
///
/// Registered on a builder via
/// [`with_known_pamms`](super::stream::PriceLevelStreamBuilder::with_known_pamms), so their
/// components carry the venue name instead of the raw address; an
/// [`add_pamm`](super::stream::PriceLevelStreamBuilder::add_pamm) call for one of these
/// addresses overrides the corresponding entry.
///
/// Only the venues' router addresses are registered, because an entry here doubles as the
/// execution target ([`PAMM_ADDRESS_ATTRIBUTE`](super::stream::PAMM_ADDRESS_ATTRIBUTE)): it must
/// be an address a swap can be sent to. A venue whose streamed key is not that address is
/// bridged by [`PAMM_ADDRESS_ALIASES`] rather than registered under the streamed key.
pub fn default_served_pamms() -> Vec<PriceLevelStreamConfig> {
    // The venues' `IPropAMM::swap` gas, calibrated by replaying real fills on the live venues at
    // fresh-oracle blocks via `debug_traceCall`, plus a small headroom. Deliberately excludes
    // router-level overhead (user/input/fee transfers): tycho-execution's gas estimator accounts
    // for those on top of this per-swap value.
    let pamms = [
        // The FermiSwapper router. Measured ~177k-182k (2026-08-18).
        ("fermiswap", "0x5979458912f80b96d30d4220af8e2e4925a33320", 185_000u64),
        // The KipseliPropAMMWrapper router. Measured ~308k-329k (2026-08-18). Titan's venue docs
        // list a newer Kipseli router (0x342b8458…), which has no activity. The stream keys
        // Kipseli's books by 0xc9a9… instead, which `PAMM_ADDRESS_ALIASES` maps onto this
        // address.
        ("kipseli", "0x71e790dd841c8a9061487cb3e78c288e75ce0b3d", 335_000u64),
        // The Metric router, per Titan's venue docs. It replaced 0xe715dc29…, which has had no
        // activity since 2026-09-07. Titan's V2 socket streams Metric's books under
        // 0xe8447019… instead, bridged by `PAMM_ADDRESS_ALIASES`. Its swap gas depends on the
        // pair, so one value cannot fit all of them. Over 213 fills (2026-10-16), p50 was ~165k
        // on USDC/USDT, ~245k on the WETH pairs and ~550k on WBTC/USDC; across all fills p50
        // ~234k, p90 ~280k. This takes the p90: it overstates the stablecoin pair and
        // understates WBTC/USDC.
        ("metric", "0x99ab7c1c5eab47801f7937075998527a550fba8a", 280_000u64),
        // The BopAMM (Bebop) router. It replaced 0xb09aaa56… on 2026-09-15, which has had no
        // activity since. The bytecode is the same except for one embedded address, so the
        // earlier measurement (~133k-136k, 2026-08-18) still applies.
        ("bebop", "0xb09aaa8933626d7e4c48d65dad2d77021cfbca9a", 140_000u64),
        // The TaurusFi router, which both sockets stream TaurusFi under: an ERC-1967 proxy that
        // emits the pAMM swap event, so it needs no alias. It replaced 0x217d5893…, and in turn
        // 0x3ce2672a… (tycho#1633), neither of which has had a swap in the last 7,200 blocks.
        // Gas over 25 traced fills (2026-10-17): p50 ~49k, p90 ~66k, max ~109k.
        ("taurusfi", "0x20861433d9119b866eeb398cb1ad46652a3cf953", 110_000u64),
        // The Tempest router (unverified), per Titan's venue docs. Measured ~120k-155k
        // (2026-09-11).
        ("tempest", "0x00000003f1ec2379e79f58e12ec6c4f51ee92149", 160_000u64),
        // The Goliath router, which the stream keys its books by. Gas over 80 traced fills
        // (2026-10-19): p50 ~64k, p90 ~89k, max ~98k.
        ("goliath", "0x4367985a63063fc970b009c6e599e9225181f551", 105_000u64),
        // The Stelaxis router, which the stream keys its books by: a minimal proxy that emits
        // the pAMM swap event. Gas over 72 traced fills (2026-10-19): p50 ~65k, p90 ~95k, max
        // ~100k.
        ("stelaxis", "0x77047af6cd8f7f84d96a020a2833d24916b75fde", 105_000u64),
        // The ElZorro router, which the stream keys its books by: a minimal proxy that emits the
        // pAMM swap event. Gas over 71 traced fills (2026-10-19): p50 ~35k, p90 ~68k, max ~97k.
        ("elzorro", "0xcf211b4dd0d2be5c173ea57bcf938fc61d1d3bd3", 105_000u64),
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
    // No streamed venue is currently known to reject the executor's swap. This is where one
    // goes that gates settlement — on a taker allowlist, say — or otherwise reverts a swap sent
    // by an arbitrary caller.
    Vec::new()
}

/// The streamed identities Titan streams some venues under, each paired with that venue's canonical
/// public identity — the address a swap is sent to.
///
/// A streamed `quoteAddress` is a join key on Titan's streams, not necessarily an executable
/// address. Where the two differ, the public identity is the executable one: it is what component
/// ids, venue names, and the
/// [`PAMM_ADDRESS_ATTRIBUTE`](super::stream::PAMM_ADDRESS_ATTRIBUTE) carry, and what
/// [`add_pamm`](super::stream::PriceLevelStreamBuilder::add_pamm) and
/// [`deny_pamm`](super::stream::PriceLevelStreamBuilder::deny_pamm) are expected to name. Either
/// address may be passed to those two, since both normalize through this table.
///
/// Venues absent from this table stream under their public identity, so they need no entry.
pub const PAMM_ADDRESS_ALIASES: &[(&str, &str)] = &[
    // Kipseli streams both its books and its overrides under 0xc9a9…, while its swaps go to the
    // KipseliPropAMMWrapper router (0x71e7…) — the alias Titan's public taker docs publish, and
    // the address `default_served_pamms` registers it under.
    ("0xc9a956b5196dfe110debe8781857bc8b97d32091", "0x71e790dd841c8a9061487cb3e78c288e75ce0b3d"),
    // Metric streams its books under 0xe8447019…, which holds no swap entry point — it emits no
    // pAMM swap event at all — while its swaps go to the router 0x99ab7c1c…, the address
    // `default_served_pamms` registers it under.
    //
    // Metric's router also reads the state of a second quote address, 0x30a88bf8…, which streams
    // no books of its own (2026-10-21) but lists 0x99ab7c1c… in its quote stream entries'
    // `pricedAddresses`. It is deliberately left unaliased. The overrides path merges its entries
    // into Metric's own through `pricedAddresses`; aliased, they would count as Metric's own
    // entries and replace rather than complete them. And any book it streamed in future would
    // become a second book of the same pair, where the duplicate rule keeps the last in stream
    // order rather than the better price.
    ("0xe844701934d9c6085b292e428f41b911cc0b3872", "0x99ab7c1c5eab47801f7937075998527a550fba8a"),
];

/// [`PAMM_ADDRESS_ALIASES`] as a lookup, built once per process.
static CANONICAL_PAMM_ADDRESSES: LazyLock<HashMap<Bytes, Bytes>> = LazyLock::new(|| {
    PAMM_ADDRESS_ALIASES
        .iter()
        .map(|(streamed, public)| {
            (
                Bytes::from_str(streamed).expect("hardcoded streamed pAMM address must parse"),
                Bytes::from_str(public).expect("hardcoded public pAMM address must parse"),
            )
        })
        .collect()
});

/// The canonical public identity of the venue `address` names: the aliased address from
/// [`PAMM_ADDRESS_ALIASES`], or `address` itself when no alias names it.
///
/// Idempotent — a public identity canonicalizes to itself — so normalizing an already
/// canonical address is harmless.
pub(super) fn canonical_pamm_address(address: &Bytes) -> Bytes {
    CANONICAL_PAMM_ADDRESSES
        .get(address)
        .unwrap_or(address)
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An alias exists to carry a streamed key onto an address a swap can be sent to, and
    /// `default_served_pamms` is what makes that address a named venue. An alias pointing at an
    /// address nothing registers therefore serves nothing: the books canonicalize onto a venue
    /// the stream does not know, and are skipped as unregistered. That is how a venue goes dark
    /// silently, so pin it.
    #[test]
    fn every_alias_points_at_a_registered_venue() {
        let registered: Vec<Bytes> = default_served_pamms()
            .into_iter()
            .map(|config| config.address)
            .collect();

        for (streamed, public) in PAMM_ADDRESS_ALIASES {
            let public = Bytes::from_str(public).expect("hardcoded public pAMM address");
            assert!(
                registered.contains(&public),
                "the alias of {streamed} points at {public}, which no venue registers"
            );
        }
    }

    /// A streamed key and the public identity it aliases must differ, and no streamed key may be
    /// registered as a venue: registering one would make the venue its own alias target and leave
    /// the canonical address ambiguous.
    #[test]
    fn a_streamed_key_is_never_itself_a_registered_venue() {
        let registered: Vec<Bytes> = default_served_pamms()
            .into_iter()
            .map(|config| config.address)
            .collect();

        for (streamed, public) in PAMM_ADDRESS_ALIASES {
            let streamed = Bytes::from_str(streamed).expect("hardcoded streamed pAMM address");
            assert_ne!(&streamed, &Bytes::from_str(public).expect("public"));
            assert!(
                !registered.contains(&streamed),
                "{streamed} is both a streamed key and a registered venue"
            );
        }
    }
}
