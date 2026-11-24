use std::{any::Any, collections::HashMap, num::NonZeroU64, time::Instant};

use alloy::primitives::{address, Address, U256};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams},
    },
    Bytes,
};

use crate::{
    book::{
        component::PairState,
        sim::{self, SwapDirection},
    },
    evm::protocol::u256_num::{biguint_to_u256, u256_to_biguint},
    pamm::protocols::efficient_prop::ladder::{FillPath, Ladder, Refusal},
};

/// Gas of the router's delegatecall of `PropAMMExecutor.swap` into a listing door, selling base,
/// on a listing reading only its operator's word (fence kind WORD or NONE) of its pool's hook pair
/// (the pool's first listing, its base the pool's token 0 and its quote token 1): the door's
/// `swap`, the front's named fill behind it with its pull of the input from the door, and the
/// pool's payout to the receiver. The router's own transfer of the input to the door, and any
/// transfer of the output after the swap, are charged by tycho-execution's gas estimator.
///
/// Every figure here was measured the same way, with real transactions on a Base fork holding an
/// earlier test deployment that is no longer live, through Base's `PropAMMExecutor`:
/// - on the front's counted clock, as live Base runs it: blocks 200 ms apart on the count's grid,
///   so a fill never calls the BaseTime predeploy;
/// - the first fill of the side in its block;
/// - the receiver already holding the output token;
/// - the call's gas before refunds: the refund for the storage the fill clears (19,900) comes back
///   at the transaction's end and is not taken off;
/// - with the tokens' own transfer cost as measured on WETH, USDC and cbBTC, Base's deployed code.
///
/// Each constant is the least first fill measured: 118,157 selling WETH for USDC here and 121,988
/// buying it. A second fill of the side in the same block costs about 60 less. Not charged: about
/// 660 for each depth rung a fill walks past its first, and about 17,100 when the receiver holds
/// none of the output token. A pair of tokens cheaper or dearer to move than these spends that much
/// less or more.
pub(super) const SELL_BASE_SWAP_GAS: u64 = 118_157;

/// Gas of the same call buying base: see [`SELL_BASE_SWAP_GAS`].
pub(super) const BUY_BASE_SWAP_GAS: u64 = 121_988;

/// Gas of the same call selling base into a listing reading only its word whose pair is past its
/// pool's hook pair: any listing but the pool's first, which the pool settles by another route
/// than the hook pair's.
///
/// Measured as [`SELL_BASE_SWAP_GAS`], on a cbBTC/USDC listing of a pool holding WETH, USDC and
/// cbBTC as its tokens 0, 1 and 2, cbBTC the base: 151,734 selling cbBTC for USDC and 150,865
/// buying it.
pub(super) const SELL_BASE_PAST_HOOK_PAIR_SWAP_GAS: u64 = 151_734;

/// Gas of the same call buying base: see [`SELL_BASE_PAST_HOOK_PAIR_SWAP_GAS`].
pub(super) const BUY_BASE_PAST_HOOK_PAIR_SWAP_GAS: u64 = 150_865;

/// Gas of the same call selling base into a listing of its pool's hook pair whose fill reads a
/// price source: a PRINT or SESSION listing, which reads its print in the fill, and a listing of
/// an asset-module pair.
///
/// Measured as [`SELL_BASE_SWAP_GAS`], on a PRINT WETH/USDC listing of its pool's hook pair bound
/// to the protocol's own stored price lane through a source that is not guarded, its print posted
/// in the block before the fill: 130,482 selling WETH and 134,794 buying it. The fill reads the
/// lane's print and its time on top of a WORD listing's fill: 12,325 selling base and 12,806
/// buying it more than [`SELL_BASE_SWAP_GAS`] and [`BUY_BASE_SWAP_GAS`]. A source other than that
/// lane spends what its own code spends. A listing of an asset-module pair, whose fill asks its
/// module for a verdict as well, was not measured: it is charged these figures, or the sum in
/// [`swap_gas`] past the hook pair.
pub(super) const SELL_BASE_PRINT_SWAP_GAS: u64 = 130_482;

/// Gas of the same call buying base: see [`SELL_BASE_PRINT_SWAP_GAS`].
pub(super) const BUY_BASE_PRINT_SWAP_GAS: u64 = 134_794;

/// Gas of the same call selling base into a listing of its pool's hook pair reading a print from a
/// guarded source, which asks its lane for the fill's side's worst price over the prints the lane
/// still holds.
///
/// Measured as [`SELL_BASE_SWAP_GAS`], on a PRINT WETH/USDC listing of its pool's hook pair bound
/// to a guarded source on Base's hold lane, with no print held at the fill: 136,461 selling WETH
/// and 140,520 buying it, 18,304 and 18,532 more than [`SELL_BASE_SWAP_GAS`] and
/// [`BUY_BASE_SWAP_GAS`]. A fill while the lane holds a replaced print reads its words too, about
/// 4,800 to 5,500 more; that is not charged. Past the hook pair, see [`swap_gas`].
pub(super) const SELL_BASE_GUARDED_SWAP_GAS: u64 = 136_461;

/// Gas of the same call buying base: see [`SELL_BASE_GUARDED_SWAP_GAS`].
pub(super) const BUY_BASE_GUARDED_SWAP_GAS: u64 = 140_520;

/// What a listing's fill reads besides its operator's word, which the gas charged follows (see
/// [`swap_gas`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FillReads {
    /// Only the word: a WORD or NONE listing of a pair with no asset module.
    Word,
    /// A price source that is not guarded, or an asset-module pair's module.
    Source,
    /// A price print from a guarded source.
    GuardedSource,
}

/// The chain whose fills a state's gas figures come from: a fill on Base reads its word from the
/// maker's stored push, one on Ethereum consumes the word its maker's update landed in the block,
/// so each chain has its own measurements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum GasChain {
    Base,
    Ethereum,
}

/// Where a listing's pair sits in its pool, which the gas charged follows (see [`swap_gas`]): the
/// pool settles its hook pair by one route and any other pair by another, and on Ethereum a pair
/// without the pool's asset costs other figures than one with it, and one with it costs by whether
/// the input is the asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PairPlace {
    /// The pool's hook pair: its first listing, its base the pool's token 0 and its quote token 1.
    HookPair,
    /// Past the hook pair, the pool's asset (its token 0) the pair's base.
    PastWithAsset,
    /// Past the hook pair, neither of the pair's tokens the pool's asset.
    PastWithoutAsset,
    /// Past the hook pair, the pool's asset the pair's quote.
    PastWithAssetAsQuote,
}

impl PairPlace {
    /// The place of a pair whose base is the pool's token `base_index` and whose quote its token
    /// `quote_index`, as `doorView` gives them.
    pub(super) fn of(base_index: u8, quote_index: u8) -> Self {
        match (base_index, quote_index) {
            (0, 1) => Self::HookPair,
            (0, _) => Self::PastWithAsset,
            (_, 0) => Self::PastWithAssetAsQuote,
            _ => Self::PastWithoutAsset,
        }
    }
}

