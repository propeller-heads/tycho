use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use tracing::debug;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    Bytes,
};

use crate::protocol::models::ProtocolComponent;

/// Configuration shared by every book feed on one chain: the trading universe and the liquidity
/// floor. Every feed builder takes it by value as its first argument and stores it whole; build
/// a second config to give one provider a different universe or threshold. The token map is
/// reference-counted, so cloning the config for several feeds shares one map.
#[derive(Clone, derive_more::Debug)]
pub struct CommonConfig {
    pub chain: Chain,
    /// The universe of tradable tokens with their metadata; pairs involving tokens missing here
    /// are not served.
    #[debug("{} tokens", tokens.len())]
    pub tokens: Arc<HashMap<Bytes, Token>>,
    /// Minimum book TVL in USD; 100 USD is a sensible floor for the supported providers.
    pub min_tvl_usd: f64,
}

impl CommonConfig {
    /// The two tokens of a pair, or `None` when either is outside the universe and the pair is
    /// therefore not served.
    pub(crate) fn pair_tokens(&self, a: &Bytes, b: &Bytes) -> Option<(&Token, &Token)> {
        Some((self.tokens.get(a)?, self.tokens.get(b)?))
    }

    /// Whether a book with `tvl_usd` clears the floor; logs the book it filters out.
    pub(crate) fn clears_min_tvl(&self, tvl_usd: f64, book: &str) -> bool {
        let clears = tvl_usd >= self.min_tvl_usd;
        if !clears {
            debug!(
                book,
                tvl_usd,
                min_tvl_usd = self.min_tvl_usd,
                "filtering out book below the TVL floor"
            );
        }
        clears
    }
}

/// Builds the token map of a test [`CommonConfig`] from `(address, symbol, decimals)` entries.
#[cfg(test)]
pub(crate) fn test_token_map(entries: &[(&Bytes, &str, u32)]) -> HashMap<Bytes, Token> {
    entries
        .iter()
        .map(|(address, symbol, decimals)| {
            (
                (*address).clone(),
                Token::new(address, symbol, *decimals, 0, &[Some(10_000)], Chain::Ethereum, 100),
            )
        })
        .collect()
}

/// One pair's book: the component that identifies the pair and the ready-to-simulate state
/// holding the provider's price levels for it.
///
/// The state is shared, so keeping one past the snapshot it came from costs a reference count;
/// cloning a whole book copies the component. A consumer whose own store holds states by value
/// calls [`ProtocolSim::clone_box`] on the entry it wants.
#[derive(Clone, Debug)]
pub struct Book {
    pub component: ProtocolComponent,
    pub state: Arc<dyn ProtocolSim>,
    /// When the provider last updated this book, for providers that report it (Bebop, Liquorice,
    /// Metric). `None` for providers that do not (Hashflow). The provider's clock, not this
    /// machine's; the snapshot's [`ReceivedAt`] anchor is the feed's own.
    pub updated_at: Option<DateTime<Utc>>,
}

/// One provider's complete set of books at one instant. Never a delta — a pair missing from the
/// snapshot no longer exists. The books are `Arc`'d so cloning the snapshot (e.g. out of a watch
/// channel) never copies states.
///
/// This is the [`SnapshotFeed::Snapshot`](crate::snapshot_feed::SnapshotFeed) payload of every
/// feed in this module, wrapped in an `Option` whose `None` means there is no servable snapshot:
/// none received yet, or the last one withdrawn as stale (see `max_book_age` on `WsFeedConfig`
/// and `max_missed_polls` on `HttpFeedConfig`).
///
/// `A` is what the snapshot is anchored to. Every feed in this module publishes
/// `BookSnapshot<ReceivedAt>`; a feed whose snapshots belong to a block rather than to a moment
/// publishes a block anchor instead, and the anchor type tells its consumers which.
#[derive(Clone, Debug)]
pub struct BookSnapshot<A> {
    pub anchor: A,
    pub books: Arc<HashMap<String, Book>>,
}

