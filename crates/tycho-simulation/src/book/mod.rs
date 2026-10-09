//! Book feeds: a provider's complete set of priced pairs, republished whenever it changes.

use std::{
    collections::{hash_map::Entry, HashMap},
    fmt,
    sync::Arc,
};

use chrono::{DateTime, Utc};
use tracing::{debug, warn};
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
pub(crate) mod wire;

/// One pair's book: the component that identifies the pair and the ready-to-simulate state
/// holding the provider's price levels for it.
///
/// The state is shared, so keeping one past the snapshot it came from costs a reference count;
/// cloning a whole book copies the component.
#[derive(Clone, Debug)]
pub struct Book {
    pub component: ProtocolComponent,
    pub state: Arc<dyn ProtocolSim>,
    /// When the provider last updated this book, on the provider's clock; `None` when the
    /// provider reports none. The snapshot's [`ReceivedAt`] anchor is the feed's own.
    pub updated_at: Option<DateTime<Utc>>,
}

/// The books a source has decoded out of one answer from its venue, each under the id of the
/// component it carries.
#[derive(Debug, Default, derive_more::Deref)]
pub(crate) struct Books(HashMap<String, Book>);

impl Books {
    pub fn new() -> Self {
        Books::default()
    }

    /// Keeps `book` under its own component's id.
    ///
    /// Two books claiming one component leaves the one added last, and says so: they price the
    /// same pair differently, and which of them a consumer quotes is the order they arrived in.
    pub fn insert(&mut self, book: Book) {
        match self
            .0
            .entry(book.component.id.to_string())
        {
            Entry::Occupied(mut taken) => {
                warn!(
                    component = %taken.key(),
                    "two books price one component, keeping the last"
                );
                taken.insert(book);
            }
            Entry::Vacant(free) => {
                free.insert(book);
            }
        }
    }
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
    pub(crate) fn received_now(books: Books) -> Self {
        BookSnapshot { anchor: ReceivedAt(Utc::now()), books: Arc::new(books.0) }
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
    /// The numeric chain id a venue's API addresses this chain by, or a [`FeedError::Fatal`]
    /// naming the chain that has none.
    pub(crate) fn chain_id(&self) -> Result<u64, FeedError> {
        self.chain
            .try_id()
            .map_err(|error| FeedError::Fatal(format!("no chain id for {}: {error}", self.chain)))
    }

    /// The two tokens of a pair, or `None` when either is outside the universe and the pair is
    /// therefore not served.
    pub(crate) fn pair_tokens(&self, a: &Bytes, b: &Bytes) -> Option<(&Token, &Token)> {
        Some((self.tokens.get(a)?, self.tokens.get(b)?))
    }

    /// Whether a book with `tvl_usd` clears the floor; names `book_label` in the log line when
    /// it does not.
    pub(crate) fn clears_min_tvl(&self, tvl_usd: f64, book_label: impl fmt::Display) -> bool {
        let clears = tvl_usd >= self.min_tvl_usd;
        if !clears {
            debug!(
                book = %book_label,
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
/// # use tycho_simulation::book::{BookFeedEvent, BookFeedOutcome, BookFeedStreams};
/// # async fn consume() {
/// // Anchored like the snapshots it carries; `add` infers this from the feed.
/// let mut feeds: BookFeedStreams = BookFeedStreams::new();
///
/// while let Some((provider, event)) = feeds.next().await {
///     match event {
///         BookFeedEvent::Published(snapshot) => { /* quote from `snapshot.books` */ }
///         BookFeedEvent::Withdrawn => { /* stop quoting `provider` until it publishes again */ }
///         // `provider` is out of the set now, whichever of the three ended it.
///         BookFeedEvent::Ended(BookFeedOutcome::Failed(error)) => { /* it gave up */ }
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

#[cfg(test)]
mod tests {
    use tycho_common::models::Chain;

    use super::*;
    use crate::{book::component::pair_component, evm::decoder::MockProtocolSim};

    fn book(base: &Bytes, quote: &Bytes) -> Book {
        let token =
            |address: &Bytes| Token::new(address, "T", 18, 0, &[Some(0)], Chain::Ethereum, 100);
        Book {
            component: pair_component(
                component::pair_component_id("book:venue", base, quote),
                "book:venue",
                "venue_pool",
                Chain::Ethereum,
                token(base),
                token(quote),
            ),
            state: Arc::new(MockProtocolSim::new()),
            updated_at: None,
        }
    }

    /// Two answers for one pair are one book, whichever way round the venue sent them.
    #[test]
    fn a_book_is_kept_under_its_own_component() {
        let weth = Bytes::from(vec![0x11; 20]);
        let usdc = Bytes::from(vec![0x22; 20]);
        let mut books = Books::new();

        books.insert(book(&weth, &usdc));
        books.insert(book(&usdc, &weth));
        books.insert(book(&weth, &usdc));

        assert_eq!(books.len(), 2);
        let ids: Vec<_> = books.keys().cloned().collect();
        assert!(
            ids.contains(&component::pair_component_id("book:venue", &weth, &usdc).to_string()),
            "{ids:?}"
        );
    }
}
