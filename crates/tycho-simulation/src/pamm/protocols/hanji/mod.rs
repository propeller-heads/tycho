//! Hanji on Base: central limit order books whose depth is virtual liquidity placed by a quoter
//! around each swap, quoted and executed through one proxy orderbook per market.
//!
//! Reference: <https://docs.hanji.io/for-developers/liquidity-source-integration>
//!
//! # Where the books come from
//!
//! Hanji's aggregator API (`/aggregators/markets`) lists the markets and the proxy orderbook of
//! each. The feed reads that list on its first poll and again every
//! [`HanjiFeedBuilder::market_refresh`] after. A refresh the API fails, or does not answer within
//! half the poll's request timeout, keeps the last list and is retried 30 s later. Every poll then
//! reads each listed market over JSON-RPC, all at one block in one `Multicall3.aggregate3` (see
//! `reader.rs`). Events cannot rebuild these books: the quoter places its orders inside the swap
//! and removes them after, so no event ever records them.
//!
//! # Whose book
//!
//! A proxy fills a caller against the orders of one of two quoters. The transaction's sender and
//! callers on the proxy's whitelist (`whitelisted(address)`) get its FastQuoter
//! (`0x598877e528437c0b4429e910d0bba850708cd98c` on the three ETH and cbBTC markets); any other
//! contract, such as Tycho's router, gets a second quoter
//! (`0x611c491d84469c4498b2c4ffd56fd3adc7ce5a1f`) whose book is wider: at Base block 61,693,481
//! its best bid sat 2.0 bps under FastQuoter's on ETH/USDC, 2.2 bps on cbBTC/USDC and 6.2 bps on
//! cbBTC/ETH. FastQuoterHelper, which Hanji's documentation points aggregators to, reports only
//! FastQuoter's book. The feed reads both quoters' books, and whether its taker (Tycho's router
//! unless [`HanjiFeedBuilder::taker`] says otherwise) is whitelisted, and publishes the book the
//! proxy will fill the taker against. The router was not whitelisted on any market at that block.
//! The tokenized-equity markets name no second quoter and run other implementations, and their
//! fills and gas were not recorded, so the feed does not read them, whoever the taker. A paused
//! proxy reverts every fill while its quoters still report a book, so a paused market is left out.
//!
//! # One inventory, re-priced on every fill
//!
//! One FastQuoter and one contract quoter serve all three ETH and cbBTC markets, from one shared
//! inventory, and after every fill they place a new book on all three, at worse prices for a
//! second fill that takes the same token out. Measured on a fork of Base block 61,711,413: after a
//! cbBTC/USDC sell of about $2k a second cbBTC/USDC sell filled 1.5 bps under the levels the first
//! left, and 10.8 bps under after one of about $18k; after the $18k sell, selling ETH on ETH/USDC
//! filled 10.7 bps under its book. Depth came back each time, so the executor fills such a swap in
//! full, and only the route's `minAmountOut` meets the difference. No state here models it: a
//! simulated swap leaves the side it walked empty, so a route cannot use the same market twice in
//! one direction, but a route through two of these markets that takes the same token out of both
//! is over-quoted by about 0.75 bps per $1k the first hop took.
//!
//! # Freshness
//!
//! A fleet of keeper addresses pushes the quoters' prices several times a second (selector
//! `0xae7e8d81`). The book is empty from about 3 s after the last push, measured at Base block
//! 61,595,000 with `eth_call` time overrides. The feed publishes only reads from a block at most
//! 1 s behind the wall clock and withdraws a book 2 s after the poll that read it, so no book is
//! served more than 3 s after its block; a swap that lands after the book went empty fills
//! nothing, and the executor reverts it. The quoters also price off four Base pools seen in their
//! call traces (two Uniswap V3, one PancakeSwap V3 and one Aerodrome CL, whose TWAP they read), so
//! a route that trades through one of those before Hanji meets a different Hanji book than the one
//! simulated.
//!
//! # How a market fills
//!
//! A market trades token X (the base) against token Y (the quote). Prices are Y units per X unit
//! (`shares`), both scaled by the market's `scalingFactorTokenX`/`scalingFactorTokenY`. Recorded
//! at Base block 61,693,481 (`testdata/`) for both kinds of caller, with each output measured as
//! the caller's balance gain, and matched wei for wei by [`HanjiState`] against the book each kind
//! gets:
//!
//! - Selling X sends `placeOrder` with `floor(amountIn / scalingX)` shares. They fill best bid
//!   first, and the taker receives `(value - fee) * scalingY`, where `value` is the sum of `shares
//!   * price` over the levels filled and `fee = ceil(value * rate / 1e18)`.
//! - Buying X sends `placeMarketOrderWithTargetValue` with a target of `floor(amountIn /
//!   scalingY)`. It buys, best ask first, the most shares whose `value + fee` fits in the target,
//!   and the taker receives `shares * scalingX`.
//!
//! The rate is `totalAggressiveCommissionRate + passiveOrderPayoutRate` from `getConfig()`; the
//! fee rounds up, where the documented formula reads as rounding down. `passiveOrderPayoutRate`
//! was zero on every recorded market, so whether a taker pays it is not measured. The proxy pulls
//! only what it fills and does not revert when its book runs out; tycho-execution's executor
//! reverts a fill short of the input instead. Within that, the input left over below one share
//! (selling) or one share's cost (buying) stays with the caller, which for a swap through Tycho's
//! router is the router, and a native input the proxy does not use is refunded to it. The quotes
//! are the exact fill of the input; the leftover is not returned to the user.
//!
//! # Native ETH
//!
//! On a market that trades ETH (`supportsNativeEth`), the ETH side is always paid out as native
//! ETH, whichever side it is, and taken as `msg.value`. Components therefore carry the chain's
//! native token (the zero address) in place of WETH, which the API lists for it.
//!
//! # What this integration depends on
//!
//! The proxies and both quoters are not source-verified, and each market's proxy runs its own
//! implementation. Beside what Hanji documents (the API's market list, `getConfig()`,
//! `assembleOrderbookFromOrders` and the two swap methods), the feed calls three unnamed getters
//! of the proxies, identified by what they return (`reader.rs`), and `whitelisted(address)`. The
//! fill rules and the choice of quoter were measured rather than read from source. A change to how
//! the proxy fills against the books its quoters report would break the quotes without failing any
//! read; re-recording the fixture with `testdata/record_fixture.py` and running the state tests
//! checks them.

