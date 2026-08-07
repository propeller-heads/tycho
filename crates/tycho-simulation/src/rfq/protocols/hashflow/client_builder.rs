use std::collections::{HashMap, HashSet};

use tokio::time::Duration;
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use super::client::HashflowClient;
use crate::rfq::{errors::RFQError, protocols::utils::default_quote_tokens_for_chain};

/// `HashflowClientBuilder` is a builder pattern implementation for creating instances of
/// `HashflowClient`.
///
/// # Example
/// ```rust
/// use tycho_simulation::rfq::protocols::hashflow::client_builder::HashflowClientBuilder;
/// use tycho_common::{models::{token::Token, Chain}, Bytes};
/// use std::{collections::HashMap, str::FromStr, time::Duration};
///
/// let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
/// let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
/// let mut tokens = HashMap::new();
/// tokens.insert(weth.clone(), Token::new(&weth, "WETH", 18, 0, &[Some(15_000)], Chain::Ethereum, 100));
/// tokens.insert(usdc.clone(), Token::new(&usdc, "USDC", 6, 0, &[Some(40_000)], Chain::Ethereum, 100));
///
/// let client = HashflowClientBuilder::new(
///     Chain::Ethereum,
///     "auth_user".to_string(),
///     "auth_key".to_string()
/// )
/// .tokens(tokens)
/// .tvl_threshold(500.0)
/// .poll_time(Duration::from_secs(10))
/// .build()
/// .unwrap();
/// ```
pub struct HashflowClientBuilder {
    chain: Chain,
    auth_user: String,
    auth_key: String,
    tokens: HashMap<Bytes, Token>,
    tvl: f64,
    quote_tokens: Option<HashSet<Bytes>>,
    poll_time: Duration,
    quote_timeout: Duration,
}

impl HashflowClientBuilder {
    pub fn new(chain: Chain, auth_user: String, auth_key: String) -> Self {
        Self {
            chain,
            auth_user,
            auth_key,
            tokens: HashMap::new(),
            tvl: 100.0, // Default $100 minimum TVL
            quote_tokens: None,
            poll_time: Duration::from_secs(5), // Default 5 second polling
            quote_timeout: Duration::from_secs(5), // Default 5 second timeout
        }
    }

    /// Set the tokens for which to monitor prices, with their metadata
    pub fn tokens(mut self, tokens: HashMap<Bytes, Token>) -> Self {
        self.tokens = tokens;
        self
    }

    /// Set the minimum TVL threshold for pools
    pub fn tvl_threshold(mut self, tvl: f64) -> Self {
        self.tvl = tvl;
        self
    }

    /// Set custom quote tokens for TVL calculation
    /// If not set, will use chain-specific defaults
    pub fn quote_tokens(mut self, quote_tokens: HashSet<Bytes>) -> Self {
        self.quote_tokens = Some(quote_tokens);
        self
    }

    /// Set the polling interval for fetching price levels
    pub fn poll_time(mut self, poll_time: Duration) -> Self {
        self.poll_time = poll_time;
        self
    }

    /// Set the timeout for firm quote requests
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.quote_timeout = timeout;
        self
    }

    pub fn build(self) -> Result<HashflowClient, RFQError> {
        let quote_tokens;
        if let Some(tokens) = self.quote_tokens {
            quote_tokens = tokens;
        } else {
            quote_tokens = default_quote_tokens_for_chain(&self.chain)?
        }

        HashflowClient::new(
            self.chain,
            self.tokens,
            self.tvl,
            quote_tokens,
            self.auth_user,
            self.auth_key,
            self.poll_time,
            self.quote_timeout,
        )
    }
}
