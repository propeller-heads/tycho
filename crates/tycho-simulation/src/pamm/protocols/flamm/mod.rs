//! Efficient pools, the pools behind Everlong, as a book feed: the hook pair of a served pool is
//! quoted from the chain by the pool's own `previewSwap`, at one block, for Tycho's router as the
//! caller, and priced locally from that ladder of quotes. A poll reads each pool by one `eth_call`
//! of a sampler that is never deployed: the node runs it by state override at the router, so the
//! feed needs a node that takes state overrides, and falls back to nothing else.
//!
//! No pool, hook, price-feed, lender or policy arithmetic is ported. `previewSwap` takes the
//! swap's own branch and plan, asks the hook that prices the pair with the swap's request (the
//! caller as its sender) and checks what the pool's venues can fund now, reverting where the swap
//! would. A new venue kind or policy hook changes what the chain answers, not this module; a new
//! factory is one more address in [`FlammFeedBuilder::factories`].
//!
//! Anyone can create a pool over a hook and a price feed of their own, and a hook's quote and fill
//! are separate functions, so the feed serves only pools it has reason to trust: those whose hook
//! class ([`FlammFeedBuilder::hook_classes`], each listed with the [`HookShape`] of its answers)
//! and price feed ([`FlammFeedBuilder::price_feeds`] or
//! [`FlammFeedBuilder::price_feed_code_hashes`]) are listed, and those named in
//! [`FlammFeedBuilder::pools`], whatever they run, but for a hook of no listed class that shows a
//! leverage row. A new hook or feed is one more entry there. The lists are configuration: a builder
//! without them refuses to build. They cover the code, not whoever operates a pool: an operator can
//! still move a price between a quote and a fill, within the hook's own limits.
//!
//! At every quoted input the price is the pool's to the wei, less the haircut the feed is built
//! with. Between two quoted inputs a swap is priced on the chord between them, rounded down, where
//! one route of the pool's hook priced both, and at the lower one's output where the route
//! changes: an Efficient pool's hook labels each answer with the fee of the curve or leverage
//! route that priced it, and the ladder's rule settles the answers by those labels. A pool whose
//! answers fall in a way the rule cannot see can still be quoted above what it fills, and the
//! executor asks the pool for no minimum output
//! (`crates/tycho-execution/contracts/src/executors/PropAMMExecutor.sol:29-42`), so such a swap
//! fills below its quote rather than failing: only the router's minimum for the whole route
//! (`crates/tycho-execution/contracts/src/TychoRouterV3.sol:194`) bounds it. A swap moves only its
//! own side of the book.
//!
//! The haircut, 2 bps by default, is kept back from every quoted output, so a swap that fills at
//! the pool's own answer pays out about that much more than its quote. A route passes the quote to
//! the router as `expectedAmountOut` (`TychoRouterV3.sol:193`), and a live router keeps what is
//! paid above it for its fee receiver, not the swapper: positive slippage capture starts enabled
//! on every deployment (`crates/tycho-execution/contracts/src/FeeCalculator.sol:100-101`), covers
//! every client not exempted (`:276-289`) and credits the surplus to the router's fee receiver,
//! less any share the router gives the client (`:134-171`).
//!
//! A pool's pairs all draw on one inventory, and a book per pair would promise it once for each: a
//! route through two of them would price its second leg off the full ladder. So a pool of three or
//! more tokens is served its hook pair alone, tokens 0 and 1, until one component can carry every
//! direction of a pool and move them together.
//!
//! A pool a front prices, one whose hook its factory lists as a front or whose cheap lane is on, is
//! not served, whatever the lists, [`FlammFeedBuilder::pools`] or
//! [`FlammFeedBuilder::serve_front_priced`] say: its pairs trade through the front's listing
//! doors, which other components serve. Swaps run through the generic `PropAMMExecutor`, which
//! sends the pool the whole input before its swap
//! (`crates/tycho-execution/contracts/src/executors/PropAMMExecutor.sol:16-19`): a ladder ends at
//! the input a pool clips a quote to, so no quoted input is clipped.
use alloy::primitives::{address, Address};
use tycho_common::models::Chain;

pub mod feed;
#[cfg(test)]
mod fork;
mod gas;
mod gas_table;
mod guard;
mod ladder;
mod reader;
#[cfg(test)]
mod sampler_evm;
pub mod state;
#[cfg(test)]
mod test_support;

pub use feed::{FlammFeed, FlammFeedBuilder};
pub use gas::{gas_limit_addition, hook_class};
pub use reader::HookShape;
pub use state::FlammPairState;

/// Protocol system stamped on every pool pair; tycho-execution maps it to the generic
/// `PropAMMSwapEncoder` and `PropAMMExecutor`.
pub const PROTOCOL_SYSTEM: &str = "flamm";
/// Component type stamped on every pool pair.
pub(crate) const PROTOCOL_TYPE: &str = "flamm_pair";
/// Static attribute the generic `PropAMMSwapEncoder` reads: the address the executor calls, here
/// the pool.
pub const POOL_ATTRIBUTE: &str = "pamm_address";

/// Tycho's router on `chain`, the caller quotes are asked for: the router delegatecalls the
/// executor, so the pool sees the router as its caller and its hooks see it as the request's
/// sender. `None` on a chain the executor is not built for.
pub(crate) fn router(chain: Chain) -> Option<Address> {
    match chain {
        Chain::Ethereum => Some(address!("1644D2477f809cc2C71bCCFd6Dc9497E3F83210d")),
        Chain::Base => Some(address!("AbA5B53b03eAfaD1C5fc8BD5Fc765fC85Bb3de67")),
        Chain::Bsc => Some(address!("7f3D12BBaFb8955E51B3ab9588b34c8ad95BDA4e")),
        Chain::Starknet |
        Chain::ZkSync |
        Chain::Arbitrum |
        Chain::Unichain |
        Chain::Polygon |
        Chain::Plasma |
        Chain::Robinhood |
        Chain::Arc |
        Chain::Custom(_) |
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_common::Bytes;

    use super::*;

    /// The quotes are asked as the address the router swaps from, so the two crates must agree.
    #[rstest]
    #[case::ethereum(Chain::Ethereum)]
    #[case::base(Chain::Base)]
    #[case::bsc(Chain::Bsc)]
    fn quotes_are_asked_as_the_chains_router(#[case] chain: Chain) {
        let expected = tycho_execution::encoding::evm::get_router_address(&chain).unwrap();
        assert_eq!(&Bytes::from(router(chain).unwrap().to_vec()), expected);
    }

    #[test]
    fn no_other_chain_has_a_taker() {
        assert_eq!(router(Chain::Arbitrum), None);
    }
}
