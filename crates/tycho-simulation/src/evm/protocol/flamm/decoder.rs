// Copyright (c) 2026 Everlong Labs Limited

//! From a component's attributes to the composed pool state: the identity and pins of the static
//! attributes ([`Statics`], set once at creation by the `base-flamm` substreams), the raw words
//! unpacked by the storage layouts of c104 @ `80abd43` ([`super::words`]), the feeds
//! ([`super::feeds`]) and the consistency checks that make a state quotable ([`decode_core`]).
//! Then the [`TryFromWithBlock`] entry the stream decoder calls on a snapshot.
//!
//! Everything fails closed: an attribute the layout needs that is absent, a value of the wrong
//! width, a codehash the port does not model, a pool whose wiring disagrees with its static
//! identity, a venue set the statics do not describe, or a feed of a kind the port cannot read
//! is a typed [`DecodeError`], never a guess. A FLAMM-owned word reads as zero when absent (never
//! written), except the words below marked required: the pinned code writes them non-zero when
//! it constructs the contract, so they are in the stream from the component's creation on, and
//! a zero would decode into a state that quotes a different amount instead of refusing.

use std::{collections::HashMap, sync::Arc};

use alloy::primitives::{Address, B256, U256};
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::{
    almcurve::Support,
    deps::EverlongLeverageV1,
    fee::FeeParams,
    feeds::{Feed, FeedKind, FeedRole},
    gate::{LoanCfg, Pool},
    hook::HookState,
    morpho::{Market, Position, VenueMarket},
    pricefeed::{FeedRound, FeedToken, PriceFeedState},
    router::{Loan, Router as MmRouter, Venue},
    sim::{FlammPoolState, VenueKind},
    state::{HookKind, LeverageHookSlot, PoolHooks, SpreadHookSlot, SpreadHookState, SwapHookSlot},
    words::{
        account_market_slot, address_of, array_base, factory_is_pool_slot, field, field_addr,
        field_bool, field_u64, hash_of, pricefeed_token_slot, router_record_slot, word_of,
        Attributes, Words, ERC20_NS, FLAMM_NS,
    },
    Flamm,
};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

/// Why a component cannot be decoded into a quotable state.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DecodeError {
    /// An attribute the layout needs is absent.
    Missing(String),
    /// An attribute value of the wrong width or shape.
    Malformed(String),
    /// A codehash, immutable or address the port does not model (`registry`).
    Pin(String),
    /// The pool's own words disagree with its static identity or with each other.
    Drift(String),
    /// A configuration the port does not quote (a second venue or loan asset, a feed kind the
    /// port cannot read).
    Unsupported(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(s) => write!(f, "missing attribute {s}"),
            Self::Malformed(s) => write!(f, "malformed attribute {s}"),
            Self::Pin(s) => write!(f, "unregistered code or immutable: {s}"),
            Self::Drift(s) => write!(f, "wiring drift: {s}"),
            Self::Unsupported(s) => write!(f, "unsupported configuration: {s}"),
        }
    }
}

impl From<DecodeError> for InvalidSnapshotError {
    fn from(e: DecodeError) -> Self {
        match e {
            DecodeError::Missing(s) => InvalidSnapshotError::MissingAttribute(s),
            other => InvalidSnapshotError::ValueError(other.to_string()),
        }
    }
}

/// The code and immutables this port models: the c104 deployment on Base (`c104-deploy` @
/// `80abd43`, `script/flamm/c104/deployments/c104.8453.json`), runtime codehashes as
/// `eth_getCode` reports them. A component whose static attributes name other code fails closed
/// until the port is extended to it.
pub mod registry {
    use alloy::primitives::{address, b256, Address, B256};

    /// `FLAMM` implementation `0xaAD580BeAa2cbd8Ab5F3956a5c56EDa1D5ee7184`.
    pub const IMPLEMENTATION_CODEHASH: B256 =
        b256!("2eb0fb32b59b1cca33e7bd6cad0a214bc6a293dc219d6ff9a5db1c58d0c10cb7");
    /// `EverlongHook` `0x65CBD227cBC61248ae77a5fC813A29C54C092134`.
    pub const HOOK_CODEHASH: B256 =
        b256!("63ca81587b713df89dc9a657dae2cb5a70910cbafbe23b9ebbbdedc1bfdc3e1d");
    /// `EverlongHook.genesisStrategyHash` (`EverlongHook.sol:96`), the fee law the port
    /// implements.
    pub const HOOK_GENESIS_STRATEGY_HASH: B256 =
        b256!("533d23efc2573bf73577c58cdb4f547eea911233a5c0dc1433baa73282208fd0");
    /// `EverlongLeverageHook` `0xE0A98d8e60035832B8BaD7f7af7B9B0b3A7308F3`.
    pub const LEVERAGE_HOOK_CODEHASH: B256 =
        b256!("c6b46f4287cafa36c360234956b22dc635eb725dd17134a668939e731b819756");
    /// `LeverageSpreadHook` `0x04988aF54ec88D2de77b191025EAef2fe488f93b`.
    pub const SPREAD_HOOK_CODEHASH: B256 =
        b256!("79d826c02ae2d2de5f8fdebfc1207731a04a96c0733e7af7c1f030b69d65a425");
    /// `MMRouter` `0x19A9b39E6710AAD109C829294b0841F0851c6bB4`.
    pub const ROUTER_CODEHASH: B256 =
        b256!("6ca3c38096320c757612b113e543c37862f48bce5a2f19de2375357bc31ddcc8");
    /// `MorphoBlueAccount` `0x6760E3b032eE2d670Cb684d9076b8f48cb066c48`.
    pub const ACCOUNT_CODEHASH: B256 =
        b256!("7d5828b262882bb77ce568da078e381979429cc8a40d624e90e15a1895eee847");
    /// `AdaptiveCurveIrm` v1.0.0 `0x46415998764C29aB2a25CbeA6254146D50D22687`, the one rate
    /// model the port prices (`super::super::irm`).
    pub const IRM_CODEHASH: B256 =
        b256!("9978b522abfe0f3b8279800375d833b9d9660ae4f6321a2efb1f1f98850a0cbe");
    pub const IRM: Address = address!("46415998764C29aB2a25CbeA6254146D50D22687");
    /// Morpho Blue v1.0.0 on Base (`super::super::morpho`).
    pub const MORPHO: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
}

