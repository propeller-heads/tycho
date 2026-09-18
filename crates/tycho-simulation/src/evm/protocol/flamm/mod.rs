// Copyright (c) 2026 Everlong Labs Limited

//! Everlong FLAMM (Base 8453, c104 @ `80abd43`): a native Tycho integration of the pool's swap
//! venue and lever-up venue, quoted to the wei from tracked state without a VM.
//!
//! # What is integrated
//!
//! One pool (`0xc0fdCB1799cCc2CEBaA1fe247157b0dF33D57572`, cbBTC / USDC) as two components of the
//! protocol system `flamm` (type `flamm_pool`): the swap venue (id = the pool address, `FLAMM.swap`
//! in both directions) and the lever-up venue (id = `pool || 0x00000000 || uint64(1)`,
//! `FLAMM.leverUp`, pool asset in only). Lever-down is not quoted: it pulls only its net pay leg,
//! which the router would strand. Execution is the `FLAMMExecutor` of the execution PR.
//!
//! # State sourcing
//!
//! The `base-flamm` substreams (`protocols/substreams/base-flamm`) streams every storage word a
//! quote reads as an attribute: the FLAMM-owned contracts' words raw (`pool:`, `hook:`, `spread:`,
//! `router:`, `account:`, `pricefeed:`, `factory:`, [`words`]), Morpho Blue's market and position
//! words and the `AdaptiveCurveIrm` rate raw (`mm:`, `irm:`), and the four Chainlink feeds decoded
//! (`feed:`, [`feeds`]), the last three seeded at the package's start block. The decoder
//! ([`decoder`]) unpacks the raw words by the storage layouts of the deployed code, pinned by the
//! codehashes the component's static attributes carry, checks the pool's wiring against its static
//! identity and builds the composed state ([`state`]). Nothing is read from a node.
//!
//! # Exactness
//!
//! A quote is the pool's whole transaction at the execution clock: the priced context, the
//! plan, the hook fill, the validation and the settlement through the Router and Morpho, with
//! Morpho's accrual, the IRM's adaptation, the feeds' staleness, the sequencer grace, the spread's
//! age and the Morpho oracle's SVR reveal evaluated at the block the quote executes in
//! ([`ProtocolSim::apply_block`][apply_block]). Every module is a port of the Go simulator of the
//! same pool, each function tied to its Solidity line, and the parity tests under `tests/` replay
//! the Solidity-generated fixtures of that port row by row with no tolerance; the snapshot tests
//! replay the deployed pool's own `previewSwap` grids at three pinned blocks, and the stream
//! test (`tests/e2e.rs`) replays the `base-flamm` substreams' own output over the pool's
//! history on Base (its creation, activation, every settled swap, some of its deposits and
//! withdrawals, a keeper recenter and the leverage unpause as their own blocks, the rest as
//! catch-up diffs), block by block as the stream decoder would, against the chain's answers at
//! every pinned block and every settled swap.
//!
//! # Limits
//!
//! Only sizes the pool fills in full are quoted: `get_limits` locates the largest per direction,
//! and inside it a buy's dust, which the pool refuses (a few thousand loan-asset units, below a
//! hundredth of a percent of the limit at every recorded block; the sizes the protocol test
//! harness derives from the limit fill), is the empty trade, so every size in `[0, limit]`
//! answers and `query_pool_swap` (the generic search) runs on both venues. `spot_price(base,
//! quote)` is the trait's price, the `quote` that buys one `base` gross of that direction's
//! fee; the lever-up venue, which only sells the pool asset, has one rate and answers both
//! orderings from it (`spot_price(pool asset, loan asset)` is the price of the direction it
//! trades, as `query_pool_swap` and the harness read it). `fee` is the fee or spread a fill
//! pays right now, bounded as the pool bounds it. `apply_block` reports a change when a quote
//! can observe the clock's move: a deadline crossed (a feed's heartbeat, the sequencer grace,
//! the spread's age, the Morpho oracle's reveal, a scheduled change, the rate ceiling's
//! verdict) or a pool positioned in its venue, whose debt and supply accrue every second; an
//! unpositioned pool is quiet between deadlines. A state whose attributes are incomplete (a
//! word the pinned code writes at construction is required, never read as zero), whose code or
//! wiring the port does not model, whose IRM or oracle cannot be read, whose venue is
//! quarantined or whose scheduled implementation / hook-set change is executable refuses to
//! quote rather than guess; at snapshot time [`flamm_filter`] skips such a component instead
//! of failing the stream. Balances (TVL) are not a quote input.
//!
//! | module | contract |
//! |---|---|
//! | [`almcurve`] | `AlmCurve.sol`, the normalized reservation curve the swap hook trades on |
//! | [`fee`] | `EverlongStrategy.sol`, the fill-fee law |
//! | [`hook`] | `EverlongHook.sol`, the swap hook's lazy book rescale, fill and commit |
//! | [`morpho`] | Morpho Blue v1.0.0 share math and market transitions |
//! | [`irm`] | `AdaptiveCurveIrm` v1.0.0 |
//! | [`account`] | `MorphoBlueAccount.sol`, the venue account's views and Router-driven mutators |
//! | [`router`] | `MMRouterLib.sol` / `MMRouter.sol` and the `FLAMMSwapLib` settlement legs |
//! | [`gate`] | `FLAMMGateLib.sol`, the credit gate, room, frame, NAV and entry / exit gates |
//! | [`levcurve`] | `CollRebalancerMath.sol`, the frozen leverage curve |
//! | [`levhook`] | `EverlongLeverageHook.sol`, the leverage venue's frame and fill |
//! | [`pricefeed`] | `PriceFeed.sol` over the Chainlink rounds and the sequencer feed |
//! | [`state`] | `FLAMMStore.sol`: the composed pool state, `FLAMMGateLib.priced` |
//! | [`swap`] | `FLAMMSwapLib.sol`, `previewSwap` / `swap` |
//! | [`lever`] | `FLAMMLeverLib.sol`, `previewLever` / `leverUp` / `leverDown` |
//! | [`context`] | `IFLAMMHooks.sol` / `IFLAMMLeverage.sol`, the frames core hands its hooks |
//! | [`deps`] | the seam the pool core calls its hooks and Router through |
//! | [`error`] | one variant per deployed custom error, with the selector table |
//! | [`math`] | `Math.mulDiv` / `ceilDiv` / `sqrt`, `Mul512` and checked `int256` arithmetic |
//! | [`words`] | the attribute words: slot keys and packed fields of the storage layouts |
//! | [`feeds`] | the Chainlink feeds, the read guard and the `DualAggregator` reveal |
//! | [`decoder`] | static identity, pins, the composed state from the attributes |
//! | [`sim`] | the `ProtocolSim` |
//!
//! Every entry of the pool core takes the block timestamp it runs at: the feed checks, the
//! spread's age and, through the Router, the Morpho accrual and the IRM adaptation are evaluated
//! there. Every refusal is the revert the chain would raise ([`FlammError`]), and every arithmetic
//! step keeps Solidity's operation order and rounding.
//!
//! [apply_block]: tycho_common::simulation::protocol_sim::ProtocolSim::apply_block

