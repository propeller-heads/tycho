use std::collections::{HashMap, HashSet};

use tokio::time::Duration;
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use super::{client::MetricClient, models::MetricOracleUpdatePolicy};
use crate::rfq::{
    constants::get_metric_config, errors::RFQError,
    protocols::utils::default_quote_tokens_for_chain,
};

pub struct MetricClientBuilder {
    chain: Chain,
    tokens: HashSet<Bytes>,
    token_metadata: HashMap<Bytes, Token>,
    tvl: f64,
    quote_tokens: Option<HashSet<Bytes>>,
    base_url: String,
    secret_key: Option<String>,
    poll_time: Duration,
    quote_timeout: Duration,
    oracle_update_policy: MetricOracleUpdatePolicy,
}

impl MetricClientBuilder {
    pub fn new(chain: Chain) -> Self {
        let config = get_metric_config();
        let oracle_update_policy = MetricOracleUpdatePolicy::default_for_chain(chain);
        Self {
            chain,
            tokens: HashSet::new(),
            token_metadata: HashMap::new(),
            tvl: 0.0,
            quote_tokens: None,
            base_url: config.base_url,
            secret_key: config.secret_key,
            poll_time: Duration::from_secs(5),
            quote_timeout: Duration::from_secs(5),
            oracle_update_policy,
        }
    }

    pub fn tokens(mut self, tokens: HashSet<Bytes>) -> Self {
        self.tokens = tokens;
        self
    }

    /// Provide Tycho token metadata for Metric TVL normalization.
    ///
    /// The `tokens` filter above controls which RFQ pairs are emitted. This metadata is broader:
    /// Metric may need token decimals for a one-hop quote-token pool that is not itself emitted.
    pub fn token_metadata(mut self, tokens: HashMap<Bytes, Token>) -> Self {
        self.token_metadata = tokens;
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

    pub fn oracle_update_policy(mut self, policy: MetricOracleUpdatePolicy) -> Self {
        self.oracle_update_policy = policy;
        self
    }

    pub fn build(self) -> Result<MetricClient, RFQError> {
        let quote_tokens = match self.quote_tokens {
            Some(tokens) => tokens,
            None => default_quote_tokens_for_chain(&self.chain)?,
        };

        MetricClient::new_with_token_metadata(
            self.chain,
            self.tokens,
            self.token_metadata,
            self.tvl,
            quote_tokens,
            self.base_url,
            self.secret_key,
            self.poll_time,
            self.quote_timeout,
            self.oracle_update_policy,
        )
    }
}