/// The component's static attributes (schema 3.3), parsed and pinned.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Statics {
    pub pool: Address,
    pub kind: VenueKind,
    pub implementation: Address,
    pub hook: Address,
    pub hook_loan_scale: U256,
    pub leverage_hook: Address,
    pub spread_hook: Address,
    pub router: Address,
    pub price_feed: Address,
    pub price_feed_sequencer: Address,
    pub price_feed_sequencer_grace: U256,
    pub factory: Address,
    pub pool_asset: Address,
    pub loan_asset_0: Address,
    pub venue_0_account: Address,
    pub venue_0_market_id: B256,
    pub venue_0_morpho: Address,
    pub venue_0_irm: Address,
    pub venue_0_oracle: Address,
    pub venue_0_oracle_scale_factor: U256,
    pub feed_mo0_max_sync_iterations: u32,
    pub feed_asset_proxy: Address,
    pub feed_loan0_proxy: Address,
    pub feed_seq_proxy: Address,
}

/// The component id of the lever-up venue: `pool (20 bytes) || 0x00000000 || uint64(1)`.
pub fn lever_up_id(pool: Address) -> String {
    let mut id = [0u8; 32];
    id[..20].copy_from_slice(pool.as_slice());
    id[31] = 1;
    format!("0x{}", hex::encode(id))
}

/// The component id of the swap venue: the pool address.
pub fn swap_id(pool: Address) -> String {
    format!("0x{}", hex::encode(pool.as_slice()))
}

/// The pool and venue a component id names: 20 bytes for the swap venue, the 32-byte
/// `pool || 0x00000000 || uint64(1)` for lever-up.
pub fn parse_component_id(id: &str) -> Result<(Address, VenueKind), DecodeError> {
    let raw = hex::decode(id.strip_prefix("0x").unwrap_or(id))
        .map_err(|e| DecodeError::Malformed(format!("component id {id}: {e}")))?;
    match raw.len() {
        20 => Ok((Address::from_slice(&raw), VenueKind::Swap)),
        32 if raw[20..24] == [0; 4] && raw[24..] == [0, 0, 0, 0, 0, 0, 0, 1] => {
            Ok((Address::from_slice(&raw[..20]), VenueKind::LeverUp))
        }
        _ => Err(DecodeError::Malformed(format!(
            "component id {id}: neither a pool address nor pool || 0x00000000 || uint64(1)"
        ))),
    }
}

