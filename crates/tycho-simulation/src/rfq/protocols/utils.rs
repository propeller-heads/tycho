use std::{collections::HashSet, str::FromStr};

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
        _ => Ok(HashSet::new()),
    }
}
