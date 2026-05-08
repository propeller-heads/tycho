use std::collections::HashSet;

use tokio::time::Duration;
use tycho_common::{models::Chain, Bytes};

use super::client::MetricClient;
use crate::rfq::{errors::RFQError, protocols::utils::default_quote_tokens_for_chain};

pub struct MetricClientBuilder {
    chain: Chain,
    tokens: HashSet<Bytes>,
    tvl: f64,
    quote_tokens: Option<HashSet<Bytes>>,
    base_url: String,
    secret_key: Option<String>,
    poll_time: Duration,
    quote_timeout: Duration,
}

impl MetricClientBuilder {
    pub fn new(chain: Chain) -> Self {
        Self {
            chain,
            tokens: HashSet::new(),
            tvl: 0.0,
            quote_tokens: None,
            base_url: "http://54.199.103.16:8080".to_string(),
            secret_key: None,
            poll_time: Duration::from_secs(5),
            quote_timeout: Duration::from_secs(5),
        }
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

    pub fn base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    pub fn secret_key(mut self, secret_key: Option<String>) -> Self {
        self.secret_key = secret_key;
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

    pub fn build(self) -> Result<MetricClient, RFQError> {
        let quote_tokens = match self.quote_tokens {
            Some(tokens) => tokens,
            None => default_quote_tokens_for_chain(&self.chain)?,
        };

        MetricClient::new(
            self.chain,
            self.tokens,
            self.tvl,
            quote_tokens,
            self.base_url,
            self.secret_key,
            self.poll_time,
            self.quote_timeout,
        )
    }
}