impl Statics {
    /// Parses and pins the static attributes of a component whose id is `id`.
    pub fn parse(id: &str, attrs: &HashMap<String, Bytes>) -> Result<Self, DecodeError> {
        let (pool, kind) = parse_component_id(id)?;
        let get = |n: &str| -> Result<&Bytes, DecodeError> {
            attrs
                .get(n)
                .ok_or_else(|| DecodeError::Missing(n.to_owned()))
        };
        let addr = |n: &str| -> Result<Address, DecodeError> { address_of(n, get(n)?) };
        let word = |n: &str| -> Result<U256, DecodeError> { word_of(n, get(n)?) };
        let hash = |n: &str| -> Result<B256, DecodeError> { hash_of(n, get(n)?) };
        let pin = |n: &str, want: B256| -> Result<(), DecodeError> {
            let got = hash(n)?;
            if got != want {
                return Err(DecodeError::Pin(format!("{n} {got} is not {want}")));
            }
            Ok(())
        };

        let component_kind = word("component_kind")?;
        let want_kind = U256::from(match kind {
            VenueKind::Swap => 0u8,
            VenueKind::LeverUp => 1u8,
        });
        if component_kind != want_kind {
            return Err(DecodeError::Drift(format!(
                "component_kind {component_kind} does not match the id's venue {kind:?}"
            )));
        }
        pin("implementation_codehash", registry::IMPLEMENTATION_CODEHASH)?;
        pin("hook_codehash", registry::HOOK_CODEHASH)?;
        pin("hook_genesis_strategy_hash", registry::HOOK_GENESIS_STRATEGY_HASH)?;
        pin("router_codehash", registry::ROUTER_CODEHASH)?;
        pin("venue_0_account_codehash", registry::ACCOUNT_CODEHASH)?;
        let leverage_hook = addr("leverage_hook")?;
        let spread_hook = addr("spread_hook")?;
        if leverage_hook != Address::ZERO {
            pin("leverage_hook_codehash", registry::LEVERAGE_HOOK_CODEHASH)?;
        }
        if spread_hook != Address::ZERO {
            pin("spread_hook_codehash", registry::SPREAD_HOOK_CODEHASH)?;
        }
        let venue_0_irm = addr("venue_0_irm")?;
        if venue_0_irm != Address::ZERO {
            if venue_0_irm != registry::IRM {
                return Err(DecodeError::Pin(format!(
                    "venue_0_irm {venue_0_irm} is not the AdaptiveCurveIrm"
                )));
            }
            pin("irm_codehash", registry::IRM_CODEHASH)?;
        }
        let venue_0_morpho = addr("venue_0_morpho")?;
        if venue_0_morpho != registry::MORPHO {
            return Err(DecodeError::Pin(format!(
                "venue_0_morpho {venue_0_morpho} is not Morpho Blue"
            )));
        }
        let hook = addr("hook")?;
        let hook_loan_scale = word("hook_loan_scale")?;
        if leverage_hook != Address::ZERO {
            let lev_scale = word("leverage_hook_loan_scale")?;
            if lev_scale != hook_loan_scale {
                return Err(DecodeError::Drift(format!(
                    "leverage_hook_loan_scale {lev_scale} is not the swap hook's {hook_loan_scale}"
                )));
            }
            let bound = addr("leverage_hook_swap_hook")?;
            if bound != hook {
                return Err(DecodeError::Drift(format!(
                    "leverage_hook_swap_hook {bound} is not the swap hook {hook}"
                )));
            }
        }
        let venue_0_oracle = addr("venue_0_oracle")?;
        if venue_0_oracle == Address::ZERO {
            return Err(DecodeError::Unsupported("venue_0_oracle is zero".into()));
        }
        let feed_mo0_proxy = addr("feed_mo0_proxy")?;
        let venue_0_oracle_base_feed_1 = addr("venue_0_oracle_base_feed_1")?;
        let feed_mo0_secondary_proxy = addr("feed_mo0_secondary_proxy")?;
        if venue_0_oracle_base_feed_1 != feed_mo0_proxy ||
            feed_mo0_secondary_proxy != feed_mo0_proxy
        {
            // The oracle reads the DualAggregator through its secondary proxy: any other wiring
            // takes the primary path, which is not modelled.
            return Err(DecodeError::Unsupported(format!(
                "the Morpho oracle's feed {venue_0_oracle_base_feed_1} must be the DualAggregator's secondary proxy {feed_mo0_secondary_proxy} (feed_mo0_proxy {feed_mo0_proxy})"
            )));
        }
        let max_sync = word("feed_mo0_max_sync_iterations")?;
        let feed_mo0_max_sync_iterations = u32::try_from(max_sync)
            .map_err(|_| DecodeError::Malformed("feed_mo0_max_sync_iterations".into()))?;
        let pool_asset = addr("pool_asset")?;
        let loan_asset_0 = addr("loan_asset_0")?;
        if pool_asset == Address::ZERO ||
            loan_asset_0 == Address::ZERO ||
            pool_asset == loan_asset_0
        {
            return Err(DecodeError::Drift("pool_asset / loan_asset_0 are not a pair".into()));
        }
        Ok(Self {
            pool,
            kind,
            implementation: addr("implementation")?,
            hook,
            hook_loan_scale,
            leverage_hook,
            spread_hook,
            router: addr("router")?,
            price_feed: addr("price_feed")?,
            price_feed_sequencer: addr("price_feed_sequencer")?,
            price_feed_sequencer_grace: word("price_feed_sequencer_grace")?,
            factory: addr("factory")?,
            pool_asset,
            loan_asset_0,
            venue_0_account: addr("venue_0_account")?,
            venue_0_market_id: hash("venue_0_market_id")?,
            venue_0_morpho,
            venue_0_irm,
            venue_0_oracle,
            venue_0_oracle_scale_factor: word("venue_0_oracle_scale_factor")?,
            feed_mo0_max_sync_iterations,
            feed_asset_proxy: addr("feed_asset_proxy")?,
            feed_loan0_proxy: addr("feed_loan0_proxy")?,
            feed_seq_proxy: addr("feed_seq_proxy")?,
        })
    }
}

/// The decoded, consistency-checked state of a component: the composed pool state (with the
/// Morpho market oracle still to be evaluated at a clock,
/// [`ProtocolSim::apply_block`][apply_block]) and the scheduled change the quote refuses across.
///
/// [apply_block]: tycho_common::simulation::protocol_sim::ProtocolSim::apply_block
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Core {
    pub flamm: Flamm,
    /// The Morpho market oracle's own feed, the one feed a quote re-reads at each clock; the
    /// asset, loan-0 and sequencer feeds are read once here into `flamm.feed`.
    pub mo0: Feed,
    /// The earliest `executableAt` of a pending implementation, hook set, venue or loan-asset
    /// change (`FLAMMFactory.sol:53-54`, `FLAMMStore.sol:292-297`, `:308-311`), at least 1 while
    /// one is pending and 0 when none is; a quote at or past it is refused.
    pub scheduled_at: u64,
}

fn drift(what: &str) -> DecodeError {
    DecodeError::Drift(what.to_owned())
}

