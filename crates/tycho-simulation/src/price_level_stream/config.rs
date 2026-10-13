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

/// The pAMMs known to be served by the Titan price level stream (as of 2026-09-11): FermiSwap,
/// Kipseli, Metric, Bebop, TaurusFi, and Tempest.
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
        // The Metric router (unverified; identified via its pools' pricing reads of the Metric
        // oracle 0x28d9cced…). Measured ~225k (2026-08-18).
        ("metric", "0xe715dc29d2c273d0fc5a03e5cca9ccb0abb1dcdb", 230_000u64),
        // The BopAMM (Bebop) router. It replaced 0xb09aaa56… on 2026-09-15, which has had no
        // activity since. The bytecode is the same except for one embedded address, so the
        // earlier measurement (~133k-136k, 2026-08-18) still applies.
        ("bebop", "0xb09aaa8933626d7e4c48d65dad2d77021cfbca9a", 140_000u64),
        // The TaurusFi router, per Titan's venue docs. Measured ~105k (2026-08-18).
        ("taurusfi", "0x217d58931a8549ca539426aa8152e33dafc3d95a", 110_000u64),
        // The Tempest router (unverified), per Titan's venue docs. Measured ~120k-155k
        // (2026-09-11).
        ("tempest", "0x00000003f1ec2379e79f58e12ec6c4f51ee92149", 160_000u64),
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

/// The token symbols the price level stream quotes in, bound to the Ethereum addresses of the
/// assets they name.
///
/// A streamed book names its tokens by symbol only, and a symbol is not an identity: resolving
/// one against the consumer's token metadata by name would let any token that calls itself
/// `WBTC` capture the bound asset's quotes. A symbol is therefore only ever priced if this table
/// — or a caller's override — binds it to an address, and that address is additionally present
/// in the consumer's own token metadata.
///
/// Symbols match exactly, case included, so a symbol differing in case stays unbound instead of
/// being guessed at. A streamed symbol absent here is never priced: `NVDAx`, quoted by TaurusFi
/// in the 2026-10-08 capture, is deliberately unbound until its address is independently
/// resolved, since inferring it from the symbol is exactly what this table exists to prevent.
pub const SYMBOL_BINDINGS: &[(&str, &str)] = &[
    ("WETH", "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
    ("USDC", "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
    ("USDT", "0xdac17f958d2ee523a2206206994597c13d831ec7"),
    ("WBTC", "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599"),
    ("cbBTC", "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf"),
];

/// The symbol bindings one stream resolves streamed book symbols through: [`SYMBOL_BINDINGS`]
/// with the caller's overrides layered on top.
///
/// Internal: callers pass their overrides as a plain map to
/// [`with_symbol_bindings`](super::stream::PriceLevelStreamBuilder::with_symbol_bindings), which
/// layers them here.
#[derive(Debug, Clone)]
pub(super) struct SymbolBindings(HashMap<String, Bytes>);

impl SymbolBindings {
    /// The trusted defaults with `overrides` layered over them: an override replaces the default
    /// binding of its symbol and binds symbols the defaults do not carry.
    pub(super) fn with_overrides(overrides: HashMap<String, Bytes>) -> Self {
        let mut bindings: HashMap<String, Bytes> = SYMBOL_BINDINGS
            .iter()
            .map(|(symbol, address)| {
                (
                    (*symbol).to_string(),
                    Bytes::from_str(address).expect("hardcoded token address must parse"),
                )
            })
            .collect();
        bindings.extend(overrides);
        Self(bindings)
    }

    /// The address bound to `symbol`, or `None` if no binding names it. Matching is exact.
    pub(super) fn address(&self, symbol: &str) -> Option<&Bytes> {
        self.0.get(symbol)
    }
}

impl Default for SymbolBindings {
    /// The trusted defaults, without overrides.
    fn default() -> Self {
        Self::with_overrides(HashMap::new())
    }
}