/// Gas on Ethereum of the router's delegatecall of `PropAMMExecutor.swap` into a listing door,
/// selling base, on a listing of its pool's hook pair whose fill reads a price source that is not
/// guarded (a PRINT listing bound to its pair's own price contract): the door's `swap`, the front's
/// fill behind it, which consumes the quote its maker's update landed earlier in the same block and
/// reads the pair's print, and the pool's payout to the receiver. As on Base, the router's own
/// transfer of the input to the door is charged by tycho-execution's gas estimator.
///
/// Every Ethereum figure here was measured the same way, with real transactions on a fork of
/// Ethereum holding the deployed build (front `0x0000…AcD6`), through Ethereum's
/// `PropAMMExecutor` (`0xc196…E4fa`):
/// - each fill sent right behind its block's relay entries, the only fill of its side in the block;
/// - the receiver already holding the output token;
/// - the call's gas less the refund the fill earns as the only fill of its transaction, which is
///   what the taker pays: the router's transfer writes the door's balance of the input from zero
///   and the fill clears it, 19,900, the refund counter of every custody fill traced (wstETH, WETH,
///   USDC and USDT paid in). A fifth of the transaction's gas caps a refund; on the smallest
///   single-fill transaction traced that cap was 38,823, so it does not bind;
/// - from a pool whose custody holds the output, so the fill calls no lender.
///
/// Each constant is the least such fill seen across every run and size, through the executor on
/// the fork and in tycho-execution's own fork test, less that refund, so that no figure charges
/// a first fill in a fresh block more than its transaction pays for it. The call itself uses the
/// figure plus the refund before the transaction ends: a gas limit is sized from that sum.
///
/// The hook pair's two were measured on 24 Nov on a pool of wstETH and WETH alone, its
/// wstETH/WETH listing in no credit mode: 105,899 selling wstETH and 106,520 buying it. The
/// figure follows the listing's credit mode, not its pool's lenders: the same pool with a lender
/// bound and unused cost within 16 gas of them, while the same pair on a pool whose listing is in
/// credit mode 2 cost 118,009 and 113,527 from custody (the front's read of the pool's room, and
/// the input passing through the door), which is why a listing in a credit mode is charged its
/// configured figures ([`CreditGas`]) and never these. A hook pair of other tokens spends what
/// their own transfers cost and is not charged these ([`ETHEREUM_MEASURED`]).
///
/// Same-size first fills vary by up to about 500 gas from run to run, and other sizes cost up to
/// about 1,100 more, so a fill may spend up to about that much more than it is charged. A second
/// fill of the same side in one block costs about 500 less. A fill of the word the maker's update
/// landed in the block before, where the listing and the word carry, costs about 2,400 more than
/// a first fill in the update's own block; that is not charged.
pub(super) const ETHEREUM_SELL_BASE_PRINT_SWAP_GAS: u64 = 105_899;

/// Gas of the same call buying base: see [`ETHEREUM_SELL_BASE_PRINT_SWAP_GAS`].
pub(super) const ETHEREUM_BUY_BASE_PRINT_SWAP_GAS: u64 = 106_520;

/// Ethereum's wstETH, the base of three of the pairs measured there.
const ETHEREUM_WSTETH: Address = address!("7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0");

/// Ethereum's WETH.
const ETHEREUM_WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

/// Ethereum's USDT.
const ETHEREUM_USDT: Address = address!("dAC17F958D2ee523a2206206994597C13D831ec7");

/// Ethereum's USDC.
const ETHEREUM_USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

/// A pair measured on Ethereum at a place of its pool: `(selling base, buying base)`.
struct MeasuredPair {
    place: PairPlace,
    base: Address,
    quote: Address,
    gas: (u64, u64),
}

/// Every pair measured on the deployed build on Ethereum, each a listing in no credit mode reading
/// a price source that is not guarded, measured as [`ETHEREUM_SELL_BASE_PRINT_SWAP_GAS`]:
/// - wstETH/WETH, its pool's hook pair, on a pool of those two tokens;
/// - on a pool of wstETH, WETH, USDC and USDT, past the hook pair: wstETH/USDC, 126,434 selling
///   wstETH and 125,271 buying it; wstETH/USDT, 131,620 and 126,449; and USDT/USDC, neither the
///   pool's asset, 134,412 selling USDT and 138,945 buying it.
///
/// A fill spends what its own tokens' transfers and its own pool cost: wstETH/USDT costs about
/// 5,200 more than wstETH/USDC selling wstETH (USDT's transfer out of the pool against USDC's).
/// So a figure stands for its own pair, with the same base, at its own place, and for nothing
/// else. A listing of any other pair, of a measured pair at another place, or reading only its
/// word (no listing of the deployed build does, so that class was never measured on it) has no
/// figure here: it is served only where the caller names its gas
/// ([`EfficientPropFeedBuilder::credit_listing`]), and left out otherwise rather than charged
/// another pair's figure, which may be over what it spends.
///
/// [`EfficientPropFeedBuilder::credit_listing`]:
/// crate::pamm::protocols::efficient_prop::EfficientPropFeedBuilder::credit_listing
const ETHEREUM_MEASURED: [MeasuredPair; 4] = [
    MeasuredPair {
        place: PairPlace::HookPair,
        base: ETHEREUM_WSTETH,
        quote: ETHEREUM_WETH,
        gas: (ETHEREUM_SELL_BASE_PRINT_SWAP_GAS, ETHEREUM_BUY_BASE_PRINT_SWAP_GAS),
    },
    MeasuredPair {
        place: PairPlace::PastWithAsset,
        base: ETHEREUM_WSTETH,
        quote: ETHEREUM_USDC,
        gas: (126_434, 125_271),
    },
    MeasuredPair {
        place: PairPlace::PastWithAsset,
        base: ETHEREUM_WSTETH,
        quote: ETHEREUM_USDT,
        gas: (131_620, 126_449),
    },
    MeasuredPair {
        place: PairPlace::PastWithoutAsset,
        base: ETHEREUM_USDT,
        quote: ETHEREUM_USDC,
        gas: (134_412, 138_945),
    },
];

/// One class's gas on Base, each `(selling base, buying base)`: on the hook pair and past it.
struct PlaceGas {
    hook: (u64, u64),
    past: (u64, u64),
}

/// `base + (to - from)`, each direction: a figure moved by another's difference between two
/// classes. A sum, not a measurement.
const fn moved(base: (u64, u64), from: (u64, u64), to: (u64, u64)) -> (u64, u64) {
    (base.0 + to.0 - from.0, base.1 + to.1 - from.1)
}

const BASE_WORD: (u64, u64) = (SELL_BASE_SWAP_GAS, BUY_BASE_SWAP_GAS);
const BASE_WORD_PAST: (u64, u64) =
    (SELL_BASE_PAST_HOOK_PAIR_SWAP_GAS, BUY_BASE_PAST_HOOK_PAIR_SWAP_GAS);
const BASE_PRINT: (u64, u64) = (SELL_BASE_PRINT_SWAP_GAS, BUY_BASE_PRINT_SWAP_GAS);
const BASE_GUARDED: (u64, u64) = (SELL_BASE_GUARDED_SWAP_GAS, BUY_BASE_GUARDED_SWAP_GAS);