/// Decodes a component's dynamic attributes against its statics into the composed state. The
/// Morpho market oracle fields are left at their defaults (not ok): they are a function of the
/// clock and the caller evaluates them (`FlammPoolState::refresh_oracle`).
pub fn decode_core(st: &Statics, attrs: &Attributes) -> Result<Core, DecodeError> {
    let w = Words::new(attrs);
    let pool = st.pool;
    let ns = |n: u64| FLAMM_NS + U256::from(n);

    // ---- FLAMMStore.S (FLAMMStore.sol:248-311), schema 2.1
    let pool_asset = field_addr(w.owned("pool", ns(0))?, 0);
    if pool_asset != st.pool_asset {
        return Err(drift(&format!("poolAsset {pool_asset} is not {}", st.pool_asset)));
    }
    let router = field_addr(w.owned("pool", ns(1))?, 0);
    let price_feed = field_addr(w.owned("pool", ns(2))?, 0);
    let factory = field_addr(w.owned("pool", ns(5))?, 0);
    if router != st.router || price_feed != st.price_feed || factory != st.factory {
        return Err(drift("pool bindings (router, priceFeed, factory)"));
    }
    let w7 = w.owned("pool", ns(7))?;
    let w10 = w.owned("pool", ns(10))?;
    let addrs = [
        field_addr(w7, 1),
        field_addr(w.owned("pool", ns(8))?, 0),
        field_addr(w.owned("pool", ns(9))?, 0),
        field_addr(w10, 0),
        field_addr(w.owned("pool", ns(24))?, 0),
        field_addr(w.owned("pool", ns(25))?, 0),
        field_addr(w.owned("pool", ns(28))?, 0),
    ];
    // The swap kind fills the invariant, fee, recenter and controller slots with one hook
    // (hook_registry.go resolveHooksIn); no kind fills loanSwapHook; leverage and spread are a
    // pair (FLAMMOpsLib._checkHookSet).
    if addrs[0] != st.hook || addrs[1] != st.hook || addrs[2] != st.hook || addrs[3] != st.hook {
        return Err(drift(
            "the invariant, fee, recenter and controller slots are not the swap hook",
        ));
    }
    if addrs[4] != st.leverage_hook || addrs[5] != st.spread_hook {
        return Err(drift("leverage / spread hook slots"));
    }
    if addrs[6] != Address::ZERO {
        return Err(DecodeError::Unsupported(format!("loanSwapHook {}", addrs[6])));
    }
    if (addrs[4] == Address::ZERO) != (addrs[5] == Address::ZERO) {
        return Err(drift("leverage and spread hooks are set as a pair"));
    }
    if !field_bool(w10, 20) || !field_bool(w10, 21) {
        return Err(drift("pool is not initialized and bootstrapped"));
    }
    let paused = field_bool(w10, 22);
    let lev_paused = field_bool(w10, 23);
    let loan_count = w.owned("pool", ns(11))?;
    if loan_count != U256::from(1u8) {
        return Err(DecodeError::Unsupported(format!("loans.length {loan_count}")));
    }
    let loans_base = array_base(ns(11));
    let l0 = w.owned("pool", loans_base)?;
    let loan_token = field_addr(l0, 0);
    let loan_decimals = field_u64(l0, 20, 1) as u8;
    if loan_token != st.loan_asset_0 {
        return Err(drift(&format!("loans[0].token {loan_token} is not {}", st.loan_asset_0)));
    }
    if loan_decimals > 18 {
        return Err(drift(&format!("loans[0].decimals {loan_decimals}")));
    }
    let scale = U256::from(10u8).pow(U256::from(18 - loan_decimals));
    let scale_word = w.owned("pool", loans_base + U256::from(1u8))?;
    if scale_word != scale {
        return Err(drift(&format!("loans[0].scale {scale_word} is not 10^(18-{loan_decimals})")));
    }
    if st.hook_loan_scale != scale {
        return Err(drift(&format!(
            "hook LOAN_SCALE {} is not loans[0].scale {scale}",
            st.hook_loan_scale
        )));
    }
    // Required: `swapPriceBandWad` is non-zero by `FLAMMOpsLib.initialize` (`FLAMMOpsLib.sol:114`)
    // and stays so through `setLoanConfig` (`:486`); a zero band would refuse every fill.
    let l2 = w.required_owned("pool", loans_base + U256::from(2u8))?;
    let loan_cfg = LoanCfg {
        token: loan_token,
        scale,
        swap_price_band_wad: field(l2, 0, 8),
        fee_floor_wad: field(l2, 8, 8),
        max_swap_notional: w.owned("pool", loans_base + U256::from(3u8))?,
        reserve_target: w.owned("pool", loans_base + U256::from(4u8))?,
        liquid: w.owned("pool", loans_base + U256::from(5u8))?,
    };
    let physical = w.owned("pool", ns(12))?;
    // Required: `phiWad >= phiMinWad > 0` at `initialize` (`FLAMMOpsLib.sol:112`) and inside the
    // envelope at every `setDials` (`FLAMMGateLib.sol:374`); `lastDialMoveTs` is the initialize
    // block's timestamp (`FLAMMOpsLib.sol:148`); `feeCapWad` bounds every fill's fee
    // (`FLAMMSwapLib.sol:196`, zero would clip the fee to nothing). A zero word in any of the
    // three would quote a different amount, not refuse.
    let w13 = w.required_owned("pool", ns(13))?;
    let w15 = w.required_owned("pool", ns(15))?;
    let w16 = w.required_owned("pool", ns(16))?;
    let w22 = w.owned("pool", ns(22))?;
    let gate_pool = Pool {
        physical,
        loans: vec![loan_cfg],
        ltv_wad: field(w13, 24, 8),
        phi_wad: field(w13, 0, 8),
        room_epsilon_wad: field(w15, 14, 8),
        features: w.owned("pool", ns(23))?,
        price_wad: Vec::new(),
        cross_wad: Vec::new(),
    };
    let fee_floor_wad = field(w15, 22, 8);
    let fee_cap_wad = field(w16, 0, 8);
    let last_lever_spread_ppm = field(w22, 20, 4);
    // Required: `bootstrap` mints more than `MINIMUM_SHARES` (`FLAMMFlowLib.sol:601-603`), so a
    // bootstrapped pool has written its supply.
    let share_supply = w.required_owned("pool", ERC20_NS + U256::from(2u8))?;

    // ---- the scheduled changes (tracker_reads.go scheduledChangeAt)
    let f1 = w.owned("factory", U256::from(1u8))?;
    let pending_impl = field_addr(f1, 0);
    let impl_at = field_u64(f1, 20, 6);
    let pending_invariant = field_addr(w.owned("pool", ns(19))?, 0);
    let hook_set_at = field_u64(w22, 24, 6);
    let pending_venue_hash = w.owned("pool", ns(32))?;
    let pending_venue_at = field_u64(w.owned("pool", ns(33))?, 0, 6);
    let pending_loan_hash = w.owned("pool", ns(30))?;
    let pending_loan_at = field_u64(w.owned("pool", ns(31))?, 0, 6);
    let mut scheduled_at = 0u64;
    for (pending, at) in [
        (pending_impl != Address::ZERO || impl_at != 0, impl_at),
        (pending_invariant != Address::ZERO || hook_set_at != 0, hook_set_at),
        (!pending_venue_hash.is_zero() || pending_venue_at != 0, pending_venue_at),
        (!pending_loan_hash.is_zero() || pending_loan_at != 0, pending_loan_at),
    ] {
        let at = at.max(1);
        if pending && (scheduled_at == 0 || at < scheduled_at) {
            scheduled_at = at;
        }
    }

    // ---- FLAMMFactory (FLAMMFactory.sol:52-59), schema 2.5c
    let implementation = field_addr(w.owned("factory", U256::ZERO)?, 0);
    if implementation != st.implementation {
        return Err(drift(&format!(
            "beacon implementation {implementation} is not {}",
            st.implementation
        )));
    }
    if !field_bool(w.owned("factory", factory_is_pool_slot(pool))?, 0) {
        return Err(drift("factory.isPool[pool] is false"));
    }

    // ---- EverlongHook (EverlongHook.sol:94-111), schema 2.2
    let h = |slot: u64| w.owned("hook", U256::from(slot));
    // Required: the words the constructor writes non-zero, so that a lost one is `Missing`
    // rather than a zero that decodes into a state quoting a different amount. `_p = p`
    // (`EverlongHook.sol:157`) puts `Params.aWad | spanUpWad` in slot 0 — `cWad` reverts below
    // `MIN_A_WAD = 5e17 + 1` (`AlmCurve.sol:120`) and `supportFor` reverts unless
    // `spanUpWad > WAD` (`:79`) — and the `Tuning` row's fee parameters in slots 4 and 5 and its
    // inventory surcharge and half-lives in slot 6, the last never zero since
    // `emaHalfLife >= minEmaHalfLife > 0` (`:178-179`, `:205`). `_sup = supportFor(...)` (`:158`)
    // fills 10-13: `aWad` by the same bound, `xLo > MIN_X_WAD` and `xLo < xHi < MAX_X_WAD`
    // (`AlmCurve.sol:85`), and `yHi = yAtX(xHi)`, which is above `yAtX(MAX_X_WAD)` because `yAtX`
    // is decreasing — the one word here whose non-zero rests on the curve's range rather than on
    // a revert. Then `anchorSqrtX96 > MIN_SQRT_PRICE_X96` (`:163`, `:164`) in 14,
    // `reservationPriceWad = anchorPriceWad * WAD` with `anchorPriceWad != 0` (`:152`, `:165`) in
    // 15, `kappa = KAPPA_SEED` and `xWad = HALF` unconditionally (`:166`, `:167`) in 16 and 17,
    // and `reservesAt` in 18 and 20 (`:168`), zero only for a one-wei-wide band that holds
    // nothing. A keeper's retuning or recentre rewrites these words, never removes them. Zero fee
    // parameters would quote a fee-free fill, a zero inventory surcharge a fill without it
    // (`S.fillFee(t.fee, st, !ctx.poolAssetIn, t.invSkewKappaWad, t.invSkewBandWad)`, `:503`),
    // a zero reservation price, support, anchor or book a different curve. Slots 4 and 5 are the
    // weakest of the set: `_validateTuning` admits an all-zero fee row when
    // `bounds.minFeeWad == 0` (`:190`). They stay required because every pool this package
    // indexes writes them, and relaxing them would only weaken the guard on the pool that
    // exists.
    //
    // Optional, and deliberately so: 19 and 21 (`idleStable`, `idleVolatile`), first written by
    // a fill, and 23 (`rvWad`), first written by an observation — the constructor writes 22, 24
    // and 25 but never 23 (`:170-172`). Requiring them would refuse the pool from genesis until
    // its first fill and its first variance print. The required set is therefore a
    // construction-time completeness guard, not a general lost-word detector: a lost slot 23
    // still decodes, and shifts the quote by about 1.3 %.
    let required = |slot: u64| w.required_owned("hook", U256::from(slot));
    let h0 = required(0)?;
    let h4 = required(4)?;
    let h5 = required(5)?;
    let h6 = required(6)?;
    let sup_a_wad = required(10)?;
    // `Params.aWad` and `_sup.aWad` are one value held twice: the constructor derives the second
    // from the first (`EverlongHook.sol:157-158`) and `setCurveConfig` rewrites both from
    // `cfg.concentrationWad` (`:289-290`). A copy that disagrees is a corrupted word, not a pool
    // (`snapshot_decoding_fails_closed` pins the refusal).
    if field(h0, 0, 16) != sup_a_wad {
        return Err(drift("Params.aWad is not _sup.aWad"));
    }
    let hook_state = HookState {
        a_wad: field(h0, 0, 16),
        support: Support {
            a_wad: sup_a_wad,
            x_lo: required(11)?,
            x_hi: required(12)?,
            y_hi: required(13)?,
        },
        anchor_sqrt_x96: field(required(14)?, 0, 20),
        reservation_price_wad: required(15)?,
        kappa: required(16)?,
        x_wad: required(17)?,
        reserve_stable: required(18)?,
        idle_stable: h(19)?,
        reserve_volatile: required(20)?,
        idle_volatile: h(21)?,
        rv_wad: h(23)?,
        fee: FeeParams {
            mid_fee_wad: field(h4, 0, 8),
            out_fee_wad: field(h4, 8, 8),
            gamma_wad: field(h4, 16, 8),
            sigma_ref_wad: field(h4, 24, 8),
            vol_beta_wad: field(h5, 0, 8),
            vol_min_wad: field(h5, 8, 8),
            vol_max_wad: field(h5, 16, 8),
            dir_skew_wad: field(h5, 24, 8),
        },
        inv_skew_kappa_wad: field(h6, 0, 8),
        inv_skew_band_wad: field(h6, 8, 8),
        loan_scale: st.hook_loan_scale,
    };
    let swap = SwapHookSlot { kind: HookKind::EverlongSwapV1, everlong_swap: Some(hook_state) };
    let leverage = if st.leverage_hook != Address::ZERO {
        LeverageHookSlot {
            kind: HookKind::EverlongLeverageV1,
            everlong_leverage: Some(EverlongLeverageV1 {}),
        }
    } else {
        LeverageHookSlot::default()
    };
    // ---- LeverageSpreadHook (LeverageSpreadHook.sol:30-35), schema 2.3
    let spread = if st.spread_hook != Address::ZERO {
        // Required: the constructor writes `lastSetTs = uint48(block.timestamp)`
        // (`LeverageSpreadHook.sol:54`), so the word is non-zero for every parameter set, and a
        // zero decodes into a 17,500 ppm post read as a 0 ppm one that `maxSpreadAge == 0` never
        // lets lapse (`:76-78`), which `FLAMMLeverLib._spread` then floors to
        // `LEV_SPREAD_FLOOR_PPM = 2_500` (`FLAMMLeverLib.sol:23`, `:171`) — a venue that refuses
        // on chain would quote. A pool with no spread hook keeps `SpreadHookSlot::default()`, so
        // the guard belongs inside this branch.
        let s0 = w.required_owned("spread", U256::ZERO)?;
        SpreadHookSlot {
            kind: HookKind::EverlongSpreadV1,
            everlong_spread: Some(SpreadHookState {
                spread: field(s0, 0, 3),
                max_spread_age: field(s0, 9, 4),
                last_set_ts: field(s0, 13, 6),
            }),
        }
    } else {
        SpreadHookSlot::default()
    };

    // ---- MMRouter record (MMRouterLib.sol:40-77), schema 2.4
    let global_paused = field_bool(w.owned("router", U256::ZERO)?, 0);
    let rec = router_record_slot(pool);
    let r0 = w.owned("router", rec)?;
    if field_addr(r0, 0) != st.pool_asset {
        return Err(drift("the Router record's poolAsset"));
    }
    let r1 = w.owned("router", rec + U256::from(1u8))?;
    let router_loans = w.owned("router", rec + U256::from(2u8))?;
    let router_venues = w.owned("router", rec + U256::from(3u8))?;
    if router_loans != U256::from(1u8) {
        return Err(drift(&format!("the Router's loans.length {router_loans}")));
    }
    if router_venues != U256::from(1u8) {
        return Err(DecodeError::Unsupported(format!("venues.length {router_venues}")));
    }
    let rl = array_base(rec + U256::from(2u8));
    let rl0 = w.owned("router", rl)?;
    if field_addr(rl0, 0) != st.loan_asset_0 {
        return Err(drift("the Router's loans[0].token"));
    }
    let rl_scale = w.owned("router", rl + U256::from(1u8))?;
    if rl_scale != scale {
        return Err(drift(&format!("the Router's loans[0].loanScale {rl_scale}")));
    }
    let rl2 = w.owned("router", rl + U256::from(2u8))?;
    let loan = Loan {
        decimals: field_u64(rl0, 20, 1) as u8,
        loan_scale: rl_scale,
        debt_cap: field(rl2, 0, 16),
        supply_cap: field(rl2, 16, 16),
        borrow_enabled: field_bool(rl0, 21),
        retired: field_bool(rl0, 22),
    };
    if loan.decimals != loan_decimals {
        return Err(drift("the Router's loans[0].decimals"));
    }
    let rv = array_base(rec + U256::from(3u8));
    let account = field_addr(w.owned("router", rv)?, 0);
    let market_id = B256::from(w.owned("router", rv + U256::from(1u8))?);
    if account != st.venue_0_account || market_id != st.venue_0_market_id {
        return Err(drift("venues[0].account / id"));
    }
    let v2 = w.owned("router", rv + U256::from(2u8))?;
    let v3 = w.owned("router", rv + U256::from(3u8))?;
    let venue_kind = field_u64(v2, 0, 1) as u8;
    let loan_index = field_u64(v2, 1, 1) as u8;
    if loan_index != 0 {
        return Err(drift(&format!("venues[0].loanIndex {loan_index}")));
    }
    let mut orders = Vec::with_capacity(4);
    for k in 4u8..8 {
        let len = w.owned("router", rec + U256::from(k))?;
        let len = usize::try_from(len).map_err(|_| drift("an order length"))?;
        if len > 16 * 4 {
            return Err(DecodeError::Unsupported(format!("an order of {len} entries")));
        }
        let base = array_base(rec + U256::from(k));
        let mut order = Vec::with_capacity(len);
        for i in 0..len {
            let word = w.owned("router", base + U256::from(i / 16))?;
            let id = field_u64(word, 2 * (i % 16), 2) as u16;
            if id != 0 {
                return Err(drift(&format!(
                    "a priority names venue {id}, which the record has not"
                )));
            }
            order.push(id);
        }
        orders.push(order);
    }

    // ---- MorphoBlueAccount (MorphoBlueAccount.sol:58-63), schema 2.5a
    let a = |slot: u64| w.owned("account", U256::from(slot));
    if field_addr(a(0)?, 0) != st.router ||
        field_addr(a(1)?, 0) != pool ||
        field_addr(a(2)?, 0) != st.pool_asset ||
        field_addr(a(3)?, 0) != st.loan_asset_0 ||
        field_addr(a(4)?, 0) != st.venue_0_morpho
    {
        return Err(drift(
            "the financing account's ROUTER / POOL / POOL_ASSET / LOAN_ASSET / MORPHO",
        ));
    }
    let ms = account_market_slot(market_id);
    let m0 = w.owned("account", ms)?;
    let m1 = w.owned("account", ms + U256::from(1u8))?;
    let oracle = field_addr(m0, 0);
    let market_lltv = field(m0, 20, 8);
    let irm = field_addr(m1, 0);
    if oracle != st.venue_0_oracle || irm != st.venue_0_irm {
        return Err(drift("the financing account's market oracle / irm"));
    }
    let has_irm = irm != Address::ZERO;

    // ---- Morpho Blue and the IRM (schema 2.5d, 2.5e, 2.8)
    let mm0 = w.required_word("mm:0:market:0")?;
    let mm1 = w.required_word("mm:0:market:1")?;
    let mm2 = w.required_word("mm:0:market:2")?;
    let p0 = w.required_word("mm:0:position:0")?;
    let p1 = w.required_word("mm:0:position:1")?;
    let rate_at_target =
        if has_irm { w.required_word("irm:0:rate_at_target")? } else { U256::ZERO };
    if rate_at_target.bit(255) {
        return Err(DecodeError::Unsupported("a negative rateAtTarget".into()));
    }
    let morpho = VenueMarket {
        market: Market {
            total_supply_assets: field(mm0, 0, 16),
            total_supply_shares: field(mm0, 16, 16),
            total_borrow_assets: field(mm1, 0, 16),
            total_borrow_shares: field(mm1, 16, 16),
            last_update: field(mm2, 0, 16),
            fee: field(mm2, 16, 16),
        },
        position: Position {
            supply_shares: p0,
            borrow_shares: field(p1, 0, 16),
            collateral: field(p1, 16, 16),
        },
        lltv: market_lltv,
        has_irm,
        // The port evaluates the AdaptiveCurveIrm itself (`irm`), which cannot revert at a clock
        // past the market's last update; the account's own grace / quarantine branches run on
        // the rate it computes.
        irm_readable: true,
        rate_at_target,
        oracle_ok: false,
        oracle_price: U256::ZERO,
        oracle_zero: false,
    };
    let venue = Venue {
        morpho,
        kind: venue_kind,
        loan_index,
        lltv_wad: field(v2, 2, 8),
        borrow_enabled: field_bool(v2, 10),
        supply_enabled: field_bool(v2, 11),
        retired: field_bool(v2, 12),
        debt_cap: field(v2, 13, 16),
        supply_cap: field(v3, 0, 16),
        max_borrow_rate_wad: field(v3, 16, 8),
        managed_collateral: w.owned("router", rv + U256::from(4u8))?,
        managed_supply_shares: w.owned("router", rv + U256::from(5u8))?,
    };
    let mut orders = orders.into_iter();
    let mm_router = MmRouter {
        global_paused,
        pin_ltv_wad: field(r0, 20, 8),
        safety_gap_wad: field(r1, 0, 8),
        oracle_band_wad: field(r1, 8, 8),
        max_drawn_assets: field_u64(r0, 28, 1) as u8,
        loans: vec![loan],
        venues: vec![venue],
        borrow_order: orders.next().unwrap_or_default(),
        supply_order: orders.next().unwrap_or_default(),
        withdraw_order: orders.next().unwrap_or_default(),
        repay_order: orders.next().unwrap_or_default(),
        transient_repay: Default::default(),
    };

    // ---- the feeds (schema 2.6), PriceFeed token config (PriceFeed.sol:13-19), schema 2.5b
    let asset = Feed::decode(FeedRole::Asset, attrs)?;
    let loan0 = Feed::decode(FeedRole::Loan0, attrs)?;
    let seq = Feed::decode(FeedRole::Seq, attrs)?;
    let mo0 = Feed::decode(FeedRole::Mo0, attrs)?;
    for f in [&asset, &loan0, &seq] {
        if f.kind == FeedKind::Dual {
            return Err(DecodeError::Unsupported(format!(
                "feed {} is a DualAggregator, which PriceFeed would read on its primary path",
                f.role.name()
            )));
        }
    }
    if mo0.kind != FeedKind::Dual {
        return Err(DecodeError::Unsupported(format!(
            "feed mo0 is {:?}, not the DualAggregator",
            mo0.kind
        )));
    }
    let token = |t: Address, proxy: Address, feed: &Feed| -> Result<FeedToken, DecodeError> {
        let base = pricefeed_token_slot(t);
        // Required once the statics name a proxy for the token: `_tokens[token] = Token({...})`
        // is a whole-struct write in the constructor, the only write there is
        // (`PriceFeed.sol:54-60` — the mapping has no setter), and it stores `heartbeat != 0`
        // (`:50`), `scale = 10 ** (18 - feedDec) >= 1` and `unit = 10 ** dec >= 1`, so both words
        // are present for every registered token. A lost first word reads the aggregator as zero
        // and is caught by the drift check below; a lost second word reads `unit = 0` and deletes
        // the peg band, which quotes full-size sells the chain refuses. An unregistered token
        // keeps `owned`, so the `aggregator == Address::ZERO` branch below stays reachable.
        let (t0, t1) = if proxy != Address::ZERO {
            (
                w.required_owned("pricefeed", base)?,
                w.required_owned("pricefeed", base + U256::from(1u8))?,
            )
        } else {
            (w.owned("pricefeed", base)?, w.owned("pricefeed", base + U256::from(1u8))?)
        };
        let aggregator = field_addr(t0, 0);
        if aggregator != proxy {
            return Err(drift(&format!(
                "PriceFeed._tokens[{t}].aggregator {aggregator} is not {proxy}"
            )));
        }
        let round =
            if aggregator == Address::ZERO { FeedRound::default() } else { feed.feed_round()? };
        Ok(FeedToken {
            known: aggregator != Address::ZERO,
            heartbeat: field(t0, 20, 4),
            scale: field(t0, 24, 8),
            unit: field(t1, 0, 8),
            peg_band_wad: field(t1, 8, 8),
            round,
        })
    };
    let feed = PriceFeedState {
        has_sequencer: st.price_feed_sequencer != Address::ZERO,
        sequencer_grace: st.price_feed_sequencer_grace,
        sequencer: if st.price_feed_sequencer != Address::ZERO {
            if st.price_feed_sequencer != st.feed_seq_proxy {
                return Err(drift("SEQUENCER_FEED is not feed_seq_proxy"));
            }
            seq.feed_round()?
        } else {
            FeedRound::default()
        },
        asset: token(st.pool_asset, st.feed_asset_proxy, &asset)?,
        loans: vec![token(st.loan_asset_0, st.feed_loan0_proxy, &loan0)?],
    };

    let flamm = Flamm {
        block: 0,
        timestamp: 0,
        pool_asset,
        pool: gate_pool,
        paused,
        lev_paused,
        fee_floor_wad,
        fee_cap_wad,
        share_supply,
        last_lever_spread_ppm,
        hooks: PoolHooks { swap, leverage, spread },
        router: mm_router,
        feed,
    };
    Ok(Core { flamm, mo0, scheduled_at })
}

