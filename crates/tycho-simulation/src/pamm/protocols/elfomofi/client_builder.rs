use std::{collections::HashSet, time::Duration};

use tycho_common::{models::Chain, Bytes};

use super::{client::ElfomofiClient, rpc::regular_rpc_url};
use crate::snapshot_feed::errors::FeedError;

/// Internal configuration of RPC reads with new-head updates.
pub struct ElfomofiClientBuilder {
    chain: Chain,
    rpc_url: Option<String>,
    ws_rpc_url: Option<String>,
    updates: Updates,
    tokens: HashSet<Bytes>,
    poll_time: Duration,
    quote_timeout: Duration,
    max_age: Duration,
}

enum Updates {
    NewHeads,
    Polling,
}

impl ElfomofiClientBuilder {
    pub fn new(chain: Chain) -> Self {
        Self {
            chain,
            rpc_url: None,
            ws_rpc_url: None,
            updates: Updates::NewHeads,
            tokens: HashSet::new(),
            poll_time: Duration::from_secs(2),
            quote_timeout: Duration::from_secs(5),
            max_age: Duration::from_secs(10),
        }
    }

    /// Uses the regular `RPC_URL` / `WS_RPC_URL` configuration, with `.env` fallback.
    pub fn from_env(chain: Chain) -> Self {
        let mut builder = Self::new(chain);
        builder.rpc_url = regular_rpc_url("RPC_URL").ok();
        builder.ws_rpc_url = regular_rpc_url("WS_RPC_URL").ok();
        builder
    }

    #[cfg(test)]
    pub fn rpc_url(mut self, rpc_url: impl Into<String>) -> Self {
        self.rpc_url = Some(rpc_url.into());
        self
    }

    /// Uses a WebSocket `eth_subscribe("newHeads")` feed to trigger complete book refreshes.
    /// Calling this after `poll_time` switches updates back to subscriptions.
    #[cfg(test)]
    pub fn ws_rpc_url(mut self, ws_rpc_url: impl Into<String>) -> Self {
        self.ws_rpc_url = Some(ws_rpc_url.into());
        self.updates = Updates::NewHeads;
        self
    }

    /// An empty set includes all pairs. Otherwise both pair tokens must be present.
    pub fn tokens(mut self, tokens: HashSet<Bytes>) -> Self {
        self.tokens = tokens;
        self
    }

    /// Explicitly selects HTTP head polling for providers without WebSocket subscriptions.
    /// Calling this after `ws_rpc_url` switches updates to polling.
    pub fn poll_time(mut self, poll_time: Duration) -> Self {
        self.poll_time = poll_time;
        self.updates = Updates::Polling;
        self
    }

    /// Bounds the complete RPC operation, including receiving and decoding the response body.
    pub fn quote_timeout(mut self, quote_timeout: Duration) -> Self {
        self.quote_timeout = quote_timeout;
        self
    }

    /// Sets the maximum age of both the block and the observation used for indicative quotes.
    pub fn max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    pub fn build(self) -> Result<ElfomofiClient, FeedError> {
        let rpc_url = self.rpc_url.ok_or_else(|| {
            FeedError::InvalidInput("Set RPC_URL in the environment or .env file".into())
        })?;
        let client = ElfomofiClient::new(
            self.chain,
            rpc_url,
            self.tokens,
            self.poll_time,
            self.quote_timeout,
            self.max_age,
        )?;
        match self.updates {
            Updates::NewHeads => {
                let ws_rpc_url = self.ws_rpc_url.ok_or_else(|| {
                    FeedError::InvalidInput(
                        "Set WS_RPC_URL for newHeads updates; \
                     select poll_time explicitly for HTTP polling"
                            .into(),
                    )
                })?;
                client.with_ws_rpc_url(ws_rpc_url)
            }
            Updates::Polling => Ok(client),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder() -> ElfomofiClientBuilder {
        ElfomofiClientBuilder::new(Chain::Base).rpc_url("https://base.example.test/private-key")
    }

    #[test]
    fn requires_websocket_for_default_head_updates() {
        let error = builder().build().unwrap_err();
        assert!(error.to_string().contains("WS_RPC_URL"));
        assert!(!error
            .to_string()
            .contains("private-key"));
    }

    #[test]
    fn update_setters_select_subscriptions_or_explicit_polling() {
        assert!(builder()
            .poll_time(Duration::from_secs(2))
            .build()
            .is_ok());
        assert!(builder()
            .ws_rpc_url("https://invalid-websocket.example.test")
            .poll_time(Duration::from_secs(2))
            .build()
            .is_ok());
        assert!(builder()
            .poll_time(Duration::from_secs(2))
            .ws_rpc_url("https://invalid-websocket.example.test")
            .build()
            .is_err());

        let client = builder()
            .ws_rpc_url("wss://base.example.test/websocket-key")
            .build()
            .unwrap();
        let debug = format!("{client:?}");
        assert!(debug.contains("ws_rpc_url: Some(\"[redacted]\")"));
        assert!(!debug.contains("private-key"));
        assert!(!debug.contains("websocket-key"));
    }
}
