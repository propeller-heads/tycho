use std::{collections::HashSet, str::FromStr};

use alloy_primitives::Address;
use tycho_common::{models::Chain, Bytes};

use crate::rfq::errors::RFQError;

fn str_to_bytes(address: &str) -> Result<Bytes, RFQError> {
    Bytes::from_str(address).map_err(|_| {
        RFQError::FatalError(format!("Failed to parse default quote token: {address}"))
    })
}

/// Returns default quote tokens for TVL calculation based on the chain
pub fn default_quote_tokens_for_chain(chain: &Chain) -> Result<HashSet<Bytes>, RFQError> {
    match chain {
        Chain::Ethereum => Ok(HashSet::from([
            str_to_bytes("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")?, // USDC
            str_to_bytes("0xdac17f958d2ee523a2206206994597c13d831ec7")?, // USDT
            str_to_bytes("0x6b175474e89094c44da98b954eedeac495271d0f")?, // DAI
            str_to_bytes("0x4c9EDD5852cd905f086C759E8383e09bff1E68B3")?, // USDe
        ])),
        Chain::Base => Ok(HashSet::from([
            str_to_bytes("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913")?, // USDC
            str_to_bytes("0xfde4c96c8593536e31f229ea8f37b2ada2699bb2")?, // USDT
        ])),
        Chain::Arbitrum => Ok(HashSet::from([
            str_to_bytes("0xaf88d065e77c8cC2239327C5EDb3A432268e5831")?, // USDC
            str_to_bytes("0xFd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9")?, // USDT (USD₮0)
        ])),
        Chain::Bsc => Ok(HashSet::from([
            str_to_bytes("0x55d398326f99059fF775485246999027B3197955")?, // USDT
            str_to_bytes("0x8AC76a51cc950d9822D68b83fE1Ad97B32Cd580d")?, // USDC
        ])),
        Chain::Robinhood => Ok(HashSet::from([
            str_to_bytes("0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168")?, // USDG
        ])),
        _ => Ok(HashSet::new()),
    }
}

pub fn bytes_to_address(address: &Bytes) -> Result<Address, RFQError> {
    if address.len() == 20 {
        Ok(Address::from_slice(address))
    } else {
        Err(RFQError::InvalidInput(format!(
            "Invalid EVM address length: expected 20 bytes, got {}",
            address.len()
        )))
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::ethereum_usdc(Chain::Ethereum, "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")]
    #[case::base_usdc(Chain::Base, "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913")]
    #[case::arbitrum_usdc(Chain::Arbitrum, "0xaf88d065e77c8cc2239327c5edb3a432268e5831")]
    #[case::arbitrum_usdt(Chain::Arbitrum, "0xfd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9")]
    #[case::bsc_usdt(Chain::Bsc, "0x55d398326f99059ff775485246999027b3197955")]
    #[case::bsc_usdc(Chain::Bsc, "0x8ac76a51cc950d9822d68b83fe1ad97b32cd580d")]
    #[case::robinhood_usdg(Chain::Robinhood, "0x5fc5360d0400a0fd4f2af552add042d716f1d168")]
    fn default_quote_tokens_include_chain_stablecoin(#[case] chain: Chain, #[case] token: &str) {
        let tokens = default_quote_tokens_for_chain(&chain).unwrap();

        assert!(tokens.contains(&Bytes::from_str(token).unwrap()));
    }

    #[test]
    fn default_quote_tokens_are_empty_for_chains_without_rfq_coverage() {
        assert!(default_quote_tokens_for_chain(&Chain::Polygon)
            .unwrap()
            .is_empty());
    }
}
