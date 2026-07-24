use tokio::time::Duration;
use tycho_common::{models::Chain, Bytes};

use super::client::PropAmmClient;
use crate::rfq::{constants::get_propamm_config, errors::RFQError};

/// `PropAmmClientBuilder` is a builder pattern implementation for creating instances of
/// `PropAmmClient`.
///
/// # Example
/// ```rust
/// use tycho_simulation::rfq::protocols::propamm::client_builder::PropAmmClientBuilder;
/// use tycho_common::{models::Chain, Bytes};
/// use std::str::FromStr;
///
/// let weth = Bytes::from_str("0x4200000000000000000000000000000000000006").unwrap();
/// let usdc = Bytes::from_str("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913").unwrap();
///
/// let client = PropAmmClientBuilder::new(Chain::Base)
///     .add_pair(weth, usdc)
///     .build()
///     .unwrap();
/// ```
pub struct PropAmmClientBuilder {
    chain: Chain,
    pairs: Vec<(Bytes, Bytes)>,
    base_url: String,
    poll_interval: Duration,
    quote_timeout: Duration,
}

impl PropAmmClientBuilder {
    pub fn new(chain: Chain) -> Self {
        let config = get_propamm_config();
        Self {
            chain,
            pairs: Vec::new(),
            base_url: config.base_url,
            poll_interval: Duration::from_millis(1000), // Default 1s polling interval
            quote_timeout: Duration::from_secs(5),      // Default 5 second timeout
        }
    }

    /// Set the directed (token_in, token_out) pairs to poll levels for.
    ///
    /// PropAMM ladders are one-directional; add the reverse pair too if both directions are
    /// needed.
    pub fn pairs(mut self, pairs: Vec<(Bytes, Bytes)>) -> Self {
        self.pairs = pairs;
        self
    }

    /// Add a single directed (token_in, token_out) pair.
    pub fn add_pair(mut self, token_in: Bytes, token_out: Bytes) -> Self {
        self.pairs.push((token_in, token_out));
        self
    }

    /// Override the PropAMM API base url (defaults to the PROPAMM_API_URL env var).
    pub fn base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    /// Set the interval between levels polls
    pub fn poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Set the timeout for firm quote requests
    pub fn quote_timeout(mut self, timeout: Duration) -> Self {
        self.quote_timeout = timeout;
        self
    }

    pub fn build(self) -> Result<PropAmmClient, RFQError> {
        PropAmmClient::new(
            self.chain,
            self.pairs,
            self.base_url,
            self.poll_interval,
            self.quote_timeout,
        )
    }
}
