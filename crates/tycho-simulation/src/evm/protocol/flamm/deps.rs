// Copyright (c) 2026 Everlong Labs Limited

//! The seam between the pool core (`state`, `swap`, `lever`, `pricefeed`) and the modules it
//! composes: the swap hook (`hook`), the financing Router (`router`) and the leverage hook
//! (`levhook`). Everything the core needs from them is one of the three traits below, with the
//! exact signatures the Go port calls through (`hook_kinds.go` `swapHookPort` /
//! `leverageHookPort`, `router.go` `mmRouter`), implemented here for the real modules:
//!
//! - [`SwapHook`] for [`HookState`]: `spot` is `EverlongHook.spot` (`hook.go` `spot`),
//!   `preview_fee_wad` is `previewFeeWad` / `executeFeeWad` (`_fee`), `fill` is `_fill` returning
//!   the fill and the book it materialised (the book `commit` then writes, `_commit`), `book_for`
//!   is `bookFor` / `_book` and `reservation_price` is `reservationPriceWad()`. The core calls
//!   `fill` for both the preview and the execution: `executeExactIn` is the same pure function of
//!   the same storage and context, so `FeeMismatch` / `FillMismatch` cannot fire and the plan's
//!   book is committed as is (`swap.go` `executeSwap`).
//! - [`LeverageHook`] for [`EverlongLeverageV1`], the stateless `EverlongLeverageHook`:
//!   `preview_lever` is [`levhook::quote`] (`levhook.go` `levQuote`), the body of `previewLever`
//!   and `executeLever` alike. The core builds the [`LevBook`] from the swap hook's
//!   `book_for(ctx.pool)` and `reservation_price()` (`hook_kinds.go`
//!   `everlongLeverageV1.previewLever`).
//! - [`Router`] for the tracked Router record [`MmRouter`] (`router::Router`, aliased so the record
//!   and the trait read apart): `positions` and `quarantine` are the two Router views the gate
//!   composites make ([`GateReads`], a supertrait here), `funding_ceiling` is
//!   `MMRouter.fundingCeiling(pool, idx, collateral, priceWad)`, `settle_sell_legs` /
//!   `settle_buy_legs` are `router::settle_sell_legs` / `settle_buy_legs` (`router.go`
//!   `mmSettleSellLegs` / `mmSettleBuyLegs`, the bodies of `FLAMMSwapLib.settleSell` and of the buy
//!   branch of `execute`, writing through the pool ledger and the Router record), and
//!   `end_transaction` drops the per-transaction repay snapshots (EIP-1153). The gate storage
//!   composites those legs call (`assertGate`, `anchor`, `assertEntryGate`, `releaseExcess`,
//!   `reclaim`'s price vector) read the price frame [`super::state::priced`] wrote into
//!   [`gate::Pool::price_wad`] / [`gate::Pool::cross_wad`] at the same timestamp; the core clears
//!   that frame once the transaction has settled.
//!
//! The traits keep the pool core generic over its hooks and Router so that the flows can be
//! exercised over scripted doubles; the deployed pool is the instantiation over the three real
//! modules ([`super::Flamm`]). The types that cross the seam are declared once each:
//! [`PoolContext`] (what `FLAMMGateLib.context` builds), [`SwapContext`], [`hook::Book`],
//! [`hook::FillResult`], [`LeverContext`], [`LevBook`], [`LevFill`],
//! [`super::router::Positions`], [`super::router::Quarantine`] and the gate ledger [`gate::Pool`] /
//! [`gate::LoanCfg`] / [`gate::Book`] / [`gate::Leg`].

use alloy::primitives::U256;

use super::{
    context::{LeverContext, PoolContext, SwapContext},
    error::FlammError,
    gate::{self, GateReads},
    hook::{self, HookState},
    levhook::{self, LevBook, LevFill},
    router::{self, Router as MmRouter},
};

