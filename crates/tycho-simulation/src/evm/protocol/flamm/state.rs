// Copyright (c) 2026 Everlong Labs Limited

//! The composed pool state the swap and leverage entries evaluate (c104 @ `80abd43`):
//! `FLAMMStore`'s ledger, dials, limits and switches (`src/core/flamm/FLAMMStore.sol`), the Router
//! record with every venue's Morpho market (`src/core/mm`, behind [`Router`]), the pool's hooks as
//! their kinds' states (the swap hook's storage, `src/hooks/everlong/EverlongHook.sol`, behind
//! [`SwapHook`]; the spread hook's post, `src/hooks/everlong/lev/LeverageSpreadHook.sol`,
//! [`SpreadHookState`]; the leverage hook is stateless and reads the swap hook's book, behind
//! [`LeverageHook`]) and the `PriceFeed`'s inputs (`src/core/PriceFeed.sol`, [`PriceFeedState`]).
//! Port of `state.go`.
//!
//! Every entry takes the block timestamp it runs at. Almost nothing in the state is a price or an
//! accrual already evaluated at the snapshot: the feed checks, the Morpho accrual and the spread's
//! age are all recomputed at `now`. The one exception is a venue's Morpho market oracle answer
//! (`morpho::VenueMarket::oracle_price`, `morpho.go` `mmVenueMarket.OraclePrice`), which is read
//! once and frozen although the chain's own answer can move with the clock alone (the BTC/USD feed
//! behind the c104 market oracle is a Chainlink SVR `DualAggregator`, which reveals each withheld
//! primary round when `block.timestamp` passes it). Only `MMRouterLib.bandOk` and Morpho's
//! `_isHealthy` read that price, never the quote. Subject to that, a state read at block B quotes
//! exactly what the chain would at any later timestamp with no transaction in between.

use alloy::primitives::{Address, U256};

use super::{
    deps::{LeverageHook, Router, SwapHook},
    error::FlammError,
    gate::{self, Book, Pool},
    math::{mul_div, PPM, WAD},
    pricefeed::PriceFeedState,
};

/// `FLAMMStore`'s curator feature bitmap, as the masks `requireFeature` tests
/// (`FLAMMStore.sol:190-200`, `:362`): `FEATURE_SWAP_SELL = 1 << 1`.
pub const FEATURE_SWAP_SELL: U256 = U256::from_limbs([1 << 1, 0, 0, 0]);
/// `FLAMMStore.FEATURE_SWAP_BUY = 1 << 2`.
pub const FEATURE_SWAP_BUY: U256 = U256::from_limbs([1 << 2, 0, 0, 0]);
/// `FLAMMStore.FEATURE_SUPPLY_LENDING = 1 << 3`, the mask `takeLoan` tests.
pub use super::gate::FEATURE_SUPPLY_LENDING;
/// `FLAMMStore.FEATURE_LEVERAGE = 1 << 5`.
pub const FEATURE_LEVERAGE: U256 = U256::from_limbs([1 << 5, 0, 0, 0]);

/// `IFLAMMHooks.PoolContext` (`IFLAMMHooks.sol:10`), the one hook frame every route builds
/// through [`gate::context`] (`FLAMMGateLib.context`).
pub use super::context::PoolContext;

/// `LeverageSpreadHook`'s storage core's staticcall reads (`LeverageSpreadHook.sol:30-35`):
/// `spread` (`uint24`), `maxSpreadAge` (`uint32`), `lastSetTs` (`uint48`).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct SpreadHookState {
    pub spread: U256,
    pub max_spread_age: U256,
    pub last_set_ts: U256,
}