use alloy::primitives::{address, Address};
use tycho_common::models::Chain;

pub mod feed;
mod market;
mod reader;
pub mod state;

pub use feed::{HanjiFeed, HanjiFeedBuilder};
pub use state::HanjiState;

/// Protocol system stamped on every component this integration emits; tycho-execution keys
/// Hanji's encoder on it.
pub const PROTOCOL_SYSTEM: &str = "hanji";

/// Component type stamped on every market this integration emits.
pub(crate) const PROTOCOL_TYPE: &str = "hanji_orderbook";

/// Static attribute holding the market's proxy orderbook, which the encoder calls.
pub const ORDERBOOK_ATTRIBUTE: &str = "orderbook";

/// Where one chain's Hanji markets are listed and read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Deployment {
    pub chain: Chain,
    /// The aggregator API's market list.
    pub markets_url: &'static str,
    /// The token the API and `getConfig()` name for the native side of a native market.
    pub wrapped_native: Address,
}

/// The Base deployment.
pub(crate) const BASE_DEPLOYMENT: Deployment = Deployment {
    chain: Chain::Base,
    markets_url: "https://api-base.hanji.io/aggregators/markets",
    wrapped_native: address!("4200000000000000000000000000000000000006"),
};

/// Tycho's default router on Base, the taker the books are read for unless
/// [`HanjiFeedBuilder::taker`] sets another.
pub(crate) const BASE_ROUTER: Address = address!("d374c00614f95492D816b5957E3797ce3f0c445C");

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use tycho_execution::encoding::evm::{
        get_router_address, HANJI_ORDERBOOK_ATTRIBUTE, HANJI_PROTOCOL_SYSTEM,
    };

    use super::*;

    /// By default the books are read for the router tycho-execution encodes Hanji swaps through.
    #[test]
    fn the_books_are_read_for_the_base_router_by_default() {
        let router = get_router_address(&Chain::Base).unwrap();

        assert_eq!(router.as_ref(), BASE_ROUTER.as_slice());
    }

    /// tycho-execution keys Hanji's encoder on the protocol system and reads the orderbook from
    /// the attribute, keeping its own copy of each; this holds the two crates to one spelling.
    #[test]
    fn the_component_carries_the_names_the_encoder_reads() {
        assert_eq!(PROTOCOL_SYSTEM, HANJI_PROTOCOL_SYSTEM);
        assert_eq!(ORDERBOOK_ATTRIBUTE, HANJI_ORDERBOOK_ATTRIBUTE);
    }
}