/// The figures a listing whose fill reads `reads`, whose pair of `base` and `quote` sits at `pair`
/// in its pool, is charged on `chain` where no gas is named for it: `(selling base, buying base)`.
///
/// On Ethereum, its own pair's measured figures ([`ETHEREUM_MEASURED`]), and none for any other
/// listing. On Base, whose path is not served, its class's: the word, print and guarded figures of
/// the hook pair and the word figure past it were measured, and past the hook pair a listing
/// reading a source is charged the past figure and its source's difference on the hook pair
/// (164,059 and 163,671 for a source that is not guarded, 170,038 and 169,397 for a guarded one),
/// whichever tokens the pair holds.
pub(super) fn swap_gas(
    chain: GasChain,
    reads: FillReads,
    pair: PairPlace,
    base: &Bytes,
    quote: &Bytes,
) -> Option<(u64, u64)> {
    match chain {
        GasChain::Base => {
            let class = match reads {
                FillReads::Word => PlaceGas { hook: BASE_WORD, past: BASE_WORD_PAST },
                FillReads::Source => PlaceGas {
                    hook: BASE_PRINT,
                    past: moved(BASE_WORD_PAST, BASE_WORD, BASE_PRINT),
                },
                FillReads::GuardedSource => PlaceGas {
                    hook: BASE_GUARDED,
                    past: moved(BASE_WORD_PAST, BASE_WORD, BASE_GUARDED),
                },
            };
            Some(if pair == PairPlace::HookPair { class.hook } else { class.past })
        }
        GasChain::Ethereum if reads == FillReads::Source => ETHEREUM_MEASURED
            .iter()
            .find(|measured| {
                measured.place == pair &&
                    base.as_ref() == measured.base.as_slice() &&
                    quote.as_ref() == measured.quote.as_slice()
            })
            .map(|measured| measured.gas),
        GasChain::Ethereum => None,
    }
}

/// One gas figure each direction: selling base for quote, and buying base with quote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectionGas {
    pub sell_base: NonZeroU64,
    pub buy_base: NonZeroU64,
}

impl DirectionGas {
    fn of(&self, direction: SwapDirection) -> u64 {
        match direction {
            SwapDirection::BaseToQuote => self.sell_base.get(),
            SwapDirection::QuoteToBase => self.buy_base.get(),
        }
    }
}

/// What a custody fill spends more where its pool holds none of the token the fill pays into it:
/// the token writes the pool's balance from zero (22,100 gas for a first write of a cold slot,
/// against the 5,000 of a later one), and the balance stays, so no refund comes back. Measured on
/// Ethereum on 24 Nov, the same fill with the pool holding none and some: 17,068 on a custody
/// fill buying wstETH with WETH, 17,100 on one selling wstETH for USDC, and 16,964 the least seen.
/// Every measured figure is of a pool holding some of both tokens; a read that finds it holding
/// none of a custody fill's input adds this, so that the sum is never over what such a fill
/// spends. A routed fill is charged nothing more: where the token it borrows passes through a
/// pool holding none of it, the balance is written from zero and cleared again, and the refund
/// gives all but about 1,900 back (a routed sale of the first pool's wstETH/WETH listing: 17,028
/// more before refunds, 15,100 more refunded); a routed buy whose input stays in such a pool
/// spends about 17,100 to 18,500 more than it is charged.
pub(super) const FIRST_BALANCE_WRITE_GAS: u64 = 16_964;

/// What the largest size a side's custody fills may earn back beyond its class's refund: where
/// that fill takes the pool's whole balance of the output, the balance is cleared, and each slot
/// cleared refunds 4,800. Measured on Ethereum on 24 Nov on the two served samples that emptied
/// their pool: 4,800 (a wstETH/WETH buy taking the pool's last wstETH) and 9,600 (a wstETH/USDC
/// sale taking its last USDC). The state takes the larger off the charge of each side's largest
/// custody size alone, whether or not that fill empties the pool, so that no charge is over what
/// such a fill costs its transaction; a size under it leaves a balance and earns nothing more.
pub(super) const LAST_CUSTODY_FILL_REFUND: u64 = 9_600;

/// What a swap of a component settling through Tycho's fallback router is charged, whichever its
/// listing: the least a transaction pays on either of the router's two branches, less the 40,000
/// tycho-execution's gas estimator adds for every `fallback:` component
/// (`FALLBACK_ROUTER_OVERHEAD_GAS`), so that the two together are never over what the swap costs.
///
/// Measured on 24 Nov with real transactions on a fork of Ethereum holding the deployed build,
/// through the `PropAMMFallbackExecutor` (`0x355d…84c9`) and the router (`0xA4bC…f633`) deployed
/// there, beside the same fill through `PropAMMExecutor`, and stated as that fill's charge plus
/// the difference between the two receipts:
/// - where the door quotes nothing (a block without the maker's update) the router swaps on the
///   solver's pool, and the transaction pays for the door's refused quote (about 40,000) and that
///   pool's swap: with a Uniswap V2 pair, 90,445 and 90,531 on wstETH/WETH and 99,354 and 102,984
///   on USDT/USDC; with a Uniswap V3 pool, 105,181 to 109,541 over the listings measured;
/// - where the door fills, the router first quotes the door and the pool, and the transaction pays
///   48,794 to 74,415 more than the direct fill (154,693 at the least).
///
/// The charge is the least of them, 90,445. Which branch a swap takes is known only in the block
/// it lands in, and the pool is the solver's: a swap that fills at the door costs about 64,000 to
/// 121,000 more than it is charged, one that falls back to a dearer pool what that pool costs. A
/// door refusing sooner than for a missing update (a stopped desk) was not measured.
pub(super) const FALLBACK_SWAP_GAS: u64 = 90_445 - 40_000;

/// The gas a credit listing's fills are charged, measured on that listing's own pool: one figure
/// for a fill its pool's custody fills, and one for a fill through the pool's credit route, each
/// direction. Measure each with real transactions through Ethereum's `PropAMMExecutor`, as the
/// other Ethereum figures were (the call's gas less the refund its transaction earns as a
/// single fill, read from a trace of that transaction): a custody fill of each side, and a
/// routed one of each side, each on a pool holding some of both tokens
/// ([`FIRST_BALANCE_WRITE_GAS`] is added to a custody fill where it holds none of the input).
/// On the first Ethereum pool's wstETH/WETH listing (credit mode 2, Morpho Blue, measured 24 Nov
/// on a fork of the deployed contracts): custody 117,990 selling wstETH and 113,508 buying it
/// (137,890 and 133,408 before a refund of 19,900), route 824,095 and 932,904 (908,514 and
/// 1,017,323 before refunds of 84,400, each less 19 for the difference between the two meters
/// used). Each is the least seen over the states run: no loan and a loan open, the pool holding
/// none of the borrowed token (925,553 to 925,593 before a refund of 99,500) and some of it, a
/// route the pool's custody covers in part, which is the cheapest. A purchase of wstETH past
/// custody repays what the pool owes in WETH: owing, it spends 1,045,623 (1,134,823 before a
/// refund of 89,200) up to the debt; owing nothing, the door refuses it (`NotEnoughDepth` in the
/// fill) but for the first 0.0104 wstETH past custody (951,411), and the feed quotes only the
/// sizes the door answers. Name the least figure
/// measured: a charge is never to be over what a first fill in a fresh block pays, and where a
/// pool's route may draw at more than one lender, the cheaper lender's figure. Measure again
/// when the pool's settings change: a route spends less after some of them. A solver's gas
/// limit is another matter: it is sized from the gas before refunds (about 0.93M selling and
/// 1.04M to 1.13M buying on that listing) and must cover the route on any listing that has one:
/// a size sampled inside custody fills through the route where another fill takes the pool's
/// balance first, at several times the gas. The route's gas is the pool's, not this code's:
/// a pool of two tokens and one of three, a rate ceiling or a pair cap on the venue, each spend
/// their own, so no constant serves every pool. In credit mode 2, a pool past its first borrow
/// routes every bid until a flush proves it debt-free, so its bids cost the route's figure for most
/// of its life. In mode 3 every size the pool's switches let route is routed, those its custody
/// fills whole too: those draw nothing and are charged the route's figure, more than they spend.
/// One route figure a side stands for every path the route takes (a borrow, a repay, the pool's
/// first borrow): a dearer path is under-charged, which the figure's measurement states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreditGas {
    pub custody: DirectionGas,
    pub route: DirectionGas,
}

