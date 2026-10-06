//! Book feeds: a provider's complete set of priced pairs, republished whenever it changes.

use std::{collections::HashMap, fmt, sync::Arc};

use chrono::{DateTime, Utc};
use tracing::debug;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    Bytes,
};

use crate::{
    protocol::models::ProtocolComponent,
    snapshot_feed::{
        errors::FeedError, SnapshotFeedEvent, SnapshotFeedOutcome, SnapshotFeedStream,
        SnapshotFeedStreams, SnapshotFeedWatch,
    },
};

pub(crate) mod component;
pub(crate) mod levels;
pub mod quote_tokens;
pub(crate) mod sim;
pub(crate) mod tvl;

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
    /// Metric). `None` for those that do not (Hashflow, Native). The provider's clock, not this
    /// machine's; the snapshot's [`ReceivedAt`] anchor is the feed's own.
    pub updated_at: Option<DateTime<Utc>>,
}

/// One provider's complete set of books at one instant, so a pair absent from a snapshot is one
/// the provider has stopped serving. The books are `Arc`'d so cloning the snapshot (e.g. out of a
/// watch channel) never copies states.
///
/// This is the [`SnapshotFeed::Snapshot`](crate::snapshot_feed::SnapshotFeed) payload of every
/// feed in this module, wrapped in an `Option` whose `None` means there is no servable snapshot:
/// none received yet, or the last one withdrawn as stale (see `max_snapshot_age` on both feed
/// configs).
///
/// `A` is what the snapshot is anchored to. Every feed in this module publishes
/// `BookSnapshot<ReceivedAt>`; a feed whose snapshots belong to a block rather than to a moment
/// publishes a block anchor instead, and the anchor type tells its consumers which.
#[derive(Clone, Debug)]
pub struct BookSnapshot<A> {
    pub anchor: A,
    pub books: Arc<HashMap<String, Book>>,
}

impl BookSnapshot<ReceivedAt> {
    /// `books` as the complete set a feed received just now.
    pub fn received_now(books: HashMap<String, Book>) -> Self {
        BookSnapshot { anchor: ReceivedAt(Utc::now()), books: Arc::new(books) }
    }
}

/// When the feed received a snapshot, on the feed's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceivedAt(pub DateTime<Utc>);

/// What every book feed needs, whatever transport it runs on: the chain, the tokens it may serve
/// pairs of, and the liquidity floor below which a book is not worth publishing. Every feed
/// builder takes it by value as its first argument and stores it whole; build a second config to
/// give one provider a different universe or floor. The token map is reference-counted, so
/// cloning the config for several feeds shares one map.
///
/// The transport's own tuning is separate:
/// [`snapshot_feed::ws::WsFeedConfig`](crate::snapshot_feed::ws::WsFeedConfig)
/// and [`snapshot_feed::http::HttpFeedConfig`](crate::snapshot_feed::http::HttpFeedConfig).
#[derive(Clone, derive_more::Debug)]
pub struct BookFeedConfig {
    pub chain: Chain,
    /// The universe of tradable tokens with their metadata; pairs involving tokens missing here
    /// are not served.
    #[debug("{} tokens", tokens.len())]
    pub tokens: Arc<HashMap<Bytes, Token>>,
    /// Minimum book TVL in USD; 100 USD is a sensible floor for the supported providers.
    pub min_tvl_usd: f64,
}

impl BookFeedConfig {
    /// The two tokens of a pair, or `None` when either is outside the universe and the pair is
    /// therefore not served.
    pub(crate) fn pair_tokens(&self, a: &Bytes, b: &Bytes) -> Option<(&Token, &Token)> {
        Some((self.tokens.get(a)?, self.tokens.get(b)?))
    }

    /// Whether a book with `tvl_usd` clears the floor; logs the book it filters out.
    pub(crate) fn clears_min_tvl(&self, tvl_usd: f64, book: impl fmt::Display) -> bool {
        let clears = tvl_usd >= self.min_tvl_usd;
        if !clears {
            debug!(
                book = %book,
                tvl_usd,
                min_tvl_usd = self.min_tvl_usd,
                "filtering out book below the TVL floor"
            );
        }
        clears
    }
}

/// Several book feeds, merged — [`SnapshotFeedStreams`] with the types every book feed uses.
///
/// [`add`](SnapshotFeedStreams::add) each feed under the label you want its events to carry —
/// usually as each one is built, since a provider without credentials or without support for the
/// chain simply never joins the set — then read [`BookFeedEvent`]s until the set runs dry:
///
/// ```
/// # use futures::StreamExt as _;
/// # use tycho_simulation::book::{BookFeedEvent, BookFeedStreams};
/// # use tycho_simulation::snapshot_feed::SnapshotFeedOutcome;
/// # async fn consume() {
/// // Anchored like the snapshots it carries; `add` infers this from the feed.
/// let mut feeds: BookFeedStreams = BookFeedStreams::new();
///
/// while let Some((provider, event)) = feeds.next().await {
///     match event {
///         BookFeedEvent::Published(snapshot) => { /* quote from `snapshot.books` */ }
///         BookFeedEvent::Withdrawn => { /* stop quoting `provider` until it publishes again */ }
///         // `provider` is out of the set now, whichever of the three ended it.
///         BookFeedEvent::Ended(SnapshotFeedOutcome::Failed(error)) => { /* it gave up */ }
///         BookFeedEvent::Ended(outcome) => { /* ran out, or died of a bug */ }
///     }
/// }
/// # }
/// ```
pub type BookFeedStreams<A = ReceivedAt> = SnapshotFeedStreams<BookSnapshot<A>, FeedError>;

/// One book feed, read as a stream of [`BookFeedEvent`]s. See [`SnapshotFeedStream`].
pub type BookFeedStream<A = ReceivedAt> = SnapshotFeedStream<BookSnapshot<A>, FeedError>;

/// What happened on a book feed. See [`BookFeedStreams`].
pub type BookFeedEvent<A = ReceivedAt> = SnapshotFeedEvent<BookSnapshot<A>, FeedError>;

/// One book feed, held as the newest book set a consumer can read without awaiting — for one
/// that prices on demand rather than reacting to every update, with
/// [`ended`](SnapshotFeedWatch::ended) for why a venue stopped. See [`SnapshotFeedWatch`].
pub type BookFeedWatch<A = ReceivedAt> = SnapshotFeedWatch<BookSnapshot<A>, FeedError>;

/// How a book feed ended. See [`SnapshotFeedWatch::ended`].
pub type BookFeedOutcome = SnapshotFeedOutcome<FeedError>;

/// Builds the token map of a test [`BookFeedConfig`] from `(address, symbol, decimals)` entries.
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
