use std::collections::HashSet;

use tokio::time::Duration;
use tycho_common::{models::Chain, Bytes};

use super::client::NativeClient;
use crate::rfq::{
    constants::get_native_auth, errors::RFQError, protocols::utils::default_quote_tokens_for_chain,
};

pub struct NativeClientBuilder {
    chain: Chain,
    api_key: String,
    tokens: HashSet<Bytes>,
    tvl: f64,
    quote_tokens: Option<HashSet<Bytes>>,
    poll_time: Duration,
    quote_timeout: Duration,
}

impl NativeClientBuilder {
    pub fn new(chain: Chain, api_key: String) -> Self {
        Self {
            chain,
            api_key,
            tokens: HashSet::new(),
            tvl: 100.0,
            quote_tokens: None,
            poll_time: Duration::from_secs(5),
            quote_timeout: Duration::from_secs(5),
        }
    }

    pub fn from_env(chain: Chain) -> Result<Self, RFQError> {
        let auth = get_native_auth()?;
        Ok(Self::new(chain, auth.key))
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

    pub fn quote_timeout(mut self, quote_timeout: Duration) -> Self {
        self.quote_timeout = quote_timeout;
        self
    }

    pub fn build(self) -> Result<NativeClient, RFQError> {
        let quote_tokens = match self.quote_tokens {
            Some(tokens) => tokens,
            None => default_quote_tokens_for_chain(&self.chain)?,
        };

        NativeClient::new(
            self.chain,
            self.api_key,
            self.tokens,
            self.tvl,
            quote_tokens,
            self.poll_time,
            self.quote_timeout,
        )
    }
}