impl std::str::FromStr for CreditGas {
    type Err = String;

    /// Reads `custody sell:custody buy:route sell:route buy`, four gas figures above zero: a
    /// custody fill selling base and buying it, then a routed fill selling base and buying it.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let figures = text
            .split(':')
            .map(|figure| {
                figure
                    .trim()
                    .parse::<NonZeroU64>()
                    .map_err(|e| format!("{figure} is not a gas figure above zero: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let [custody_sell, custody_buy, route_sell, route_buy] = figures[..] else {
            return Err(format!(
                "{text} is not four gas figures, custody sell:custody buy:route sell:route buy"
            ));
        };
        Ok(Self {
            custody: DirectionGas { sell_base: custody_sell, buy_base: custody_buy },
            route: DirectionGas { sell_base: route_sell, buy_base: route_buy },
        })
    }
}

/// The moment a side stops quoting: on this process's monotonic clock, and the wall clock's Unix
/// milliseconds it was set from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Deadline {
    pub(super) at: Instant,
    pub(super) unix_ms: u64,
}

impl Deadline {
    /// `unix_ms`, on the wall clock that reads `now_ms` now.
    pub(super) fn at_unix_ms(unix_ms: u64, now_ms: u64) -> Self {
        let at = Instant::now() + std::time::Duration::from_millis(unix_ms.saturating_sub(now_ms));
        Self { at, unix_ms }
    }
}

/// One listing door as the door quoted it at one block: its pair, and each side's quotes at the
/// sizes sampled.
///
/// Quotes exactly what the door quoted at a sampled size and the chord between two neighbouring
/// sizes, rounded down; it refuses sizes below the smallest sampled or above the largest, the
/// most the listing filled. A fill is the door's all or nothing, so the state after a swap quotes
/// nothing: the listing's next quote depends on how the fill moved the front, which was not
/// sampled. On a credit listing it quotes nothing between the largest size its pool's custody
/// fills and the smallest its credit route was sampled at, and charges each fill the configured
/// gas of its path ([`CreditGas`]).
#[derive(Clone, derive_more::Debug, PartialEq, Serialize, Deserialize)]
pub struct EfficientPropState {
    pub(super) base_token: Token,
    pub(super) quote_token: Token,
    /// The listing door a swap is sent to.
    pub(super) door: Bytes,
    /// Selling base for quote.
    #[debug("{{ most: {:?} }}", bid.last())]
    pub(super) bid: Ladder,
    /// Selling quote for base.
    #[debug("{{ most: {:?} }}", ask.last())]
    pub(super) ask: Ladder,
    /// What a fill reads besides the word, which the gas charged follows (see [`swap_gas`]).
    pub(super) reads: FillReads,
    /// Where the listing's pair sits in its pool, which the gas charged follows (see
    /// [`swap_gas`]).
    pub(super) pair: PairPlace,
    /// The chain whose measured figures the gas charged follows (see [`swap_gas`]).
    pub(super) gas_chain: GasChain,
    /// On a credit listing, the gas configured for its fills, which every swap of it is charged in
    /// place of [`swap_gas`], by the path its size takes. Absent from the serialized state of any
    /// other listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) credit: Option<CreditGas>,
    /// Whether the listing's pool held none of the base token when the listing was read, and so
    /// a custody fill paying it in is charged [`FIRST_BALANCE_WRITE_GAS`] more. Absent from the
    /// serialized state where it held some.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) pool_lacks_base: bool,
    /// The same for the quote token.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) pool_lacks_quote: bool,
    /// Whether the state's component settles through Tycho's fallback router, and so every swap
    /// is charged [`FALLBACK_SWAP_GAS`]. Absent from the serialized state where it does not.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) through_fallback: bool,
    /// From when the bid refuses every quote: the last moment a route priced from it can be sent
    /// and still land while the price print it read lives, where that is sooner than the book's
    /// life. `None` where the side quotes for as long as the book is served. Never serialized: a
    /// state read back quotes without one.
    #[serde(skip)]
    pub(super) bid_until: Option<Deadline>,
    /// The same for the ask.
    #[serde(skip)]
    pub(super) ask_until: Option<Deadline>,
}

impl EfficientPropState {
    #[expect(clippy::too_many_arguments, reason = "one argument per field the feed reads")]
    pub(super) fn new(
        base_token: Token,
        quote_token: Token,
        door: Bytes,
        bid: Ladder,
        ask: Ladder,
        reads: FillReads,
        pair: PairPlace,
        gas_chain: GasChain,
        credit: Option<CreditGas>,
    ) -> Self {
        Self {
            base_token,
            quote_token,
            door,
            bid,
            ask,
            reads,
            pair,
            gas_chain,
            credit,
            pool_lacks_base: false,
            pool_lacks_quote: false,
            through_fallback: false,
            bid_until: None,
            ask_until: None,
        }
    }

    /// This state, charging a custody fill that pays in a token its pool holds none of
    /// [`FIRST_BALANCE_WRITE_GAS`] more: `base` and `quote` are whether the pool held none of each
    /// when the listing was read.
    pub(super) fn with_pool_lacking(mut self, base: bool, quote: bool) -> Self {
        self.pool_lacks_base = base;
        self.pool_lacks_quote = quote;
        self
    }

    /// This state, charged as a component settling through Tycho's fallback router where
    /// `through_fallback`.
    pub(super) fn with_fallback_router(mut self, through_fallback: bool) -> Self {
        self.through_fallback = through_fallback;
        self
    }

    /// This state, its sides refusing every quote from `bid_until` and `ask_until`.
    pub(super) fn with_deadlines(
        mut self,
        bid_until: Option<Deadline>,
        ask_until: Option<Deadline>,
    ) -> Self {
        self.bid_until = bid_until;
        self.ask_until = ask_until;
        self
    }

    /// When the side a swap in `direction` fills stops quoting, if it does before the book's
    /// life ends.
    pub(super) fn side_until(&self, direction: SwapDirection) -> Option<Deadline> {
        match direction {
            SwapDirection::BaseToQuote => self.bid_until,
            SwapDirection::QuoteToBase => self.ask_until,
        }
    }

    /// Fails once the side a swap in `direction` fills has stopped quoting at `now`.
    fn ensure_quotable(
        &self,
        direction: SwapDirection,
        now: Instant,
    ) -> Result<(), SimulationError> {
        match self.side_until(direction) {
            Some(until) if now >= until.at => Err(SimulationError::RecoverableError(format!(
                "the listing may stop filling before a route priced now lands, its price print \
                 lapsing or its word or depth running out: the side stopped quoting at {} ms",
                until.unix_ms
            ))),
            _ => Ok(()),
        }
    }

