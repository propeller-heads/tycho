use std::time::Duration;

use tycho_common::models::Chain;

use super::{client::Aqua0Client, models::Aqua0Market};
use crate::rfq::errors::RFQError;

pub struct Aqua0ClientBuilder {
    chain: Chain,
    base_url: String,
    market: Aqua0Market,
    poll_time: Duration,
    quote_timeout: Duration,
}

impl Aqua0ClientBuilder {
    pub fn new(chain: Chain, base_url: String, market: Aqua0Market) -> Self {
        Self {
            chain,
            base_url,
            market,
            poll_time: Duration::from_secs(5),
            quote_timeout: Duration::from_secs(5),
        }
    }

    pub fn poll_time(mut self, poll_time: Duration) -> Self {
        self.poll_time = poll_time;
        self
    }

    pub fn quote_timeout(mut self, quote_timeout: Duration) -> Self {
        self.quote_timeout = quote_timeout;
        self
    }

    pub fn build(self) -> Result<Aqua0Client, RFQError> {
        Aqua0Client::new(self.chain, self.base_url, self.market, self.poll_time, self.quote_timeout)
    }
}