pub mod account;
pub mod almcurve;
pub mod context;
pub mod decoder;
pub mod deps;
pub mod error;
pub mod fee;
pub mod feeds;
pub mod gate;
pub mod hook;
pub mod irm;
pub mod levcurve;
pub mod lever;
pub mod levhook;
pub mod math;
pub mod morpho;
pub mod pricefeed;
pub mod router;
pub mod sim;
pub mod state;
pub mod swap;
pub mod words;

#[cfg(test)]
mod tests;

pub use context::{LeverContext, PoolContext, SwapContext};
pub use decoder::flamm_filter;
pub use error::FlammError;
pub use sim::{FlammPoolState, VenueKind};
pub use state::FlammState;
use tycho_client::feed::BlockHeader;

use crate::evm::decoder::TychoStreamDecoder;

/// The protocol system the `base-flamm` substreams indexes under.
pub const PROTOCOL_SYSTEM: &str = "flamm";

/// Registers the FLAMM decoder with a stream decoder, with [`flamm_filter`] so that a component
/// whose snapshot the decoder refuses is skipped rather than fatal to the stream.
pub fn register_flamm_decoder(decoder: &mut TychoStreamDecoder<BlockHeader>) {
    decoder.register_decoder::<FlammPoolState>(PROTOCOL_SYSTEM);
    decoder.register_filter(PROTOCOL_SYSTEM, flamm_filter);
}

/// The composed state of a deployed c104 pool: the swap hook's storage, the stateless leverage
/// hook and the tracked Router record.
pub type Flamm = FlammState<hook::HookState, deps::EverlongLeverageV1, router::Router>;