    /// The side a swap in `direction` fills.
    pub(super) fn side(&self, direction: SwapDirection) -> &Ladder {
        match direction {
            SwapDirection::BaseToQuote => &self.bid,
            SwapDirection::QuoteToBase => &self.ask,
        }
    }

    /// What the listing pays out across both sides at the read, in quote token atoms: the bid's
    /// largest output, and the ask's largest input, which values the base it pays at its own
    /// price.
    pub(super) fn notional_in_quote(&self) -> BigUint {
        let bid = self
            .bid
            .last()
            .map_or(BigUint::ZERO, |(_, out)| u256_to_biguint(out));
        let ask = self
            .ask
            .last()
            .map_or(BigUint::ZERO, |(input, _)| u256_to_biguint(input));
        bid + ask
    }

    /// The figures a custody fill is charged where no gas is named for the listing
    /// ([`swap_gas`]): none on Ethereum for a pair that was not measured.
    fn measured_gas(&self) -> Option<(u64, u64)> {
        swap_gas(
            self.gas_chain,
            self.reads,
            self.pair,
            &self.base_token.address,
            &self.quote_token.address,
        )
    }

    /// Whether every fill of this state has a figure to be charged: a feed serves no state
    /// without one.
    pub(super) fn has_gas(&self) -> bool {
        self.through_fallback || self.credit.is_some() || self.measured_gas().is_some()
    }

    /// The gas charged for a swap of `amount` in `direction` whose fill takes `path`, less
    /// [`LAST_CUSTODY_FILL_REFUND`] at the side's largest custody size: through the fallback
    /// router [`FALLBACK_SWAP_GAS`], else the listing's configured figure for that path, else its
    /// pair's measured figure ([`swap_gas`]). Fails for a fill with no figure, which no feed
    /// makes: a routed fill of a state with no credit gas, or a pair not measured on Ethereum.
    fn gas(
        &self,
        direction: SwapDirection,
        path: FillPath,
        amount: U256,
    ) -> Result<BigUint, SimulationError> {
        if self.through_fallback {
            return Ok(BigUint::from(FALLBACK_SWAP_GAS));
        }
        let gas =
            match (self.credit, path) {
                (Some(credit), FillPath::Custody) => credit.custody.of(direction),
                (Some(credit), FillPath::Route) => credit.route.of(direction),
                (None, FillPath::Custody) => match (self.measured_gas(), direction) {
                    (Some((sell_gas, _)), SwapDirection::BaseToQuote) => sell_gas,
                    (Some((_, buy_gas)), SwapDirection::QuoteToBase) => buy_gas,
                    (None, _) => return Err(SimulationError::FatalError(
                        "an Efficient PropAMM listing's pair was not measured on its chain, and \
                         its state has no gas named to charge it"
                            .into(),
                    )),
                },
                (None, FillPath::Route) => return Err(SimulationError::FatalError(
                    "an Efficient PropAMM side fills through a credit route, but its state has no \
                     credit gas to charge it"
                        .into(),
                )),
            };
        // A fill inside custody pays its input into the pool, where it stays. A routed one moves
        // its tokens through the pool: a balance written from zero is cleared again and most of
        // the write is refunded, so nothing is added.
        let first_writes = match (path, direction) {
            (FillPath::Custody, SwapDirection::BaseToQuote) => u64::from(self.pool_lacks_base),
            (FillPath::Custody, SwapDirection::QuoteToBase) => u64::from(self.pool_lacks_quote),
            (FillPath::Route, _) => 0,
        };
        // The largest size custody fills may take the pool's whole balance of the output, which
        // clears it and earns the transaction more back (Ethereum's figures are net of refunds).
        let cleared = if self.gas_chain == GasChain::Ethereum &&
            path == FillPath::Custody &&
            self.side(direction)
                .is_custody_most(amount)
        {
            LAST_CUSTODY_FILL_REFUND
        } else {
            0
        };
        Ok(BigUint::from((gas + first_writes * FIRST_BALANCE_WRITE_GAS).saturating_sub(cleared)))
    }

    /// The state after a fill: the same pair, quoting nothing.
    fn spent(&self) -> Self {
        let empty = || Ladder::from_samples(&[], None);
        Self { bid: empty(), ask: empty(), ..self.clone() }
    }
}

impl PairState for EfficientPropState {
    fn base_token(&self) -> &Token {
        &self.base_token
    }

    fn quote_token(&self) -> &Token {
        &self.quote_token
    }

    fn directions(&self) -> Vec<(Bytes, Bytes)> {
        sim::directions(
            &self.base_token,
            &self.quote_token,
            !self.bid.is_empty(),
            !self.ask.is_empty(),
        )
    }
}

#[typetag::serde]
impl ProtocolSim for EfficientPropState {
    /// Zero: the listing's fees are inside the quotes the door returned.
    fn fee(&self) -> f64 {
        0.0
    }