impl SpreadHookState {
    /// `LeverageSpreadHook.spreadPpm` (`LeverageSpreadHook.sol:75-79`): no answer once a non-zero
    /// `maxSpreadAge` has elapsed since the last post (`block.timestamp > lastSetTs +
    /// maxSpreadAge`).
    pub fn spread_ppm(&self, now: u64) -> (bool, U256) {
        if !self.max_spread_age.is_zero() {
            // uint48 + uint32 in uint256 arithmetic: cannot overflow for stored widths.
            let deadline = self
                .last_set_ts
                .saturating_add(self.max_spread_age);
            if U256::from(now) > deadline {
                return (false, U256::ZERO);
            }
        }
        (true, self.spread)
    }

    /// Whether the post answers through `now + margin` (a live post is
    /// `block.timestamp <= lastSetTs + maxSpreadAge`, or `maxSpreadAge == 0`) with a ppm below
    /// `PPM`.
    pub fn live_through(&self, now: u64, margin: u64) -> bool {
        if self.spread >= PPM {
            return false;
        }
        if !self.max_spread_age.is_zero() {
            let deadline = self
                .last_set_ts
                .saturating_add(self.max_spread_age);
            let Some(at) = now.checked_add(margin) else {
                return false;
            };
            if U256::from(at) > deadline {
                return false;
            }
        }
        true
    }

    /// The deadline a post that answers at `now` stops answering after: `(lastSetTs,
    /// maxSpreadAge)`, `None` when the post does not answer or has no staleness window.
    pub fn expiry(&self, now: u64) -> Option<(U256, U256)> {
        let (live, _) = self.spread_ppm(now);
        if !live || self.max_spread_age.is_zero() {
            return None;
        }
        Some((self.last_set_ts, self.max_spread_age))
    }
}

/// A hook role's kind, as the registry names it (`hook_registry.go`): the pool's `hooks()` roles
/// are held as a kind tag beside that kind's concrete state. `None` is an empty role.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash,
)]
pub enum HookKind {
    #[default]
    None,
    /// `EverlongHook` ([`super::hook::HookState`]).
    EverlongSwapV1,
    /// `EverlongLeverageHook` ([`super::deps::EverlongLeverageV1`]), stateless.
    EverlongLeverageV1,
    /// `LeverageSpreadHook` ([`SpreadHookState`]).
    EverlongSpreadV1,
}

/// The swap role: its kind and that kind's state.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct SwapHookSlot<H> {
    pub kind: HookKind,
    pub everlong_swap: Option<H>,
}

impl<H: SwapHook> SwapHookSlot<H> {
    /// The role's port. A swap role without one is a malformed state (the Go port's
    /// `errHookUnported`, an integration refusal): the decoder must refuse such a state before
    /// quoting, so this reads as `HookInvalid()`, the pool's own vocabulary for an unusable
    /// hook set, which no quote path reaches otherwise.
    pub fn port(&self) -> Result<&H, FlammError> {
        match (self.kind, &self.everlong_swap) {
            (HookKind::EverlongSwapV1, Some(h)) => Ok(h),
            _ => Err(FlammError::HookInvalid),
        }
    }

    /// The role's port, mutably (the execution commits the fill's book in place).
    pub fn port_mut(&mut self) -> Result<&mut H, FlammError> {
        match (self.kind, &mut self.everlong_swap) {
            (HookKind::EverlongSwapV1, Some(h)) => Ok(h),
            _ => Err(FlammError::HookInvalid),
        }
    }
}

/// The leverage role: its kind and the (stateless) port.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct LeverageHookSlot<L> {
    pub kind: HookKind,
    pub everlong_leverage: Option<L>,
}

impl<L: LeverageHook> LeverageHookSlot<L> {
    pub fn port(&self) -> Result<&L, FlammError> {
        match (self.kind, &self.everlong_leverage) {
            (HookKind::EverlongLeverageV1, Some(l)) => Ok(l),
            _ => Err(FlammError::HookInvalid),
        }
    }
}

/// The spread role: its kind and that kind's state.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct SpreadHookSlot {
    pub kind: HookKind,
    pub everlong_spread: Option<SpreadHookState>,
}

