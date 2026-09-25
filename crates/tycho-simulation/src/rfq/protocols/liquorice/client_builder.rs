use std::collections::HashSet;

use tokio::time::Duration;
use tycho_common::{models::Chain, Bytes};

use super::client::LiquoriceClient;
use crate::rfq::{
    errors::RFQError, models::QuoteRule, protocols::utils::default_quote_tokens_for_chain,
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
            quote_rule: QuoteRule::OncePerMaker,
        }
    }

    /// How often one route may take quotes from Liquorice. Every maker once, by default.
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
            self.quote_rule,
        )
    }
}