/// What a snapshot decodes into, or why it is refused: the static identity, the component's
/// tokens (the pair `[pool asset, loan asset 0]` of the statics) and the attributes.
fn decode_snapshot(
    snapshot: &ComponentWithState,
) -> Result<(Statics, Attributes, Core), DecodeError> {
    let statics = Statics::parse(&snapshot.component.id, &snapshot.component.static_attributes)?;
    let tokens: Vec<Address> = snapshot
        .component
        .tokens
        .iter()
        .map(|t| address_of("component token", t))
        .collect::<Result<_, _>>()?;
    if tokens != [statics.pool_asset, statics.loan_asset_0] {
        return Err(DecodeError::Drift(format!(
            "component tokens {tokens:?} are not [pool_asset, loan_asset_0]"
        )));
    }
    let attrs: Attributes = snapshot
        .state
        .attributes
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let core = decode_core(&statics, &attrs)?;
    Ok((statics, attrs, core))
}

/// The inclusion filter to register with the `flamm` exchange: admits a component whose snapshot
/// decodes and excludes one the decoder would refuse (code or an immutable the port does not
/// model, a wiring drift, a configuration it does not quote, an attribute it needs that is
/// absent). A refused snapshot is otherwise fatal to the whole stream unless the consumer opted
/// into `skip_state_decode_failures`; with the filter the component is skipped and logged. Pure:
/// the same decode the snapshot runs, with no side effect.
pub fn flamm_filter(component: &ComponentWithState) -> bool {
    match decode_snapshot(component) {
        Ok(_) => true,
        Err(e) => {
            tracing::debug!(
                pool = component.component.id,
                reason = %e,
                "Excluding flamm component: its snapshot does not decode"
            );
            false
        }
    }
}

impl TryFromWithBlock<ComponentWithState, BlockHeader> for FlammPoolState {
    type Error = InvalidSnapshotError;

    /// Builds the component's state from its snapshot: the static identity, then the attributes,
    /// at the header's timestamp as the first execution clock (the stream decoder advances it
    /// with [`ProtocolSim::apply_block`][apply_block] right after). The component's tokens must
    /// be the pair `[pool asset, loan asset 0]` of the statics.
    ///
    /// [apply_block]: tycho_common::simulation::protocol_sim::ProtocolSim::apply_block
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        _all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let (statics, attrs, core) = decode_snapshot(&snapshot)?;
        Ok(FlammPoolState::new(
            snapshot.component.id.clone(),
            Arc::new(statics),
            Arc::new(attrs),
            Ok(core),
            block.number,
            block.timestamp,
        ))
    }
}