impl SpreadHookSlot {
    pub fn port(&self) -> Result<&SpreadHookState, FlammError> {
        match (self.kind, &self.everlong_spread) {
            (HookKind::EverlongSpreadV1, Some(s)) => Ok(s),
            _ => Err(FlammError::HookInvalid),
        }
    }

    /// What `FLAMMLeverLib._spread`'s staticcall gets at `now` (`FLAMMLeverLib.sol:163-165`). An
    /// empty spread role answers nothing: the staticcall to `address(0)` succeeds with empty
    /// data, which does not decode.
    pub fn spread_ppm(&self, now: u64) -> Result<(bool, U256), FlammError> {
        if self.kind == HookKind::None {
            return Ok((false, U256::ZERO));
        }
        Ok(self.port()?.spread_ppm(now))
    }
}

/// The pool's hook set: per role, the kind the registry names with that kind's state. The listed
/// addresses (`hooks()`: invariant, fee, recenter, controller, leverage, spread, loanSwap) are not
/// carried: [`decode_core`](super::decoder::decode_core) reads all seven and refuses any set that
/// is not the statics' own hooks, so after a decode they hold nothing the statics do not.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct PoolHooks<H, L> {
    pub swap: SwapHookSlot<H>,
    pub leverage: LeverageHookSlot<L>,
    pub spread: SpreadHookSlot,
}

impl<H, L> PoolHooks<H, L> {
    /// `$.leverageHook != address(0)` (`FLAMMLeverLib.sol:81`).
    pub fn has_leverage(&self) -> bool {
        self.leverage.kind != HookKind::None
    }

    pub fn has_spread(&self) -> bool {
        self.spread.kind != HookKind::None
    }
}

/// One pool's complete swap-path state (`state.go` `flammState`). `pool.price_wad` /
/// `pool.cross_wad` are not state: [`priced`] fills them from `feed` at the timestamp of the call.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct FlammState<H, L, R> {
    /// `block` / `timestamp` identify the snapshot the state was read at; entries run at their own
    /// timestamp.
    pub block: u64,
    pub timestamp: u64,
    pub pool_asset: Address,
    /// `FLAMMStore`'s physical balance, loan set (with each asset's band, fee floor, notional cap,
    /// reserve target and liquid) and the ltv / phi / roomEpsilon dials and feature bits
    /// ([`gate::Pool`]).
    pub pool: Pool,
    pub paused: bool,
    pub lev_paused: bool,
    pub fee_floor_wad: U256,
    pub fee_cap_wad: U256,
    pub share_supply: U256,
    /// `FLAMMStore.lastLeverSpreadPpm`, the lever-down degrade value (no view: storage, `uint32`).
    pub last_lever_spread_ppm: U256,
    /// `hooks()` with each role's kind and state; an empty leverage role is `LeverageDisabled`
    /// (`FLAMMLeverLib.sol:81`), an empty spread role answers no spread.
    pub hooks: PoolHooks<H, L>,
    pub router: R,
    pub feed: PriceFeedState,
}

impl<H, L, R> FlammState<H, L, R> {
    /// `FLAMMStore.requireFeature` (`FLAMMStore.sol:362`): `FeatureDisabled(bitIndex)` when
    /// `features & mask == 0`.
    pub fn feature(&self, mask: U256) -> Result<(), FlammError> {
        if (self.pool.features & mask).is_zero() {
            return Err(FlammError::FeatureDisabled);
        }
        Ok(())
    }

    /// `FLAMMStore.price` (`FLAMMStore.sol:354`): the checked loan-asset-0 / poolAsset cross,
    /// reverting as `PriceFeed.cross` does.
    pub fn price(&self, now: u64) -> Result<(U256, u64), FlammError> {
        let Some(loan0) = self.feed.loans.first() else {
            return Err(FlammError::PanicIndex);
        };
        self.feed
            .cross(&self.feed.asset, loan0, now)
    }

