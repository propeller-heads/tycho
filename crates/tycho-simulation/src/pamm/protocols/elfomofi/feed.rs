//! ElFomoFi's book feed and its builder.

use std::{future::Future, sync::atomic::AtomicU64, time::Duration};

use crate::{
    book::{quote_tokens::usd_stablecoins_for_chain, BookFeedConfig, BookSnapshot, ReceivedAt},
    pamm::protocols::elfomofi::{
        client::ElfomofiClient, models::ElfomofiSupportedChain, source::ElfomofiBookSource,
    },
    snapshot_feed::{
        errors::FeedError,
        http::{default_http_feed_config, run_http_poll_feed, HttpFeedConfig},
        Publisher, SnapshotFeed,
    },
};

/// ElFomoFi's book feed: polls the chain's helper over JSON-RPC and publishes every poll as a
/// complete [`BookSnapshot`]. Built through [`ElfomofiFeedBuilder`]. `Debug` output omits the
/// RPC URL.
#[derive(Debug)]
pub struct ElfomofiFeed {
    pub(super) feed_config: HttpFeedConfig,
    pub(super) source: ElfomofiBookSource,
}

/// Builds an [`ElfomofiFeed`] from the shared [`BookFeedConfig`] and an HTTP JSON-RPC endpoint of
/// the configured chain.
pub struct ElfomofiFeedBuilder {
    book_config: BookFeedConfig,
    rpc_url: String,
    feed_config: Option<HttpFeedConfig>,
}

impl ElfomofiFeedBuilder {
    /// The feed runs on [`Self::default_feed_config`] for the configured chain unless
    /// [`Self::feed_config`] says otherwise.
    pub fn new(book_config: BookFeedConfig, rpc_url: String) -> Self {
        Self { book_config, rpc_url, feed_config: None }
    }

    /// The feed tuning a new builder runs on for `chain`: a poll every second, given two seconds
    /// to answer, and books withdrawn once they go [`ElfomofiSupportedChain::max_snapshot_age`]
    /// without a refresh. Each poll is one `eth_call`; a quote holds only a few seconds after
    /// the operator's last push, so the cadence is as fast as one call allows and the
    /// withdrawal age is the chain's bound, also the most [`Self::build`] accepts.
    ///
    /// Spread from it to change one field:
    ///
    /// ```
    /// # use std::time::Duration;
    /// # use tycho_simulation::pamm::protocols::elfomofi::{
    /// #     feed::ElfomofiFeedBuilder, ElfomofiSupportedChain,
    /// # };
    /// # use tycho_simulation::snapshot_feed::http::HttpFeedConfig;
    /// let tuned = HttpFeedConfig {
    ///     poll_interval: Duration::from_millis(500),
    ///     ..ElfomofiFeedBuilder::default_feed_config(ElfomofiSupportedChain::Base)
    /// };
    /// ```
    pub fn default_feed_config(chain: ElfomofiSupportedChain) -> HttpFeedConfig {
        HttpFeedConfig {
            poll_interval: Duration::from_secs(1),
            request_timeout: Duration::from_secs(2),
            max_snapshot_age: Some(chain.max_snapshot_age()),
            ..default_http_feed_config()
        }
    }

    /// Tunes the poll loop. [`Self::build`] refuses a `max_snapshot_age` that is unset or longer
    /// than the chain's bound.
    pub fn feed_config(mut self, feed_config: HttpFeedConfig) -> Self {
        self.feed_config = Some(feed_config);
        self
    }

    /// Fails with [`FeedError::InvalidInput`] for a chain ElFomoFi is not deployed on, an RPC URL
    /// that is not `http` or `https`, a zero poll interval or request timeout, a withdrawal age
    /// that is unset, zero or longer than the chain's bound, or a TVL floor that is negative or
    /// not finite.
    pub fn build(self) -> Result<ElfomofiFeed, FeedError> {
        let chain = ElfomofiSupportedChain::try_from(self.book_config.chain)?;
        let feed_config = self
            .feed_config
            .unwrap_or_else(|| Self::default_feed_config(chain));
        check_feed_config(&feed_config, chain)?;
        let min_tvl_usd = self.book_config.min_tvl_usd;
        if !min_tvl_usd.is_finite() || min_tvl_usd < 0.0 {
            return Err(FeedError::InvalidInput(format!(
                "ElFomoFi min_tvl_usd must be finite and not negative, got {min_tvl_usd}"
            )));
        }
        let client = ElfomofiClient::new(&self.rpc_url, chain)?;
        let usd_quote_tokens = usd_stablecoins_for_chain(chain.chain()).unwrap_or_default();
        Ok(ElfomofiFeed {
            feed_config,
            source: ElfomofiBookSource {
                client,
                chain,
                book_config: self.book_config,
                usd_quote_tokens,
                max_read_lag: chain.max_read_lag(),
                highest_block: AtomicU64::new(0),
            },
        })
    }
}

