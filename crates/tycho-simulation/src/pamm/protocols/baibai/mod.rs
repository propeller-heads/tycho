//! BaiBai on Base: a maker-published curve per base token, quoted against one quote
//! token and filled from one shared custodian.
//!
//! The feed reads each pair from the chain over JSON-RPC every poll, through the CurveBook's own
//! getters, and prices swaps locally with the contract's arithmetic. The implementations behind
//! the three proxies are not source-verified; the arithmetic was checked wei for wei against the
//! deployed `CurveBook.quote` and `Entrypoint.quoteFor` (see `testdata/`), and an upgrade that
//! changes it needs this module updated. The Entrypoint was last upgraded at Base block
//! 53,492,231; the newest recording (53,545,000) postdates it.
//!
//! Only spreads of zero and depths of 0 or 100% have been seen live, so the spread and partial
//! depth paths of the arithmetic follow the contract's observed rounding but have not been
//! compared with it.
//!
//! # Priority surcharge
//!
//! The entrypoint withholds part of the gross output from a swap whose priority fee (gas price
//! less basefee) exceeds 2e6 wei (0.002 gwei): one hundredth of a basis point per 1e4 wei above
//! that, rounded down, capped at 50 bps (reached at 0.052 gwei), and the withheld amount rounded
//! up. Measured at Base block 53,545,000, after the Entrypoint upgrade at 53,492,231; before it,
//! the rule was a flat 10 bps above a gas price of 8e6 wei. The books do not price it, since the
//! priority fee is the sending client's choice and a quote does not carry one: a route through
//! BaiBai sent with a tip above 0.002 gwei receives up to 50 bps less than quoted, and reverts
//! when its slippage tolerance is tighter than that.
//!
//! # What this integration depends on
//!
//! The arithmetic, the expiry rules, the fee and the gas model were measured against one
//! implementation behind each proxy, pinned in [`BASE_DEPLOYMENT`]:
//!
//! | Proxy      | Implementation | Since Base block                         |
//! |------------|----------------|------------------------------------------|
//! | Entrypoint | `0xCc61…F3B0`  | 53,492,231                               |
//! | CurveBook  | `0x3E6A…9f1d`  | 52,963,996                               |
//! | Custodian  | `0x997b…3076`  | unchanged at every block checked from 51,000,000 |
//!
//! A Safe (`0xB1F4E4d67E330CC505e526488bAe0388e616A907`) owns all three and can upgrade any of
//! them. None is source-verified, so every upgrade is treated as one that may change the
//! arithmetic: each poll reads the three EIP-1967 implementation slots, and one that differs from
//! its pin ends the feed. Accepting a new implementation means re-recording a fixture with
//! `testdata/record_fixture.py` and comparing its quotes wei for wei, re-measuring gas and the
//! priority surcharge on a fork (tycho-execution's `Baibai.t.sol`), then updating the pin.
//!
//! The pins do not catch the owner changing parameters in storage. The TTL and the taker fee are
//! read every poll, so a change to them reaches the books. The priority surcharge's threshold
//! (view `0xc256865c`) and cap (`prioritySurchargeBps()`) are not read; a change to them would
//! leave the section above stale, though the books do not price the surcharge either way.
//!
//! Reference: <https://docs.baibai.cx/takers/quoting-and-swapping>

use alloy::primitives::{address, Address};
use tycho_common::models::Chain;

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

/// The three proxies of one BaiBai deployment, the implementation each was verified against, the
/// chain they are on and the quote token every pair trades against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Deployment {
    pub chain: Chain,
    pub entrypoint: Address,
    pub curve_book: Address,
    pub custodian: Address,
    pub entrypoint_implementation: Address,
    pub curve_book_implementation: Address,
    pub custodian_implementation: Address,
    pub quote: Address,
}

impl Deployment {
    /// Each proxy's name, address and pinned implementation.
    pub(crate) fn proxies(&self) -> [(&'static str, Address, Address); 3] {
        [
            ("Entrypoint", self.entrypoint, self.entrypoint_implementation),
            ("CurveBook", self.curve_book, self.curve_book_implementation),
            ("Custodian", self.custodian, self.custodian_implementation),
        ]
    }
}

/// The Base deployment, quoted in USDC.
pub(crate) const BASE_DEPLOYMENT: Deployment = Deployment {
    chain: Chain::Base,
    entrypoint: address!("98c1D9E102Eb2806D902b13186BDc7892aC4fFBa"),
    curve_book: address!("604d9b9eB1e1571C78661a6C1088427EC9c8c6E5"),
    custodian: address!("AaC48FEB93c5C97E0fb3c7C57E1633922A4ACDa3"),
    entrypoint_implementation: address!("Cc61F0ACB73EeA94c160aF15dA4471b9B456F3B0"),
    curve_book_implementation: address!("3E6AbC41E9f41D7F6F7f26a6bf8851434fa69f1d"),
    custodian_implementation: address!("997b2D1b539DFccC9D42f2d10Fe804F0Ac9b3076"),
    quote: address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"),
};

/// The bases listed on the Base deployment: WETH and cbBTC. The CurveBook cannot enumerate its
/// pairs, so a base listed later is served only once it is passed to
/// [`BaibaiFeedBuilder::bases`]; the feed never discovers one. The book emits
/// `PairListed(address indexed base, address indexed quote)` with a base's first curve, from
/// block 51,696,183 on, where an upgrade emitted WETH's after the fact. Up to block 53,831,402
/// the only other one is cbBTC's, at 52,869,661, so a later `PairListed` log names a base to add
/// here.
pub(crate) const BASE_LISTED_BASES: [Address; 2] = [
    address!("4200000000000000000000000000000000000006"),
    address!("cbB7C0000aB88B473b1f5aFd9ef808440eed33Bf"),
];

/// Tycho's default router on Base, the taker the books are priced for unless
/// [`BaibaiFeedBuilder::taker`] sets another. BaiBai whitelisted it at a zero fee for every base
/// at block 53,359,421, where other callers pay the venue default of 50 millionths.
pub(crate) const BASE_ROUTER: Address = address!("AbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67");

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use tycho_execution::encoding::evm::{
        get_router_address, BAIBAI_BASE_ATTRIBUTE, BAIBAI_PROTOCOL_SYSTEM, BAIBAI_QUOTE_ATTRIBUTE,
    };

    use super::*;

    /// By default the books are priced for the router tycho-execution encodes BaiBai swaps
    /// through.
    #[test]
    fn the_books_are_priced_for_the_base_router_by_default() {
        let router = get_router_address(&Chain::Base).unwrap();

        assert_eq!(router.as_ref(), BASE_ROUTER.as_slice());
    }

    /// tycho-execution keys BaiBai's encoder on the protocol system and reads the pair from the
    /// two attributes, keeping its own copy of each; this holds the two crates to one spelling.
    #[test]
    fn the_component_carries_the_names_the_encoder_reads() {
        assert_eq!(PROTOCOL_SYSTEM, BAIBAI_PROTOCOL_SYSTEM);
        assert_eq!(BASE_ATTRIBUTE, BAIBAI_BASE_ATTRIBUTE);
        assert_eq!(QUOTE_ATTRIBUTE, BAIBAI_QUOTE_ATTRIBUTE);
    }
}