    /// `FLAMMStore.pegOk` (`FLAMMStore.sol:358`): `PriceFeed.pegOk(loans[idx].token)`, whose
    /// `UnknownToken` is not caught.
    pub fn peg_ok(&self, idx: usize, now: u64) -> Result<bool, FlammError> {
        let Some(loan) = self.feed.loans.get(idx) else {
            return Err(FlammError::PanicIndex);
        };
        self.feed.peg_ok(loan, now)
    }

    /// `FLAMMSwapLib.pair` (`FLAMMSwapLib.sol:112`): exactly one side is poolAsset and the other a
    /// live loan asset. Returns `(idx, poolAssetIn)`.
    pub fn pair(&self, token_in: Address, token_out: Address) -> Result<(u8, bool), FlammError> {
        let (pool_asset_in, other) = if token_in == self.pool_asset {
            (true, token_out)
        } else if token_out == self.pool_asset {
            (false, token_in)
        } else {
            return Err(FlammError::InvalidPair);
        };
        for (i, loan) in self.pool.loans.iter().enumerate() {
            if loan.token == other {
                return Ok((i as u8, pool_asset_in));
            }
        }
        Err(FlammError::InvalidPair)
    }
}

/// `FLAMMGateLib.priced` (`FLAMMGateLib.sol:196`) over the live feed: the price-free book (Router
/// positions at `now`, [`gate::book_of`]), then `priceIn` (`:179`) evaluated at `now`: every
/// leg's `peekCross`, and for loan assets past the first the USD ratio `crossWad = mulDiv(usd_i,
/// WAD, usd_0)` when both USD peeks answer and `usd_0 != 0`. The frame is also written to
/// `pool.price_wad` / `pool.cross_wad`, where [`gate::priced`] and the Router settlement legs
/// (reclaim's price vector, the gate re-assertions) read it at the same timestamp.
///
/// A free function over the pieces it touches (`feed`, `router` read; `pool` written) so that an
/// execution can price the post-state's own ledger in place.
pub fn priced<R: Router>(
    feed: &PriceFeedState,
    router: &R,
    pool: &mut Pool,
    now: u64,
) -> Result<Book, FlammError> {
    let mut b = gate::book_of(pool, router, now)?;
    let n = b.legs.len();
    if feed.loans.len() < n {
        return Err(FlammError::PanicIndex);
    }
    let mut price_wad = vec![U256::ZERO; n];
    let mut cross_wad = vec![U256::ZERO; n];
    if n > 0 {
        let (ok, p, _) = feed.peek_cross(&feed.asset, &feed.loans[0], now);
        if ok {
            price_wad[0] = p;
        }
        cross_wad[0] = WAD;
    }
    if n > 1 {
        let (ok_usd0, usd0, _) = feed.peek_usd(&feed.loans[0], now);
        for i in 1..n {
            let (ok, p, _) = feed.peek_cross(&feed.asset, &feed.loans[i], now);
            if ok {
                price_wad[i] = p;
            }
            let (ok_usd, usd_i, _) = feed.peek_usd(&feed.loans[i], now);
            if ok_usd0 && ok_usd && !usd0.is_zero() {
                cross_wad[i] = mul_div(usd_i, WAD, usd0)?;
            }
        }
    }
    for (i, leg) in b.legs.iter_mut().enumerate() {
        leg.price_wad = price_wad[i];
        leg.cross_wad = cross_wad[i];
    }
    pool.price_wad = price_wad;
    pool.cross_wad = cross_wad;
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::{
        super::{deps::EverlongLeverageV1, hook::HookState, router::Router as MmRouter},
        *,
    };

    type State = FlammState<HookState, EverlongLeverageV1, MmRouter>;

    fn post(spread: u64, age: u64, last: u64) -> SpreadHookState {
        SpreadHookState {
            spread: U256::from(spread),
            max_spread_age: U256::from(age),
            last_set_ts: U256::from(last),
        }
    }

    #[test]
    fn spread_post_ages_inclusively() {
        let p = post(17_500, 3600, 1_000_000);
        assert_eq!(p.spread_ppm(1_003_600), (true, U256::from(17_500u64)));
        assert_eq!(p.spread_ppm(1_003_601), (false, U256::ZERO));
        assert!(p.live_through(1_003_540, 60));
        assert!(!p.live_through(1_003_541, 60));
        assert!(!p.live_through(u64::MAX, 1));
        assert_eq!(p.expiry(1_003_600), Some((U256::from(1_000_000u64), U256::from(3600u64))));
        assert_eq!(p.expiry(1_003_601), None);
        // maxSpreadAge 0: never stale, no expiry.
        let p = post(17_500, 0, 1);
        assert_eq!(p.spread_ppm(u64::MAX), (true, U256::from(17_500u64)));
        assert!(p.live_through(u64::MAX, 0));
        assert_eq!(p.expiry(5), None);
        // A ppm at PPM is live to the hook but not to the margin check.
        let p = post(1_000_000, 0, 1);
        assert_eq!(p.spread_ppm(5), (true, PPM));
        assert!(!p.live_through(5, 0));
    }

    #[test]
    fn slots_resolve_by_kind() {
        let mut s = State::default();
        assert_eq!(s.hooks.swap.port().err(), Some(FlammError::HookInvalid));
        s.hooks.swap = SwapHookSlot {
            kind: HookKind::EverlongSwapV1,
            everlong_swap: Some(HookState::default()),
        };
        assert!(s.hooks.swap.port().is_ok());
        s.hooks.swap.kind = HookKind::None;
        assert_eq!(s.hooks.swap.port().err(), Some(FlammError::HookInvalid));
        assert_eq!(s.hooks.spread.spread_ppm(0), Ok((false, U256::ZERO)));
        s.hooks.spread.kind = HookKind::EverlongSpreadV1;
        assert_eq!(s.hooks.spread.spread_ppm(0), Err(FlammError::HookInvalid));
        s.hooks.spread.everlong_spread = Some(post(9, 0, 0));
        assert_eq!(s.hooks.spread.spread_ppm(0), Ok((true, U256::from(9u64))));
        assert!(!s.hooks.has_leverage());
        assert_eq!(s.hooks.leverage.port().err(), Some(FlammError::HookInvalid));
    }

    #[test]
    fn features_and_pair() {
        let mut s = State::default();
        s.pool.features = U256::from(0b10_0110u64); // sell, buy, leverage
        assert!(s.feature(FEATURE_SWAP_SELL).is_ok());
        assert!(s.feature(FEATURE_SWAP_BUY).is_ok());
        assert!(s.feature(FEATURE_LEVERAGE).is_ok());
        assert_eq!(s.feature(FEATURE_SUPPLY_LENDING), Err(FlammError::FeatureDisabled));
        let a = Address::repeat_byte(1);
        let u = Address::repeat_byte(2);
        let x = Address::repeat_byte(3);
        s.pool_asset = a;
        s.pool
            .loans
            .push(gate::LoanCfg { token: u, ..Default::default() });
        assert_eq!(s.pair(a, u), Ok((0, true)));
        assert_eq!(s.pair(u, a), Ok((0, false)));
        assert_eq!(s.pair(a, a), Err(FlammError::InvalidPair));
        assert_eq!(s.pair(u, u), Err(FlammError::InvalidPair));
        assert_eq!(s.pair(a, x), Err(FlammError::InvalidPair));
        assert_eq!(s.pair(x, u), Err(FlammError::InvalidPair));
        assert_eq!(s.price(0), Err(FlammError::PanicIndex));
        assert_eq!(s.peg_ok(0, 0), Err(FlammError::PanicIndex));
    }
}