fn check_feed_config(
    config: &HttpFeedConfig,
    chain: ElfomofiSupportedChain,
) -> Result<(), FeedError> {
    let invalid = |reason: String| FeedError::InvalidInput(format!("ElFomoFi feed: {reason}"));
    if config.poll_interval.is_zero() {
        return Err(invalid("poll_interval must not be zero".to_string()));
    }
    if config.request_timeout.is_zero() {
        return Err(invalid("request_timeout must not be zero".to_string()));
    }
    let bound = chain.max_snapshot_age();
    match config.max_snapshot_age {
        Some(age) if !age.is_zero() && age <= bound => Ok(()),
        other => Err(invalid(format!(
            "max_snapshot_age must be set, not zero and at most {bound:?} on {}, got {other:?}",
            chain.chain()
        ))),
    }
}

impl SnapshotFeed for ElfomofiFeed {
    type Snapshot = BookSnapshot<ReceivedAt>;
    type Error = FeedError;

    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), FeedError>> + Send + 'static {
        run_http_poll_feed(self.feed_config, publisher, self.source)
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use rstest::rstest;
    use tokio::time::timeout;
    use tycho_common::models::Chain;

    use super::*;
    use crate::{
        pamm::protocols::elfomofi::test_support::{
            base_book_config, helper_answer, now_secs, weth_usdc_wire, FakeNode,
        },
        snapshot_feed::{SnapshotFeedEvent, SnapshotFeedStream},
    };

    const URL: &str = "https://rpc.example";

    fn builder() -> ElfomofiFeedBuilder {
        ElfomofiFeedBuilder::new(base_book_config(0.0), URL.to_string())
    }

    fn tuned(change: impl FnOnce(&mut HttpFeedConfig)) -> HttpFeedConfig {
        let mut config = ElfomofiFeedBuilder::default_feed_config(ElfomofiSupportedChain::Base);
        change(&mut config);
        config
    }

    #[test]
    fn a_new_builder_runs_on_the_default_tuning_of_its_chain() {
        let feed = builder().build().unwrap();
        let expected = ElfomofiFeedBuilder::default_feed_config(ElfomofiSupportedChain::Base);

        assert_eq!(feed.feed_config.poll_interval, expected.poll_interval);
        assert_eq!(feed.feed_config.request_timeout, expected.request_timeout);
        assert_eq!(feed.feed_config.max_snapshot_age, expected.max_snapshot_age);
        assert_eq!(feed.feed_config.max_backoff_exp, expected.max_backoff_exp);
        assert_eq!(
            feed.feed_config
                .max_consecutive_failures,
            expected.max_consecutive_failures
        );
        assert_eq!(expected.poll_interval, Duration::from_secs(1));
        assert_eq!(expected.request_timeout, Duration::from_secs(2));
    }

    #[rstest]
    #[case::zero_poll_interval(tuned(|c| c.poll_interval = Duration::ZERO))]
    #[case::zero_request_timeout(tuned(|c| c.request_timeout = Duration::ZERO))]
    #[case::zero_age(tuned(|c| c.max_snapshot_age = Some(Duration::ZERO)))]
    #[case::no_age(tuned(|c| c.max_snapshot_age = None))]
    #[case::age_past_the_chains_bound(tuned(|c| c.max_snapshot_age = Some(Duration::from_secs(60))))]
    fn tuning_the_feed_cannot_run_on_is_refused(#[case] config: HttpFeedConfig) {
        let error = builder()
            .feed_config(config)
            .build()
            .unwrap_err();

        assert!(matches!(error, FeedError::InvalidInput(_)), "{error}");
    }

    #[rstest]
    #[case::negative(-1.0)]
    #[case::not_a_number(f64::NAN)]
    #[case::infinite(f64::INFINITY)]
    fn a_tvl_floor_that_is_not_a_number_of_dollars_is_refused(#[case] min_tvl_usd: f64) {
        let error = ElfomofiFeedBuilder::new(base_book_config(min_tvl_usd), URL.to_string())
            .build()
            .unwrap_err();

        assert!(matches!(error, FeedError::InvalidInput(_)), "{error}");
    }

    #[rstest]
    #[case::not_a_url("rpc.example")]
    #[case::websocket("wss://rpc.example")]
    fn an_rpc_url_that_is_not_http_is_refused(#[case] url: &str) {
        let error = ElfomofiFeedBuilder::new(base_book_config(0.0), url.to_string())
            .build()
            .unwrap_err();

        assert!(matches!(error, FeedError::InvalidInput(_)), "{error}");
        assert!(
            !error
                .to_string()
                .contains("rpc.example"),
            "{error}"
        );
    }

    #[test]
    fn a_chain_without_a_deployment_is_refused() {
        let mut config = base_book_config(0.0);
        config.chain = Chain::Ethereum;

        let error = ElfomofiFeedBuilder::new(config, URL.to_string())
            .build()
            .unwrap_err();

        assert!(matches!(error, FeedError::InvalidInput(_)), "{error}");
    }

    #[tokio::test]
    async fn the_feed_publishes_each_poll() {
        let node =
            FakeNode::serving(vec![helper_answer(vec![weth_usdc_wire()], 1, now_secs() + 60)])
                .spawn()
                .await;
        let feed = ElfomofiFeedBuilder::new(base_book_config(0.0), node.server.url())
            .build()
            .unwrap();
        let mut stream = SnapshotFeedStream::spawn("elfomofi", feed);

        let event = timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("the feed publishes within the timeout")
            .expect("the stream is not over");

        let SnapshotFeedEvent::Published(snapshot) = event else {
            panic!("expected the first poll to be published");
        };
        assert_eq!(snapshot.books.len(), 1);
    }
}
