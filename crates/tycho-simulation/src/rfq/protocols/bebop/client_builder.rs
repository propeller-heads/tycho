use std::collections::HashSet;

use tokio::time::Duration;
use tycho_common::{models::Chain, Bytes};

use super::client::BebopClient;
use crate::rfq::{errors::RFQError, protocols::utils::default_quote_tokens_for_chain};

/// `BebopClientBuilder` is a builder pattern implementation for creating instances of
/// `BebopClient`.
///
/// The `origin_*` fields identify the flow behind binding quote requests. Bebop uses them for
/// abuse prevention and address screening, and can configure API accounts to require them,
/// rejecting quote requests in which a required field is missing. See
/// <https://docs.bebop.xyz/rfq-api/guides/best-practices#6-pass-origin-so-we-can-identify-legitimate-flow>.
///
/// Note: states decoded from the RFQ stream rebuild their client from the environment, not from
/// the client added to the stream builder. To apply origins on that path, set the
/// BEBOP_ORIGIN_ADDRESS, BEBOP_ORIGIN_TARGET and BEBOP_ORIGIN_SOURCE environment variables.
///
/// # Example
/// ```rust
/// use tycho_simulation::rfq::protocols::bebop::client_builder::BebopClientBuilder;
/// use tycho_common::{models::Chain, Bytes};
/// use std::{collections::HashSet, str::FromStr};
///
/// let mut tokens = HashSet::new();
/// tokens.insert(Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap()); // WETH
/// tokens.insert(Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap()); // USDC
///
/// let client = BebopClientBuilder::new(
///     Chain::Ethereum,
///     "ws_key".to_string()
/// )
/// .tokens(tokens)
/// .tvl_threshold(500.0)
/// .build()
/// .unwrap();
/// ```
pub struct BebopClientBuilder {
    chain: Chain,
    ws_key: String,
    tokens: HashSet<Bytes>,
    tvl: f64,
    quote_tokens: Option<HashSet<Bytes>>,
    quote_timeout: Duration,
    origin_address: Option<Bytes>,
    origin_target: Option<Bytes>,
    origin_source: Option<String>,
}

impl BebopClientBuilder {
    pub fn new(chain: Chain, ws_key: String) -> Self {
        Self {
            chain,
            ws_key,
            tokens: HashSet::new(),
            tvl: 100.0, // Default $100 minimum TVL
            quote_tokens: None,
            quote_timeout: Duration::from_secs(5), // Default 5 second timeout
            origin_address: None,
            origin_target: None,
            origin_source: None,
        }
    }

    /// Set the tokens for which to monitor prices
    pub fn tokens(mut self, tokens: HashSet<Bytes>) -> Self {
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

    /// Set the timeout for firm quote requests
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.quote_timeout = timeout;
        self
    }

    /// Set the real end-user's EOA, sent as `origin_address` with binding quote requests
    pub fn origin_address(mut self, origin_address: Bytes) -> Self {
        self.origin_address = Some(origin_address);
        self
    }

    /// Set the `to` address of the resulting transaction (e.g. the router contract), sent as
    /// `origin_target` with binding quote requests
    pub fn origin_target(mut self, origin_target: Bytes) -> Self {
        self.origin_target = Some(origin_target);
        self
    }

    /// Set a stable identifier for the upstream flow source, sent as `origin_source` with
    /// binding quote requests
    pub fn origin_source(mut self, origin_source: String) -> Self {
        self.origin_source = Some(origin_source);
        self
    }

    pub fn build(self) -> Result<BebopClient, RFQError> {
        let quote_tokens;
        if let Some(tokens) = self.quote_tokens {
            quote_tokens = tokens;
        } else {
            quote_tokens = default_quote_tokens_for_chain(&self.chain)?
        }

        BebopClient::new(
            self.chain,
            self.tokens,
            self.tvl,
            self.ws_key,
            quote_tokens,
            self.quote_timeout,
            self.origin_address,
            self.origin_target,
            self.origin_source,
        )
    }
}