/// When the feed received a snapshot, on the feed's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceivedAt(pub DateTime<Utc>);

/// Feed tuning for WebSocket-streaming book feeds. Set through the feed builder.
///
/// There is no `Default`: start from the builder's `default_feed_config()`, which holds the
/// values that fit that venue, and change individual fields with struct-update syntax.
#[derive(Clone, Debug)]
pub struct WsFeedConfig {
    /// Deadline for the WebSocket handshake.
    pub connect_timeout: Duration,
    /// Reconnect when no frame arrives within this window.
    pub read_idle_timeout: Duration,
    /// Base unit of the reconnect backoff: after `n` consecutive failures the feed waits
    /// `backoff_unit * 2^min(n, max_backoff_exp)`.
    pub backoff_unit: Duration,
    /// Cap on the backoff exponent.
    pub max_backoff_exp: u32,
    /// Consecutive failures (connections without decoded pricing data) before the feed gives
    /// up and resolves with an error. `None` retries forever.
    pub max_consecutive_failures: Option<u32>,
    /// Withdraw the published snapshot (publish `None`) when no new one has arrived for this long,
    /// so consumers stop quoting a book nobody is updating; the next decoded one restores it.
    /// Measured from the last publish on the feed's clock, so it also covers reconnect waits.
    /// Must not be zero: the feed future resolves with `InvalidInput` right away. `None` keeps
    /// the last snapshot until the feed ends.
    pub max_book_age: Option<Duration>,
}

/// The values every WebSocket feed builder starts from: 10 s handshake deadline, 60 s idle
/// window, 1 s backoff unit capped at exponent 5 (32 s), retry forever, and withdrawal after
/// 60 s without a book, which is the idle window — the moment the loop declares the socket dead
/// it also stops serving the book.
pub(crate) fn default_ws_feed_config() -> WsFeedConfig {
    WsFeedConfig {
        connect_timeout: Duration::from_secs(10),
        read_idle_timeout: Duration::from_secs(60),
        backoff_unit: Duration::from_secs(1),
        max_backoff_exp: 5,
        max_consecutive_failures: None,
        max_book_age: Some(Duration::from_secs(60)),
    }
}

/// Feed tuning for HTTP-polling book feeds. Set through the feed builder.
///
/// There is no `Default`: start from the builder's `default_feed_config()`, which holds the
/// values that fit that venue, and change individual fields with struct-update syntax.
#[derive(Clone, Debug)]
pub struct HttpFeedConfig {
    /// Minimum time between the starts of two polls; a poll that runs longer pushes the next one
    /// out by a full interval. Must not be zero: the feed future resolves with `InvalidInput`
    /// right away.
    pub poll_interval: Duration,
    /// Deadline for one poll's requests; a hung poll counts as a failed poll.
    pub request_timeout: Duration,
    /// Consecutive failed polls before the feed gives up and resolves with an error.
    /// `None` retries forever.
    pub max_consecutive_failures: Option<u32>,
    /// Withdraw the published snapshot (publish `None`) once this many polls in a row have
    /// failed, so consumers stop quoting a book nobody is refreshing; the next successful poll
    /// restores it. Counted in polls rather than seconds because a polling feed publishes on
    /// every success, so the count already scales with `poll_interval`. `None` keeps the last
    /// snapshot until the feed ends.
    pub max_missed_polls: Option<u32>,
}

/// The values every HTTP feed builder starts from: poll every 5 s with a 10 s deadline per
/// poll, retry forever, and withdraw the book after 2 failed polls in a row.
pub(crate) fn default_http_feed_config() -> HttpFeedConfig {
    HttpFeedConfig {
        poll_interval: Duration::from_secs(5),
        request_timeout: Duration::from_secs(10),
        max_consecutive_failures: None,
        max_missed_polls: Some(2),
    }
}
