use std::collections::HashSet;

use tokio::time::Duration;
use tycho_common::{models::Chain, Bytes};

use super::client::EuclidClient;
use crate::rfq::{constants::DEFAULT_EUCLID_API_URL, errors::RFQError};

/// `EuclidClientBuilder` is a builder pattern implementation for creating instances of
/// `EuclidClient`.
///
/// The levels stream is public; the firm-quote endpoint takes an optional
/// per-solver `x-api-key` (request one from the Euclid team, or omit it for
/// deployments where the endpoint runs open).
///
/// # Example
/// ```rust
/// use tycho_simulation::rfq::protocols::euclid::client_builder::EuclidClientBuilder;
/// use tycho_common::{models::Chain, Bytes};
/// use std::{collections::HashSet, str::FromStr};
///
/// let mut tokens = HashSet::new();
/// tokens.insert(Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap()); // WETH
/// tokens.insert(Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap()); // USDC
///
/// let client = EuclidClientBuilder::new(Chain::Ethereum, Some("api_key".to_string()))
///     .tokens(tokens)
///     .tvl_threshold(500.0)
///     .build()
///     .unwrap();
/// ```
pub struct EuclidClientBuilder {
    chain: Chain,
    api_key: Option<String>,
    base_url: String,
    tokens: HashSet<Bytes>,
    tvl: f64,
    quote_timeout: Duration,
}

impl EuclidClientBuilder {
    pub fn new(chain: Chain, api_key: Option<String>) -> Self {
        Self {
            chain,
            api_key,
            base_url: DEFAULT_EUCLID_API_URL.to_string(),
            tokens: HashSet::new(),
            tvl: 100.0, // Default $100 minimum TVL
            quote_timeout: Duration::from_secs(5),
        }
    }

    /// Set the tokens for which to monitor prices
    pub fn tokens(mut self, tokens: HashSet<Bytes>) -> Self {
        self.tokens = tokens;
        self
    }

    /// Set the minimum TVL threshold for pairs (quote-token units)
    pub fn tvl_threshold(mut self, tvl: f64) -> Self {
        self.tvl = tvl;
        self
    }

    /// Override the API base URL (levels + firm endpoints derive from it)
    pub fn base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// Set the timeout for firm quote requests
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.quote_timeout = timeout;
        self
    }

    pub fn build(self) -> Result<EuclidClient, RFQError> {
        EuclidClient::new(
            self.chain,
            self.base_url,
            self.tokens,
            self.tvl,
            self.api_key,
            self.quote_timeout,
        )
    }
}