/// A swap-role hook on the swap path (`hook_kinds.go` `swapHookPort` plus `levBookSource`): spot,
/// the fee role's `previewFeeWad` / `executeFeeWad`, the invariant role's `previewExactIn` /
/// `executeExactIn` (`FLAMMSwapLib.sol:84`, `:87`, `:149`, `:187`, `:197`) with the book the fill
/// materialised, which the execution commits; and the two reads the leverage hook makes of it
/// (`EverlongLeverageHook.sol:37`, `:48-52`).
pub trait SwapHook: Clone {
    /// `EverlongHook.spot`: the spot at the stored coordinate in the pool's price units (the
    /// context is ignored on chain and not taken).
    fn spot(&self) -> Result<U256, FlammError>;
    /// `EverlongHook.previewFeeWad` / `executeFeeWad`: the fill fee on the book as the execution
    /// would materialise it (the pool's `spotBefore` argument is ignored on chain and not
    /// taken).
    fn preview_fee_wad(&self, ctx: &SwapContext) -> Result<U256, FlammError>;
    /// `EverlongHook._fill`: the exact-input fill at `fee_wad` and the book it leaves; a retracted
    /// book, a zero input or a fee at 100% fills nothing (a zero `amount_in_used`).
    fn fill(
        &self,
        ctx: &SwapContext,
        fee_wad: U256,
    ) -> Result<(hook::FillResult, hook::Book), FlammError>;
    /// `EverlongHook._commit`: stores the book (kappa, reserves, idle, x).
    fn commit(&mut self, book: &hook::Book);
    /// `EverlongHook.bookFor` / `_book`: the stored book rescaled lazily to the context's gross
    /// poolAsset.
    fn book_for(&self, ctx: &PoolContext) -> Result<hook::Book, FlammError>;
    /// `EverlongHook.reservationPriceWad()`.
    fn reservation_price(&self) -> U256;
}

impl SwapHook for HookState {
    fn spot(&self) -> Result<U256, FlammError> {
        HookState::spot(self)
    }

    fn preview_fee_wad(&self, ctx: &SwapContext) -> Result<U256, FlammError> {
        HookState::preview_fee_wad(self, ctx)
    }

    fn fill(
        &self,
        ctx: &SwapContext,
        fee_wad: U256,
    ) -> Result<(hook::FillResult, hook::Book), FlammError> {
        HookState::fill(self, ctx, fee_wad)
    }

    fn commit(&mut self, book: &hook::Book) {
        HookState::commit(self, book)
    }

    fn book_for(&self, ctx: &PoolContext) -> Result<hook::Book, FlammError> {
        HookState::book_for(self, ctx)
    }

    /// `EverlongHook.reservationPriceWad()` (`EverlongHook.sol:103`): the stored word.
    fn reservation_price(&self) -> U256 {
        self.reservation_price_wad
    }
}

/// A leverage-role hook's `ILeverageInvariantHook.previewLever` / `executeLever`
/// (`FLAMMLeverLib.sol:100`, `:128`, `:183`) over the pool's swap hook's book (`hook_kinds.go`
/// `leverageHookPort`; `levhook.go` `levQuote`).
pub trait LeverageHook: Clone {
    fn preview_lever(&self, ctx: &LeverContext, book: &LevBook) -> Result<LevFill, FlammError>;
}

/// `EverlongLeverageHook` (`hook_kinds.go` `everlongLeverageV1`): stateless, it quotes on the book
/// of the swap hook it is bound to, so it carries nothing in the state. A braced struct rather
/// than a unit one so that `Some(EverlongLeverageV1 {})` serializes as `{}`, which reads back as
/// `Some` (a unit struct would serialize as `null`, which reads back as `None`).
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash,
)]
pub struct EverlongLeverageV1 {}

impl LeverageHook for EverlongLeverageV1 {
    /// `EverlongLeverageHook._quote(ctx)` (`EverlongLeverageHook.sol:88-122`), [`levhook::quote`].
    fn preview_lever(&self, ctx: &LeverContext, book: &LevBook) -> Result<LevFill, FlammError> {
        levhook::quote(ctx, book)
    }
}

