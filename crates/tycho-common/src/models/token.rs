use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::Arc,
};

use deepsize::DeepSizeOf;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};

use super::{Address, Balance};
use crate::{dto::ResponseToken, models::Chain, traits::TokenOwnerFinding, Bytes};

/// Cost related to a token transfer, for example amount of gas in evm chains.
pub type TransferCost = u64;

/// Tax related to a token transfer. Should be given in Basis Points (1/100th of a percent)
pub type TransferTax = u64;

#[derive(Debug, Clone, Deserialize, Serialize, Eq, DeepSizeOf)]
pub struct Token {
    pub address: Bytes,
    pub symbol: String,
    pub decimals: u32,
    pub tax: TransferTax,
    pub gas: Vec<Option<TransferCost>>,
    pub chain: Chain,
    /// Quality is between 0-100, where:
    ///  - 100: Normal token
    ///  - 75: Rebase token
    ///  - 50: Fee token
    ///  - 10: Token analysis failed at creation
    ///  - 9-5: Token analysis failed on cronjob (after creation).
    ///  - 0: Failed to extract decimals onchain
    pub quality: u32,
}

impl Token {
    pub fn new(
        address: &Bytes,
        symbol: &str,
        decimals: u32,
        tax: u64,
        gas: &[Option<u64>],
        chain: Chain,
        quality: u32,
    ) -> Self {
        Self {
            address: address.clone(),
            symbol: symbol.to_string(),
            decimals,
            tax,
            gas: gas.to_owned(),
            chain,
            quality,
        }
    }

    /// One
    /// Get one token in BigUint format
    ///
    /// ## Return
    /// Returns one token as BigUint
    pub fn one(&self) -> BigUint {
        BigUint::from((1.0 * 10f64.powi(self.decimals as i32)) as u128)
    }

    pub fn gas_usage(&self) -> BigUint {
        BigUint::from(
            self.gas
                .clone()
                .into_iter()
                .flatten()
                .collect::<Vec<u64>>()
                .iter()
                .min()
                .copied()
                .unwrap_or(0u64),
        )
    }
}

impl PartialOrd for Token {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.address.partial_cmp(&other.address)
    }
}

impl PartialEq for Token {
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address
    }
}

impl Hash for Token {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.address.hash(state);
    }
}

impl From<Arc<Token>> for Address {
    fn from(val: Arc<Token>) -> Self {
        val.address.clone()
    }
}

impl From<ResponseToken> for Token {
    fn from(value: ResponseToken) -> Self {
        Self {
            chain: value.chain.into(),
            address: value.address,
            symbol: value.symbol,
            decimals: value.decimals,
            tax: value.tax,
            gas: value.gas,
            quality: value.quality,
        }
    }
}

/// Represents the quality of a token.
///
/// * `Good`: Indicates that the token has successfully passed the analysis process.
/// * `Bad`: Indicates that the token has failed the analysis process. In this case, a detailed
///   reason for the failure is provided.
///
/// Note: Transfer taxes do not impact the token's quality.
/// Even if a token has transfer taxes, as long as it successfully passes the analysis,
/// it will still be marked as `Good`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TokenQuality {
    Good,
    Bad { reason: String },
}

impl TokenQuality {
    pub fn is_good(&self) -> bool {
        matches!(self, Self::Good { .. })
    }

    pub fn bad(reason: impl ToString) -> Self {
        Self::Bad { reason: reason.to_string() }
    }
}

/// A store for tracking token owners and their balances.
///
/// The `TokenOwnerStore` maintains a mapping between token addresses and their respective
/// owner's address and balance. It can be used to quickly retrieve token owner information
/// without needing to query external sources.
#[derive(Debug)]
pub struct TokenOwnerStore {
    /// A `HashMap` where the key is the token address and the value is a tuple containing
    /// the owner address and the balance of the owner for the token.
    values: HashMap<Address, (Address, Balance)>,
}

impl TokenOwnerStore {
    pub fn new(values: HashMap<Address, (Address, Balance)>) -> Self {
        TokenOwnerStore { values }
    }
}

#[async_trait::async_trait]
impl TokenOwnerFinding for TokenOwnerStore {
    async fn find_owner(
        &self,
        token: Address,
        _min_balance: Balance,
    ) -> Result<Option<(Address, Balance)>, String> {
        Ok(self.values.get(&token).cloned())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_constructor() {
        let token = Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC",
            6,
            1000,
            &[Some(1000u64)],
            Chain::Ethereum,
            100,
        );

        assert_eq!(token.symbol, "USDC");
        assert_eq!(token.decimals, 6);
        assert_eq!(
            format!("{token_address:#x}", token_address = token.address),
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
    }

    #[test]
    fn test_cmp() {
        let usdc = Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC",
            6,
            1000,
            &[Some(1000u64)],
            Chain::Ethereum,
            100,
        );
        let usdc2 = Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC2",
            6,
            1000,
            &[Some(1000u64)],
            Chain::Ethereum,
            100,
        );
        let weth = Token::new(
            &Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            "WETH",
            18,
            1000,
            &[Some(1000u64)],
            Chain::Ethereum,
            100,
        );

        assert!(usdc < weth);
        assert_eq!(usdc, usdc2);
    }

    #[test]
    fn test_one() {
        let usdc = Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC",
            6,
            1000,
            &[Some(1000u64)],
            Chain::Ethereum,
            100,
        );

        assert_eq!(usdc.one(), BigUint::from(1000000u64));
    }
}