    /// The rate at the smallest size the side was quoted at, `quote` per `base` in human units.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let direction = SwapDirection::require(
            &self.base_token,
            &self.quote_token,
            &base.address,
            &quote.address,
        )?;
        self.ensure_quotable(direction, Instant::now())?;
        let (input, output) = self
            .side(direction)
            .first()
            .ok_or(Refusal::Empty)?;
        let input = sim::to_human(&u256_to_biguint(input), base.decimals);
        let output = sim::to_human(&u256_to_biguint(output), quote.decimals);
        Ok(output / input)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        sim::require_positive_amount(&amount_in)?;
        let direction = SwapDirection::require(
            &self.base_token,
            &self.quote_token,
            &token_in.address,
            &token_out.address,
        )?;
        self.ensure_quotable(direction, Instant::now())?;
        if amount_in.bits() > 256 {
            return Err(SimulationError::InvalidInput("amount exceeds uint256".into(), None));
        }
        let asked = biguint_to_u256(&amount_in);
        match self.side(direction).quote(asked) {
            Ok((output, path)) => Ok(GetAmountOutResult::new(
                u256_to_biguint(output),
                self.gas(direction, path, asked)?,
                Box::new(self.spent()),
            )),
            // The most the listing fills, attached as the partial result a ladder hands back, at
            // the gas of that fill's path.
            Err(Refusal::AboveLargest { amount_in: most, amount_out, path }) => {
                Err(SimulationError::InvalidInput(
                    format!(
                        "Pool has not enough liquidity to support complete swap. Input amount: \
                         {amount_in}, consumed amount: {most}"
                    ),
                    Some(GetAmountOutResult::new(
                        u256_to_biguint(amount_out),
                        self.gas(direction, path, most)?,
                        Box::new(self.spent()),
                    )),
                ))
            }
            // Between a credit side's custody and its route nothing is quoted: the most of the
            // amount it fills is its largest custody size, attached as the partial result at the
            // custody's gas. The side's limit is its largest routed size, so amounts under it fail
            // here.
            Err(Refusal::AcrossStep { custody_most, custody_out, route_least }) => {
                Err(SimulationError::InvalidInput(
                    format!(
                        "Pool fills nothing between its custody and its credit route, from \
                         {custody_most} to {route_least}. Input amount: {amount_in}, consumed \
                         amount: {custody_most}"
                    ),
                    Some(GetAmountOutResult::new(
                        u256_to_biguint(custody_out),
                        self.gas(direction, FillPath::Custody, custody_most)?,
                        Box::new(self.spent()),
                    )),
                ))
            }
            Err(refusal) => Err(refusal.into()),
        }
    }

    /// The most the side fills at the read, and its output. Zero both ways where it fills
    /// nothing.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let direction =
            SwapDirection::require(&self.base_token, &self.quote_token, &sell_token, &buy_token)?;
        self.ensure_quotable(direction, Instant::now())?;
        Ok(self
            .side(direction)
            .last()
            .map_or((BigUint::ZERO, BigUint::ZERO), |(input, output)| {
                (u256_to_biguint(input), u256_to_biguint(output))
            }))
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        Err(TransitionError::DecodeError(
            "Efficient PropAMM state is read whole every poll and does not take deltas".into(),
        ))
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use rstest::rstest;

    use super::*;
    use crate::pamm::protocols::efficient_prop::test_support::{
        credit_gas, token, usdc, weth, weth_usdc_credit_state, weth_usdc_state,
    };

    fn amount(value: u64) -> BigUint {
        BigUint::from(value)
    }

    /// The WETH/USDC fixture sells 0.5 WETH for 1,499 USDC and 1 WETH for 2,990 USDC.
    #[rstest]
    #[case::a_sampled_size(500_000_000_000_000_000, 1_499_000_000)]
    #[case::the_largest_size(1_000_000_000_000_000_000, 2_990_000_000)]
    // 1,499 USDC + 0.25 / 0.5 of the 1,491 USDC more the chord adds.
    #[case::along_the_chord(750_000_000_000_000_000, 2_244_500_000)]
    fn selling_base_quotes_the_samples_and_their_chords(
        #[case] amount_in: u64,
        #[case] expected: u64,
    ) {
        let state = weth_usdc_state();

        let result = state
            .get_amount_out(amount(amount_in), &weth(), &usdc())
            .unwrap();

        assert_eq!(result.amount, amount(expected));
        assert_eq!(result.gas, amount(SELL_BASE_SWAP_GAS));
    }

    #[test]
    fn buying_base_quotes_the_ask() {
        let state = weth_usdc_state();

        let result = state
            .get_amount_out(amount(3_010_000_000), &usdc(), &weth())
            .unwrap();

        assert_eq!(result.amount, amount(1_000_000_000_000_000_000));
        assert_eq!(result.gas, amount(BUY_BASE_SWAP_GAS));
    }

    /// A listing whose fill reads a price source is charged the figures measured on one reading a
    /// print, on both sides and on the most a side fills.
    #[test]
    fn a_state_reading_a_source_charges_the_print_gas() {
        let mut state = weth_usdc_state();
        state.reads = FillReads::Source;

        let sell = state
            .get_amount_out(amount(500_000_000_000_000_000), &weth(), &usdc())
            .unwrap();
        let buy = state
            .get_amount_out(amount(3_010_000_000), &usdc(), &weth())
            .unwrap();
        let most = state
            .get_amount_out(amount(2_000_000_000_000_000_000), &weth(), &usdc())
            .unwrap_err();

        assert_eq!(sell.gas, amount(SELL_BASE_PRINT_SWAP_GAS));
        assert_eq!(buy.gas, amount(BUY_BASE_PRINT_SWAP_GAS));
        let SimulationError::InvalidInput(_, Some(partial)) = most else {
            panic!("expected the most the listing fills, got {most:?}");
        };
        assert_eq!(partial.gas, amount(SELL_BASE_PRINT_SWAP_GAS));
    }

    /// The gas charged by class, each direction: a listing reading only its word on its pool's
    /// hook pair, the same past the hook pair, a listing reading a source on the hook pair, and
    /// one past it, charged the past figure and the print's difference on the hook pair, a sum;
    /// a listing reading a guarded source on the hook pair, and one past it, a sum too.
    #[rstest]
    #[case::word_on_the_hook_pair(FillReads::Word, PairPlace::HookPair, 118_157, 121_988)]
    #[case::word_past_the_hook_pair(FillReads::Word, PairPlace::PastWithAsset, 151_734, 150_865)]
    #[case::word_past_it_without_the_asset(
        FillReads::Word,
        PairPlace::PastWithoutAsset,
        151_734,
        150_865
    )]
    #[case::print_on_the_hook_pair(FillReads::Source, PairPlace::HookPair, 130_482, 134_794)]
    #[case::print_past_the_hook_pair(FillReads::Source, PairPlace::PastWithAsset, 164_059, 163_671)]
    #[case::print_past_it_without_the_asset(
        FillReads::Source,
        PairPlace::PastWithoutAsset,
        164_059,
        163_671
    )]
    #[case::guarded_on_the_hook_pair(
        FillReads::GuardedSource,
        PairPlace::HookPair,
        136_461,
        140_520
    )]
    #[case::guarded_past_the_hook_pair(
        FillReads::GuardedSource,
        PairPlace::PastWithAsset,
        170_038,
        169_397
    )]
    #[case::print_past_the_hook_pair_with_the_asset_as_quote(
        FillReads::Source,
        PairPlace::PastWithAssetAsQuote,
        164_059,
        163_671
    )]
    fn the_gas_charged_is_its_class(
        #[case] reads: FillReads,
        #[case] pair: PairPlace,
        #[case] sell: u64,
        #[case] buy: u64,
    ) {
        let mut state = weth_usdc_state();
        state.reads = reads;
        state.pair = pair;

        let selling = state
            .get_amount_out(amount(500_000_000_000_000_000), &weth(), &usdc())
            .unwrap();
        let buying = state
            .get_amount_out(amount(3_010_000_000), &usdc(), &weth())
            .unwrap();

        assert_eq!((selling.gas, buying.gas), (amount(sell), amount(buy)));
    }

    /// On Ethereum a listing is charged the figures measured on its own pair, with the same base,
    /// at its own place, reading a source that is not guarded; any other has none, is refused by
    /// the state and is left out by the feed unless its gas is named. Through the fallback router
    /// every listing is charged the one figure of that path.
    #[rstest]
    #[case::wsteth_weth_on_the_hook_pair(
        FillReads::Source,
        PairPlace::HookPair,
        ETHEREUM_WSTETH,
        ETHEREUM_WETH,
        Some((105_899, 106_520))
    )]
    #[case::wsteth_usdc_past_it(
        FillReads::Source,
        PairPlace::PastWithAsset,
        ETHEREUM_WSTETH,
        ETHEREUM_USDC,
        Some((126_434, 125_271))
    )]
    #[case::wsteth_usdt_past_it(
        FillReads::Source,
        PairPlace::PastWithAsset,
        ETHEREUM_WSTETH,
        ETHEREUM_USDT,
        Some((131_620, 126_449))
    )]
    #[case::usdt_usdc_without_the_asset(
        FillReads::Source,
        PairPlace::PastWithoutAsset,
        ETHEREUM_USDT,
        ETHEREUM_USDC,
        Some((134_412, 138_945))
    )]
    #[case::the_other_way_round(
        FillReads::Source,
        PairPlace::PastWithoutAsset,
        ETHEREUM_USDC,
        ETHEREUM_USDT,
        None
    )]
    #[case::a_measured_pair_at_another_place(
        FillReads::Source,
        PairPlace::HookPair,
        ETHEREUM_WSTETH,
        ETHEREUM_USDC,
        None
    )]
    #[case::another_pair_at_a_measured_place(
        FillReads::Source,
        PairPlace::HookPair,
        ETHEREUM_WETH,
        ETHEREUM_USDC,
        None
    )]
    #[case::reading_only_its_word(
        FillReads::Word,
        PairPlace::HookPair,
        ETHEREUM_WSTETH,
        ETHEREUM_WETH,
        None
    )]
    #[case::reading_a_guarded_source(
        FillReads::GuardedSource,
        PairPlace::HookPair,
        ETHEREUM_WSTETH,
        ETHEREUM_WETH,
        None
    )]
    fn on_ethereum_only_a_measured_pair_has_a_figure(
        #[case] reads: FillReads,
        #[case] pair: PairPlace,
        #[case] base: Address,
        #[case] quote: Address,
        #[case] gas: Option<(u64, u64)>,
    ) {
        let mut state = weth_usdc_state();
        (state.base_token, state.quote_token) = (token(base), token(quote));
        state.reads = reads;
        state.pair = pair;
        state.gas_chain = GasChain::Ethereum;
        let (base, quote) = (state.base_token.clone(), state.quote_token.clone());

        let selling = state.get_amount_out(amount(500_000_000_000_000_000), &base, &quote);
        let buying = state.get_amount_out(amount(1_505_000_000), &quote, &base);

        assert_eq!(state.has_gas(), gas.is_some());
        match gas {
            Some((sell, buy)) => {
                assert_eq!((selling.unwrap().gas, buying.unwrap().gas), (amount(sell), amount(buy)))
            }
            None => {
                assert!(matches!(selling, Err(SimulationError::FatalError(_))), "{selling:?}");
                assert!(matches!(buying, Err(SimulationError::FatalError(_))), "{buying:?}");
            }
        }
        let through_fallback = state.with_fallback_router(true);
        assert!(through_fallback.has_gas());
        assert_eq!(
            through_fallback
                .get_amount_out(amount(500_000_000_000_000_000), &base, &quote)
                .unwrap()
                .gas,
            amount(50_445)
        );
    }

    /// A pair's place from `doorView`'s indices: the hook pair is base 0 and quote 1; past it, a
    /// pair with the pool's asset (its token 0) as its base, one with the asset as its quote, and
    /// one with neither.
    #[rstest]
    #[case::the_hook_pair(0, 1, PairPlace::HookPair)]
    #[case::the_asset_as_base(0, 2, PairPlace::PastWithAsset)]
    #[case::the_asset_as_quote_of_token_1(1, 0, PairPlace::PastWithAssetAsQuote)]
    #[case::the_asset_as_quote_of_token_2(2, 0, PairPlace::PastWithAssetAsQuote)]
    #[case::neither(2, 1, PairPlace::PastWithoutAsset)]
    fn a_pairs_place_follows_its_indices(
        #[case] base_index: u8,
        #[case] quote_index: u8,
        #[case] place: PairPlace,
    ) {
        assert_eq!(PairPlace::of(base_index, quote_index), place);
    }

    /// A state missing a field its gas class is read from is refused, not charged another class.
    #[rstest]
    #[case::what_it_reads("reads")]
    #[case::its_pair("pair")]
    #[case::its_chain("gas_chain")]
    fn a_state_missing_a_gas_class_field_is_refused(#[case] field: &str) {
        let mut json = serde_json::to_value(weth_usdc_state()).unwrap();
        json.as_object_mut()
            .unwrap()
            .remove(field);

        let state = serde_json::from_value::<EfficientPropState>(json);

        assert!(state.is_err(), "{state:?}");
    }

    /// A side whose print lapses before the book's life ends stops quoting at its deadline: here
    /// the bid's has passed and the ask's has not. A state read back quotes without one.
    #[test]
    fn a_side_past_its_deadline_quotes_nothing() {
        let past = Deadline { at: Instant::now(), unix_ms: 1 };
        let later = Deadline::at_unix_ms(61_000, 1_000);
        let state = weth_usdc_state().with_deadlines(Some(past), Some(later));

        let sell = state.get_amount_out(amount(500_000_000_000_000_000), &weth(), &usdc());
        let buy = state.get_amount_out(amount(3_010_000_000), &usdc(), &weth());

        assert!(matches!(sell, Err(SimulationError::RecoverableError(_))), "{sell:?}");
        assert!(state
            .get_limits(weth().address, usdc().address)
            .is_err());
        assert!(state
            .spot_price(&weth(), &usdc())
            .is_err());
        assert_eq!(buy.unwrap().amount, amount(1_000_000_000_000_000_000));
        let back: EfficientPropState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!((back.bid_until, back.ask_until), (None, None));
    }

    /// The door refuses a size it cannot fill whole: the state hands back the most it fills.
    #[test]
    fn a_size_past_the_largest_is_refused_with_the_most_the_listing_fills() {
        let state = weth_usdc_state();

        let error = state
            .get_amount_out(amount(2_000_000_000_000_000_000), &weth(), &usdc())
            .unwrap_err();

        let SimulationError::InvalidInput(_, Some(partial)) = &error else {
            panic!("expected the most the listing fills, got {error:?}");
        };
        assert_eq!(partial.amount, amount(2_990_000_000));
    }

    #[test]
    fn a_size_below_the_smallest_sample_is_refused() {
        let state = weth_usdc_state();

        let error = state
            .get_amount_out(amount(1_000), &weth(), &usdc())
            .unwrap_err();

        assert!(matches!(error, SimulationError::InvalidInput(_, None)), "{error:?}");
    }

    #[test]
    fn an_amount_past_uint256_is_refused() {
        let state = weth_usdc_state();

        let error = state
            .get_amount_out(BigUint::from(1u8) << 300, &weth(), &usdc())
            .unwrap_err();

        assert!(matches!(error, SimulationError::InvalidInput(_, _)), "{error:?}");
    }

    /// A fill moves the front, which the samples do not show, so the state after it quotes
    /// nothing either way.
    #[test]
    fn the_state_after_a_swap_quotes_nothing() {
        let state = weth_usdc_state();

        let result = state
            .get_amount_out(amount(500_000_000_000_000_000), &weth(), &usdc())
            .unwrap();
        let next = result
            .new_state
            .as_any()
            .downcast_ref::<EfficientPropState>()
            .unwrap();

        assert!(next.directions().is_empty());
        assert!(next
            .get_amount_out(amount(500_000_000_000_000_000), &weth(), &usdc())
            .is_err());
    }

    #[test]
    fn the_limits_are_the_largest_samples() {
        let state = weth_usdc_state();

        assert_eq!(
            state
                .get_limits(weth().address, usdc().address)
                .unwrap(),
            (amount(1_000_000_000_000_000_000), amount(2_990_000_000))
        );
        assert_eq!(
            state
                .get_limits(usdc().address, weth().address)
                .unwrap(),
            (amount(3_010_000_000), amount(1_000_000_000_000_000_000))
        );
    }

    /// 1,499 USDC for 0.5 WETH, at the smallest size.
    #[test]
    fn the_spot_price_is_the_rate_at_the_smallest_size() {
        let state = weth_usdc_state();

        assert_eq!(
            state
                .spot_price(&weth(), &usdc())
                .unwrap(),
            2_998.0
        );
    }

    #[test]
    fn a_side_with_no_samples_lists_no_direction_and_quotes_nothing() {
        let mut state = weth_usdc_state();
        state.ask = Ladder::from_samples(&[(U256::from(1u8), None)], None);

        assert_eq!(state.directions(), vec![(weth().address, usdc().address)]);
        assert!(state
            .get_amount_out(amount(1_000_000), &usdc(), &weth())
            .is_err());
        assert_eq!(
            state
                .get_limits(usdc().address, weth().address)
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
    }

    /// A credit listing's fills are charged its configured gas by path: a bid inside custody the
    /// custody figure, a larger bid the route's, the ask (all inside custody here) the custody
    /// figure buying base, and the most the bid fills, handed back, the route's. A bid between
    /// the two is refused with the custody's most handed back, at the custody's gas. The largest
    /// size custody fills, each side, is charged [`LAST_CUSTODY_FILL_REFUND`] less: it may take
    /// the pool's whole balance, which earns its transaction that much more back.
    #[rstest]
    #[case::a_custody_bid(50_000_000_000_000_000, true, Some(160_877))]
    #[case::a_custody_chord(75_000_000_000_000_000, true, Some(160_877))]
    #[case::the_largest_custody_bid(100_000_000_000_000_000, true, Some(151_277))]
    #[case::a_routed_bid(250_000_000_000_000_000, true, Some(610_501))]
    #[case::a_routed_chord(500_000_000_000_000_000, true, Some(610_501))]
    #[case::the_largest_routed_bid(1_000_000_000_000_000_000, true, Some(610_501))]
    #[case::a_custody_ask(1_505_000_000, false, Some(158_340))]
    #[case::the_largest_custody_ask(3_010_000_000, false, Some(148_740))]
    // 0.1 WETH is the most custody fills, 0.25 WETH the least the route was sampled at.
    #[case::across_the_step(200_000_000_000_000_000, true, None)]
    fn a_credit_state_charges_each_fill_its_paths_gas(
        #[case] amount_in: u64,
        #[case] bid: bool,
        #[case] gas: Option<u64>,
    ) {
        let state = weth_usdc_credit_state();
        let (token_in, token_out) = if bid { (weth(), usdc()) } else { (usdc(), weth()) };

        let result = state.get_amount_out(amount(amount_in), &token_in, &token_out);

        match gas {
            Some(gas) => assert_eq!(result.unwrap().gas, amount(gas)),
            None => {
                let Err(SimulationError::InvalidInput(message, Some(partial))) = &result else {
                    panic!("expected the custody's most handed back, got {result:?}");
                };
                assert!(message.contains("credit route"), "{message}");
                assert!(message.contains("consumed amount: 100000000000000000"), "{message}");
                assert_eq!(partial.gas, amount(151_277));
            }
        }
    }

    /// A credit listing's gas reads from its text form, `custody sell:custody buy:route
    /// sell:route buy`, and refuses any other.
    #[rstest]
    #[case::four_figures("137909:133427:925544:1035811", Some((137_909, 133_427, 925_544, 1_035_811)))]
    #[case::spaced(" 187000 : 192000 : 610501 : 610501 ", Some((187_000, 192_000, 610_501, 610_501)))]
    #[case::three_figures("137909:133427:925544", None)]
    #[case::five_figures("1:2:3:4:5", None)]
    #[case::a_zero("137909:0:925544:1035811", None)]
    #[case::no_number("137909:x:925544:1035811", None)]
    fn credit_gas_reads_from_its_text_form(
        #[case] text: &str,
        #[case] expected: Option<(u64, u64, u64, u64)>,
    ) {
        let figure = |gas: u64| NonZeroU64::new(gas).unwrap();
        let expected =
            expected.map(|(custody_sell, custody_buy, route_sell, route_buy)| CreditGas {
                custody: DirectionGas {
                    sell_base: figure(custody_sell),
                    buy_base: figure(custody_buy),
                },
                route: DirectionGas { sell_base: figure(route_sell), buy_base: figure(route_buy) },
            });

        assert_eq!(text.parse::<CreditGas>().ok(), expected);
    }

    /// The most a routed side fills is handed back at the route's gas, and a side every point of
    /// which routes charges the route's figure from its smallest size.
    #[test]
    fn a_routed_side_charges_the_routes_gas_to_its_partial_and_its_smallest_fill() {
        let mut state = weth_usdc_credit_state();
        state.ask = Ladder::new(state.ask.points().to_vec(), Some(0)).unwrap();

        let most = state
            .get_amount_out(amount(2_000_000_000_000_000_000), &weth(), &usdc())
            .unwrap_err();
        let smallest = state
            .get_amount_out(amount(1_505_000_000), &usdc(), &weth())
            .unwrap();

        let SimulationError::InvalidInput(_, Some(partial)) = most else {
            panic!("expected the most the listing fills, got {most:?}");
        };
        assert_eq!(partial.amount, amount(2_700_000_000));
        assert_eq!(partial.gas, amount(610_501));
        assert_eq!(smallest.gas, amount(187_000));
    }

    /// A state with a routed side and no credit gas, which no feed makes, refuses the routed
    /// fill rather than charge it a custody fill's gas; on Base, whose classes have figures, its
    /// custody fills still quote.
    #[test]
    fn a_routed_side_without_credit_gas_is_refused() {
        let mut state = weth_usdc_credit_state();
        state.credit = None;
        state.gas_chain = GasChain::Base;

        let routed = state.get_amount_out(amount(500_000_000_000_000_000), &weth(), &usdc());
        let custody = state.get_amount_out(amount(50_000_000_000_000_000), &weth(), &usdc());

        assert!(matches!(routed, Err(SimulationError::FatalError(_))), "{routed:?}");
        assert_eq!(custody.unwrap().gas, amount(SELL_BASE_SWAP_GAS));
    }

    /// A credit state round-trips with its route marks and its gas; a state of any other listing
    /// serializes no credit field, as before credit listings were served, and reads back without
    /// one.
    #[test]
    fn a_credit_state_round_trips_through_serde_with_its_marks() {
        let state: Box<dyn ProtocolSim> = Box::new(weth_usdc_credit_state());

        let json = serde_json::to_string(&state).unwrap();
        let back: Box<dyn ProtocolSim> = serde_json::from_str(&json).unwrap();

        assert!(back.eq(state.as_ref()));
        let back = back
            .as_any()
            .downcast_ref::<EfficientPropState>()
            .unwrap();
        assert!(back.bid.routes());
        assert_eq!(back.credit, Some(credit_gas()));
        let plain = serde_json::to_value(weth_usdc_state()).unwrap();
        assert!(plain.get("credit").is_none(), "{plain}");
        let mut zero = serde_json::to_value(weth_usdc_credit_state()).unwrap();
        zero["credit"]["route"]["sell_base"] = serde_json::json!(0);
        assert!(serde_json::from_value::<EfficientPropState>(zero).is_err());
    }

    #[test]
    fn a_state_round_trips_through_serde() {
        let state: Box<dyn ProtocolSim> = Box::new(weth_usdc_state());

        let json = serde_json::to_string(&state).unwrap();
        let back: Box<dyn ProtocolSim> = serde_json::from_str(&json).unwrap();

        assert!(back.eq(state.as_ref()));
    }
}
