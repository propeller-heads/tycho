//! BaiBai on Base: a maker-published curve per base token, quoted against one quote
//! token and filled from one shared custodian.
//!
//! The feed reads each pair from the chain over JSON-RPC every poll, through the CurveBook's own
//! getters, and prices swaps locally with the contract's arithmetic. The implementations behind
//! the three proxies are not source-verified; the arithmetic was checked wei for wei against the
//! deployed `CurveBook.quote` and `Entrypoint.quoteFor` (see `testdata/`), and an upgrade that
//! changes it needs this module updated.
//!
//! # Priority surcharge
//!
//! The entrypoint withholds a further 10 bps of the gross output from a swap whose transaction
//! gas price exceeds a threshold: the greater of an entrypoint-wide floor and the pair's
//! `CurveBook.updatePriorityFee`. Measured at Base block 53,234,728 the threshold was 8e6 wei
//! (0.008 gwei), set by the floor. The books do not price it, since the gas price is the sending
//! client's choice and a quote does not carry one: a route through BaiBai sent above the
//! threshold receives up to 10 bps less than quoted, and reverts when its slippage tolerance is
//! tighter than that.
//!
//! Reference: <https://docs.baibai.cx/takers/quoting-and-swapping>

use alloy::primitives::{address, Address};

pub mod feed;
mod math;
mod reader;
pub mod state;

pub use feed::{BaibaiFeed, BaibaiFeedBuilder};
pub use state::BaibaiState;

/// Protocol system stamped on every component this integration emits; tycho-execution keys
/// BaiBai's encoder on it.
pub const PROTOCOL_SYSTEM: &str = "baibai";

/// Component type stamped on every pair this integration emits.
pub(crate) const PROTOCOL_TYPE: &str = "baibai_pair";

/// Static attributes naming the pair's base and quote token, which the encoder reads to choose
/// the base and the direction.
pub const BASE_ATTRIBUTE: &str = "base";
pub const QUOTE_ATTRIBUTE: &str = "quote";

/// The three proxies of one BaiBai deployment and the quote token every pair trades against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Deployment {
    pub entrypoint: Address,
    pub curve_book: Address,
    pub custodian: Address,
    pub quote: Address,
}

/// The Base deployment, quoted in USDC.
pub(crate) const BASE_DEPLOYMENT: Deployment = Deployment {
    entrypoint: address!("98c1D9E102Eb2806D902b13186BDc7892aC4fFBa"),
    curve_book: address!("604d9b9eB1e1571C78661a6C1088427EC9c8c6E5"),
    custodian: address!("AaC48FEB93c5C97E0fb3c7C57E1633922A4ACDa3"),
    quote: address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"),
};

/// The bases listed on the Base deployment: WETH and cbBTC. The CurveBook has no enumerable
/// list of pairs, so a base listed later is served once it is passed to
/// [`BaibaiFeedBuilder::bases`].
pub(crate) const BASE_LISTED_BASES: [Address; 2] = [
    address!("4200000000000000000000000000000000000006"),
    address!("cbB7C0000aB88B473b1f5aFd9ef808440eed33Bf"),
];

/// The entrypoint's fee on Tycho's router, in millionths of the gross output: the venue default
/// every unlisted caller pays. Set by hand rather than read from the chain; becomes zero once
/// BaiBai whitelists the router. Above the live fee the books under-quote; below it a swap
/// receives less than quoted and the router's minimum output reverts it.
pub(crate) const ROUTER_FEE_PIPS: u32 = 50;

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use tycho_execution::encoding::evm::{
        BAIBAI_BASE_ATTRIBUTE, BAIBAI_PROTOCOL_SYSTEM, BAIBAI_QUOTE_ATTRIBUTE,
    };

    use super::*;

    /// tycho-execution keys BaiBai's encoder on the protocol system and reads the pair from the
    /// two attributes, keeping its own copy of each; this holds the two crates to one spelling.
    #[test]
    fn the_component_carries_the_names_the_encoder_reads() {
        assert_eq!(PROTOCOL_SYSTEM, BAIBAI_PROTOCOL_SYSTEM);
        assert_eq!(BASE_ATTRIBUTE, BAIBAI_BASE_ATTRIBUTE);
        assert_eq!(QUOTE_ATTRIBUTE, BAIBAI_QUOTE_ATTRIBUTE);
    }
}
