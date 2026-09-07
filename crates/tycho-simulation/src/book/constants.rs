use std::collections::HashSet;

use hex_literal::hex;
use tycho_common::{models::Chain, Bytes};

/// The chain's curated USD stablecoins: the recommended `usd_quote_tokens` for the feed builders
/// that normalize book TVL from price levels into USD before applying the threshold. `None` for
/// chains without a curated set — pass your own then.
pub fn usd_stablecoins_for_chain(chain: &Chain) -> Option<HashSet<Bytes>> {
    let tokens = match chain {
        Chain::Ethereum => HashSet::from([
            Bytes::from(hex!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")), // USDC
            Bytes::from(hex!("dac17f958d2ee523a2206206994597c13d831ec7")), // USDT
            Bytes::from(hex!("6b175474e89094c44da98b954eedeac495271d0f")), // DAI
            Bytes::from(hex!("4c9EDD5852cd905f086C759E8383e09bff1E68B3")), // USDe
        ]),
        Chain::Base => HashSet::from([
            Bytes::from(hex!("833589fcd6edb6e08f4c7c32d4f71b54bda02913")), // USDC
            Bytes::from(hex!("fde4c96c8593536e31f229ea8f37b2ada2699bb2")), // USDT
        ]),
        _ => return None,
    };
    Some(tokens)
}