/// One pool's Router record with the Morpho market behind each venue (`router.go` `mmRouter`), as
/// the swap and leverage entries drive it. The two views the gate composites make (`positions`,
/// `quarantine`) are the [`GateReads`] supertrait, the gate module's own bound. Methods that
/// settle write through the record and may leave a half-applied state behind an error; the core
/// runs them on a clone and keeps the result only on success, as a reverted transaction leaves no
/// trace.
pub trait Router: Clone + GateReads {
    /// `MMRouter.fundingCeiling(pool, idx, collateralIn, priceWad)` at `now` (`router.go`
    /// `fundingCeiling`): what a max-size plan would raise, `type(uint256).max - remaining`.
    fn funding_ceiling(
        &self,
        idx: u8,
        collateral_in: U256,
        price_wad: U256,
        now: u64,
    ) -> Result<U256, FlammError>;
    /// `router.go` `mmSettleSellLegs`, the body of `FLAMMSwapLib.settleSell`
    /// (`FLAMMSwapLib.sol:290-295`) writing through: `used` poolAsset in, `net` of loan asset
    /// `idx` paid out at the leg's checked cross `price_wad`, then `assertGate`.
    fn settle_sell_legs(
        &mut self,
        pool: &mut gate::Pool,
        idx: u8,
        price_wad: U256,
        used: U256,
        net: U256,
        now: u64,
    ) -> Result<(), FlammError>;
    /// `router.go` `mmSettleBuyLegs`, the buy branch of `FLAMMSwapLib.execute`
    /// (`FLAMMSwapLib.sol:103-105`) writing through: `anchor`, `settleBuy` (`takeLoan`; reclaim
    /// the payout beyond physical, strictly, at the pool's price vector; pay `net` poolAsset;
    /// `releaseExcess`), then the entry gate against the anchor.
    fn settle_buy_legs(
        &mut self,
        pool: &mut gate::Pool,
        idx: u8,
        used: U256,
        net: U256,
        now: u64,
    ) -> Result<(), FlammError>;
    /// `router.go` `endTransaction`: clears the transient repay snapshots, as EIP-1153 storage is
    /// at the end of a transaction.
    fn end_transaction(&mut self);
}

impl Router for MmRouter {
    fn funding_ceiling(
        &self,
        idx: u8,
        collateral_in: U256,
        price_wad: U256,
        now: u64,
    ) -> Result<U256, FlammError> {
        MmRouter::funding_ceiling(self, idx, collateral_in, price_wad, now)
    }

    fn settle_sell_legs(
        &mut self,
        pool: &mut gate::Pool,
        idx: u8,
        price_wad: U256,
        used: U256,
        net: U256,
        now: u64,
    ) -> Result<(), FlammError> {
        router::settle_sell_legs(pool, self, idx, price_wad, used, net, now)
    }

    fn settle_buy_legs(
        &mut self,
        pool: &mut gate::Pool,
        idx: u8,
        used: U256,
        net: U256,
        now: u64,
    ) -> Result<(), FlammError> {
        router::settle_buy_legs(pool, self, idx, used, net, now)
    }

    fn end_transaction(&mut self) {
        MmRouter::end_transaction(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_leverage_hook_port_is_the_hooks_quote() {
        // A frame with no volatile leg is `FrameUnquotable()` (EverlongLeverageHook.sol:90).
        let ctx = LeverContext::default();
        let book = LevBook::default();
        assert_eq!(
            EverlongLeverageV1 {}.preview_lever(&ctx, &book),
            Err(FlammError::FrameUnquotable)
        );
        assert_eq!(levhook::quote(&ctx, &book), Err(FlammError::FrameUnquotable));
    }

    #[test]
    fn the_swap_hook_port_reads_the_stored_reservation_price() {
        let h = HookState { reservation_price_wad: U256::from(7), ..Default::default() };
        assert_eq!(SwapHook::reservation_price(&h), U256::from(7));
        // An empty accounted book re-seeds on a zero support, which the curve refuses
        // (`AlmCurve.sol` `CurveDomain()`).
        assert_eq!(
            SwapHook::fill(&h, &SwapContext::default(), U256::ZERO).err(),
            Some(FlammError::CurveDomain)
        );
    }
}
