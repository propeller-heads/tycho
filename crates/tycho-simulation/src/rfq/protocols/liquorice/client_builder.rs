use std::collections::HashSet;

use tokio::time::Duration;
use tycho_common::{models::Chain, Bytes};

use super::client::LiquoriceClient;
use crate::rfq::{
    errors::RFQError,
    models::{ComponentLayout, QuoteRule},
    protocols::utils::default_quote_tokens_for_chain,
};

pub struct LiquoriceClientBuilder {
    chain: Chain,
    auth_solver: String,
    auth_key: String,
    tokens: HashSet<Bytes>,
    tvl: f64,
    quote_tokens: Option<HashSet<Bytes>>,
    poll_time: Duration,
    quote_timeout: Duration,
    quote_expiry_secs: u64,
    component_layout: ComponentLayout,
    quote_rule: QuoteRule,
}

impl LiquoriceClientBuilder {
    pub fn new(chain: Chain, auth_solver: String, auth_key: String) -> Self {
        Self {
            chain,
            auth_solver,
            auth_key,
            tokens: HashSet::new(),
            tvl: 100.0,
            quote_tokens: None,
            poll_time: Duration::from_secs(5),
            quote_timeout: Duration::from_secs(5),
            quote_expiry_secs: 300,
            component_layout: ComponentLayout::PerPair,
            quote_rule: LiquoriceClient::DEFAULT_QUOTE_RULE,
        }
    }

    /// The components the client streams. One per pair, by default. Register a `PerPair` client
    /// with `LiquoriceState` and a `AllPairs` client with `LiquoriceAllPairsState`.
    pub fn component_layout(mut self, component_layout: ComponentLayout) -> Self {
        self.component_layout = component_layout;
        self
    }

    /// How often one route may take quotes from Liquorice, under the `AllPairs` layout. Every
    /// maker once, by default.
    pub fn quote_rule(mut self, quote_rule: QuoteRule) -> Self {
        self.quote_rule = quote_rule;
        self
    }

    pub fn tokens(mut self, tokens: HashSet<Bytes>) -> Self {
        self.tokens = tokens;
        self
    }

    pub fn tvl_threshold(mut self, tvl: f64) -> Self {
        self.tvl = tvl;
        self
    }

    pub fn quote_tokens(mut self, quote_tokens: HashSet<Bytes>) -> Self {
        self.quote_tokens = Some(quote_tokens);
        self
    }

    pub fn poll_time(mut self, poll_time: Duration) -> Self {
        self.poll_time = poll_time;
        self
    }

    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.quote_timeout = timeout;
        self
    }

    /// Set the expiry duration for quote requests in seconds
    pub fn quote_expiry_secs(mut self, secs: u64) -> Self {
        self.quote_expiry_secs = secs;
        self
    }

    pub fn build(self) -> Result<LiquoriceClient, RFQError> {
        let quote_tokens;
        if let Some(tokens) = self.quote_tokens {
            quote_tokens = tokens;
        } else {
            quote_tokens = default_quote_tokens_for_chain(&self.chain)?
        }

        LiquoriceClient::new(
            self.chain,
            self.tokens,
            self.tvl,
            quote_tokens,
            self.auth_solver,
            self.auth_key,
            self.poll_time,
            self.quote_timeout,
            self.quote_expiry_secs,
        )
        .map(|client| {
            client
                .with_component_layout(self.component_layout)
                .with_quote_rule(self.quote_rule)
        })
    }
}
