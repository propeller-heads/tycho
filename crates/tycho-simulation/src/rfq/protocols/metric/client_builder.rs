use std::collections::HashSet;

use tokio::time::Duration;
use tycho_common::{models::Chain, Bytes};

use super::client::MetricClient;
use crate::rfq::{constants::get_metric_config, errors::RFQError};

pub struct MetricClientBuilder {
    chain: Chain,
    tokens: HashSet<Bytes>,
    tvl: f64,
    base_url: String,
    api_key: Option<String>,
    poll_time: Duration,
    quote_timeout: Duration,
}

impl MetricClientBuilder {
    pub fn new(chain: Chain) -> Self {
        let config = get_metric_config();
        Self {
            chain,
            tokens: HashSet::new(),
            tvl: 0.0,
            base_url: config.base_url,
            api_key: config.api_key,
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

    pub fn base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    pub fn api_key(mut self, api_key: Option<String>) -> Self {
        self.api_key = api_key;
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
        MetricClient::new(
            self.chain,
            self.tokens,
            self.tvl,
            self.base_url,
            self.api_key,
            self.poll_time,
            self.quote_timeout,
        )
    }
}
