use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::Utc;
use futures::{Stream, StreamExt};
use http::Request;
use tokio::{
    select,
    sync::watch,
    time::{interval, sleep_until, timeout, Instant, MissedTickBehavior},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{self, Bytes, Message, Utf8Bytes},
};
use tracing::{debug, debug_span, error, info, info_span, instrument, warn, Instrument};

use crate::book::{
    errors::FeedError,
    models::{Book, BookSnapshot, HttpFeedConfig, ReceivedAt, WsFeedConfig},
};

/// Consecutive-failure counting with capped exponential backoff, shared by the WebSocket and
/// HTTP feed loops so every feed gives up (or retries forever) the same way.
pub struct FailureTracker {
    consecutive: u32,
    max: Option<u32>,
}

impl FailureTracker {
    pub fn new(max: Option<u32>) -> Self {
        FailureTracker { consecutive: 0, max }
    }

    /// Clears the streak and returns how many consecutive failures it had reached.
    pub fn record_success(&mut self) -> u32 {
        std::mem::take(&mut self.consecutive)
    }

    /// Records one failure and returns true when the configured limit is reached.
    pub fn record_failure(&mut self) -> bool {
        self.consecutive += 1;
        self.max
            .is_some_and(|max| self.consecutive >= max)
    }

    pub fn consecutive(&self) -> u32 {
        self.consecutive
    }

    pub fn backoff(&self, unit: Duration, max_exp: u32) -> Duration {
        unit * 2_u32.pow(self.consecutive.min(max_exp))
    }
}

/// The feed's side of the watch channel. Publishing a snapshot while nothing servable is
/// published (at start, or after a withdrawal) is the event a consumer cares about and is logged
/// at `info`; every further snapshot is a `debug` heartbeat.
struct Publisher {
    tx: watch::Sender<Option<BookSnapshot<ReceivedAt>>>,
    /// Whether the watch currently holds a snapshot.
    servable: bool,
}

impl Publisher {
    fn new(tx: watch::Sender<Option<BookSnapshot<ReceivedAt>>>) -> Self {
        Publisher { tx, servable: false }
    }

    /// Publishes a complete snapshot; returns false when every receiver is gone.
    fn publish(&mut self, books: HashMap<String, Book>) -> bool {
        let count = books.len();
        let snapshot = BookSnapshot { anchor: ReceivedAt(Utc::now()), books: Arc::new(books) };
        if self.tx.send(Some(snapshot)).is_err() {
            return false;
        }
        if self.servable {
            debug!(books = count, "published book snapshot");
        } else {
            info!(books = count, "book published");
            self.servable = true;
        }
        true
    }

    /// Withdraws the published snapshot (publishes `None`) so consumers stop quoting a book nobody
    /// is refreshing. Returns whether there was one to withdraw; an already-empty watch stays
    /// quiet.
    fn withdraw(&mut self) -> bool {
        self.servable = false;
        self.tx
            .send_if_modified(|snapshot| snapshot.take().is_some())
    }

    fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Resolves once every receiver is dropped.
    async fn closed(&self) {
        self.tx.closed().await
    }
}

impl Drop for Publisher {
    /// Whatever ends a feed ends its book: the loop gave up, or the caller dropped the feed
    /// future. Nothing refreshes the published snapshot after that, so it is withdrawn rather
    /// than left standing for consumers to keep quoting. Reaching a receiver that outlives the
    /// feed is the point; on the loop's clean exit every receiver is already gone and this is a
    /// no-op.
    fn drop(&mut self) {
        self.withdraw();
    }
}

/// Withdraws the published snapshot once it is older than `max_book_age`, for the WebSocket loop,
/// where silence carries no failure signal of its own. Age is measured on this machine's
/// monotonic clock from the last publish, never from the snapshot's own timestamps.
struct StaleGuard {
    max_age: Option<Duration>,
    /// When a snapshot was last published; `None` while nothing servable is published.
    published_at: Option<Instant>,
}

impl StaleGuard {
    fn new(max_age: Option<Duration>) -> Self {
        StaleGuard { max_age, published_at: None }
    }

    fn record_publish(&mut self) {
        self.published_at = Some(Instant::now());
    }

    /// Resolves when the published snapshot becomes stale; pending forever while nothing is
    /// published or no maximum age is configured.
    async fn stale(&self) {
        match (self.max_age, self.published_at) {
            (Some(max_age), Some(published_at)) => sleep_until(published_at + max_age).await,
            _ => std::future::pending().await,
        }
    }

    fn withdraw(&mut self, publisher: &mut Publisher) {
        let max_age = self
            .max_age
            .expect("withdraw is only reachable with a configured max_book_age");
        warn!(
            max_age_secs = max_age.as_secs(),
            "no book received within max_book_age, withdrawing the published one"
        );
        publisher.withdraw();
        self.published_at = None;
    }
}

/// The provider-specific half of an HTTP-polling book feed: one call fetches one complete book.
/// [`run_http_poll_feed`] owns the source and drives it by reference.
pub trait HttpBookSource: Send + Sync {
    async fn fetch_books(&self) -> Result<HashMap<String, Book>, FeedError>;
}

/// A data-bearing WebSocket frame. Control frames never reach a [`WsBookSource`]: the loop
/// reconnects on `Close`, tokio-tungstenite answers pings while the stream is polled, and
/// tungstenite never yields raw frames on the read side.
pub enum WsPayload {
    Text(Utf8Bytes),
    Binary(Bytes),
}

/// The provider-specific half of a WebSocket book feed: it builds the handshake request and
/// decodes data frames into complete books. [`run_ws_feed`] owns the source and drives it by
/// reference.
pub trait WsBookSource: Send {
    fn request(&self) -> Result<Request<()>, FeedError>;

    /// `Ok(None)` for frames that carry no book.
    fn decode(&mut self, payload: WsPayload) -> Result<Option<HashMap<String, Book>>, FeedError>;
}

/// The poll loop shared by all HTTP-based book feeds.
///
/// Fetches a book from `source` every `poll_interval`, publishing each successful complete book.
/// One fetch is one poll: an error or a `request_timeout` overrun counts as one failed poll.
/// After `max_missed_polls` failed polls in a row the published book is withdrawn until a poll
/// succeeds again. Resolves `Ok` when every receiver is dropped, or `Err` — only with
/// `max_consecutive_failures` configured — when the feed gives up, or right away with
/// `InvalidInput` for a zero `poll_interval`.
///
/// Runs inside a `book_feed` span carrying the provider; each fetch runs in a `poll` span.
#[instrument(name = "book_feed", skip(config, tx, source))]
pub async fn run_http_poll_feed(
    provider: &str,
    config: HttpFeedConfig,
    tx: watch::Sender<Option<BookSnapshot<ReceivedAt>>>,
    source: impl HttpBookSource,
) -> Result<(), FeedError> {
    if config.poll_interval.is_zero() {
        return Err(FeedError::InvalidInput(format!("{provider}: poll_interval must not be zero")));
    }
    let mut ticker = interval(config.poll_interval);
    // A poll slower than the interval delays the next one by a full interval instead of
    // triggering back-to-back polls until the schedule catches up.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut failures = FailureTracker::new(config.max_consecutive_failures);
    let mut publisher = Publisher::new(tx);

    info!(poll_interval_secs = config.poll_interval.as_secs(), "starting book polling");
    loop {
        select! {
            biased;
            _ = publisher.closed() => return Ok(()),
            _ = ticker.tick() => {}
        }

        let polled = timeout(config.request_timeout, source.fetch_books())
            .instrument(debug_span!("poll"))
            .await;
        let error = match polled {
            Ok(Ok(books)) => {
                let cleared = failures.record_success();
                if cleared > 0 {
                    info!(after_failures = cleared, "poll succeeded again");
                }
                if !publisher.publish(books) {
                    return Ok(());
                }
                continue;
            }
            Ok(Err(e)) => e,
            Err(_) => FeedError::ConnectionError(format!(
                "poll timed out after {}s",
                config.request_timeout.as_secs()
            )),
        };

        warn!(consecutive = failures.consecutive() + 1, error = %error, "poll failed");
        if failures.record_failure() {
            return Err(FeedError::ConnectionError(format!(
                "{} consecutive failed polls, last error: {error}",
                failures.consecutive()
            )));
        }
        let missed_too_many = config
            .max_missed_polls
            .is_some_and(|max| failures.consecutive() >= max);
        if missed_too_many && publisher.withdraw() {
            warn!(
                failed_polls = failures.consecutive(),
                "failed polls reached max_missed_polls, withdrawing the published book"
            );
        }
    }
}

/// The connection loop shared by WebSocket-based book feeds.
///
/// Connects with the source's request, reads frames, and hands every data frame to the source
/// for decoding; a decoded book is published. A decode error, read error, idle timeout,
/// stream end, or server close triggers a reconnect with capped exponential backoff. Only
/// decoded pricing data counts as success — a connection that never produced a book still
/// counts as one failure. A published book older than `max_book_age` is withdrawn until the next
/// decoded one. Resolves `Ok` when every receiver is dropped, or `Err` — only with
/// `max_consecutive_failures` configured — when the feed gives up, or right away with
/// `InvalidInput` for a zero `max_book_age`.
///
/// Runs inside a `book_feed` span carrying the provider; each connection runs in a
/// `ws_connection` span and each decoded frame in a `ws_frame` span.
#[instrument(name = "book_feed", skip(config, tx, source))]
pub async fn run_ws_feed(
    provider: &str,
    config: WsFeedConfig,
    tx: watch::Sender<Option<BookSnapshot<ReceivedAt>>>,
    mut source: impl WsBookSource,
) -> Result<(), FeedError> {
    if config
        .max_book_age
        .is_some_and(|age| age.is_zero())
    {
        return Err(FeedError::InvalidInput(format!("{provider}: max_book_age must not be zero")));
    }
    let mut failures = FailureTracker::new(config.max_consecutive_failures);
    let mut stale = StaleGuard::new(config.max_book_age);
    let mut publisher = Publisher::new(tx);

    loop {
        if publisher.is_closed() {
            return Ok(());
        }

        let request = source.request()?;
        let attempt = failures.consecutive() + 1;
        let connect_error =
            match timeout(config.connect_timeout, connect_async_with_config(request, None, false))
                .await
            {
                Ok(Ok((mut ws_stream, _))) => {
                    info!("connected");
                    let end = read_connection(
                        &config,
                        &mut publisher,
                        &mut ws_stream,
                        &mut source,
                        &mut failures,
                        &mut stale,
                    )
                    .instrument(info_span!("ws_connection", attempt))
                    .await;
                    match end {
                        ConnectionEnd::ReceiversDropped => return Ok(()),
                        ConnectionEnd::Disconnected => None,
                    }
                }
                Ok(Err(e)) => Some(e.to_string()),
                Err(_) => {
                    Some(format!("connect timed out after {}s", config.connect_timeout.as_secs()))
                }
            };

        if let Some(e) = &connect_error {
            warn!(attempt, error = %e, "connect failed");
        }
        if failures.record_failure() {
            let reason = match connect_error {
                Some(e) => format!(
                    "failed to connect after {} consecutive failures: {e}",
                    failures.consecutive()
                ),
                None => format!(
                    "no pricing data received after {} consecutive failures",
                    failures.consecutive()
                ),
            };
            return Err(FeedError::ConnectionError(reason));
        }

        let backoff = failures.backoff(config.backoff_unit, config.max_backoff_exp);
        info!(?backoff, consecutive = failures.consecutive(), "reconnecting after backoff");
        let reconnect_at = Instant::now() + backoff;
        loop {
            select! {
                biased;
                _ = publisher.closed() => return Ok(()),
                _ = stale.stale() => stale.withdraw(&mut publisher),
                _ = sleep_until(reconnect_at) => break,
            }
        }
    }
}

/// Why [`read_connection`] returned.
enum ConnectionEnd {
    /// Every receiver is gone; the feed is done.
    ReceiversDropped,
    /// The connection is unusable (idle timeout, stream end, read or decode error, server
    /// close); the caller reconnects.
    Disconnected,
}

/// Reads one WebSocket connection, publishing every decoded book, until it becomes unusable or
/// every receiver is gone.
async fn read_connection(
    config: &WsFeedConfig,
    publisher: &mut Publisher,
    ws_stream: &mut (impl Stream<Item = Result<Message, tungstenite::Error>> + Unpin),
    source: &mut impl WsBookSource,
    failures: &mut FailureTracker,
    stale: &mut StaleGuard,
) -> ConnectionEnd {
    loop {
        let received = select! {
            biased;
            _ = publisher.closed() => return ConnectionEnd::ReceiversDropped,
            _ = stale.stale() => {
                stale.withdraw(publisher);
                continue;
            }
            received = timeout(config.read_idle_timeout, ws_stream.next()) => received,
        };
        let message = match received {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(e))) => {
                warn!(error = %e, "read failed, reconnecting");
                return ConnectionEnd::Disconnected;
            }
            Ok(None) => {
                warn!("stream ended, reconnecting");
                return ConnectionEnd::Disconnected;
            }
            Err(_) => {
                warn!(
                    idle_secs = config.read_idle_timeout.as_secs(),
                    "no frame within read_idle_timeout, reconnecting"
                );
                return ConnectionEnd::Disconnected;
            }
        };

        let payload = match message {
            Message::Text(text) => WsPayload::Text(text),
            Message::Binary(data) => WsPayload::Binary(data),
            // Pings are answered by tokio-tungstenite as the stream is polled; pongs need no
            // reaction.
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(Some(frame)) => {
                warn!(close_frame = %frame, "closed by server, reconnecting");
                return ConnectionEnd::Disconnected;
            }
            Message::Close(None) => {
                warn!("closed by server without a close frame, reconnecting");
                return ConnectionEnd::Disconnected;
            }
            // Documented as write-side only; a read yielding one is a tungstenite bug, not a
            // provider problem.
            Message::Frame(frame) => {
                error!(%frame, "ignoring raw frame on the read side");
                continue;
            }
        };

        let decoded = debug_span!("ws_frame").in_scope(|| source.decode(payload));
        match decoded {
            Ok(Some(books)) => {
                // A completed handshake says nothing about whether the connection works, so
                // only pricing data clears the counter.
                let cleared = failures.record_success();
                if cleared > 0 {
                    info!(after_failures = cleared, "pricing data received again");
                }
                if !publisher.publish(books) {
                    return ConnectionEnd::ReceiversDropped;
                }
                stale.record_publish();
            }
            Ok(None) => {}
            Err(e) => {
                warn!(error = %e, "decode failed, reconnecting");
                return ConnectionEnd::Disconnected;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_common::{
        models::{token::Token, Chain as TokenChain},
        Bytes,
    };

    use super::*;
    use crate::{evm::decoder::MockProtocolSim, protocol::models::ProtocolComponent};

    /// One book with a single pair whose state is never simulated, enough for the loops to
    /// publish something.
    fn book() -> HashMap<String, Book> {
        let token = Token::new(
            &Bytes::from(vec![1u8; 20]),
            "T",
            18,
            0,
            &[Some(10_000)],
            TokenChain::Ethereum,
            100,
        );
        let component = ProtocolComponent::new(
            Bytes::from(b"pair".to_vec()),
            "rfq:mock".to_string(),
            "rfq".to_string(),
            TokenChain::Ethereum,
            vec![token.clone(), token.clone()],
            vec![],
            Default::default(),
            Default::default(),
            Default::default(),
        );
        HashMap::from([(
            "pair".to_string(),
            Book { component, state: Arc::new(MockProtocolSim::new()), updated_at: None },
        )])
    }

    #[rstest]
    #[case::limit_reached_at_max(Some(3), 3, true)]
    #[case::below_limit(Some(3), 2, false)]
    fn failure_tracker_limit(
        #[case] max: Option<u32>,
        #[case] failures: u32,
        #[case] expect_reached: bool,
    ) {
        let mut tracker = FailureTracker::new(max);

        let mut reached = false;
        for _ in 0..failures {
            reached = tracker.record_failure();
        }

        assert_eq!(reached, expect_reached);
        assert_eq!(tracker.consecutive(), failures);
    }

    #[rstest]
    #[case::no_failures_no_backoff(0, Duration::from_secs(1))]
    #[case::doubles_per_failure(3, Duration::from_secs(8))]
    #[case::capped_at_max_exp(9, Duration::from_secs(32))]
    fn failure_tracker_backoff(#[case] failures: u32, #[case] expected: Duration) {
        let mut tracker = FailureTracker::new(None);
        for _ in 0..failures {
            tracker.record_failure();
        }

        assert_eq!(tracker.backoff(Duration::from_secs(1), 5), expected);
    }

    mod ws_feed {
        use std::sync::{
            atomic::{AtomicU32, Ordering},
            Mutex,
        };

        use futures::{SinkExt, StreamExt};
        use tokio::net::TcpListener;
        use tokio_tungstenite::{accept_async, tungstenite::client::IntoClientRequest};

        use super::*;
        use crate::book::models::default_ws_feed_config;

        /// A source that is never reached: the config is rejected before any connection.
        struct NoSource;

        impl WsBookSource for NoSource {
            fn request(&self) -> Result<Request<()>, FeedError> {
                unreachable!("the loop must reject the config before building a request")
            }

            fn decode(
                &mut self,
                _payload: WsPayload,
            ) -> Result<Option<HashMap<String, Book>>, FeedError> {
                unreachable!()
            }
        }

        /// A source for a mock server: a binary frame is one complete book unless it is the
        /// [`bad_frame`], which fails to decode; text frames carry nothing.
        struct MockSource {
            url: String,
        }

        impl WsBookSource for MockSource {
            fn request(&self) -> Result<Request<()>, FeedError> {
                self.url
                    .as_str()
                    .into_client_request()
                    .map_err(|e| FeedError::FatalError(e.to_string()))
            }

            fn decode(
                &mut self,
                payload: WsPayload,
            ) -> Result<Option<HashMap<String, Book>>, FeedError> {
                match payload {
                    WsPayload::Binary(data) if data.as_ref() == BAD_FRAME => {
                        Err(FeedError::ParsingError("undecodable frame".to_string()))
                    }
                    WsPayload::Binary(_) => Ok(Some(book())),
                    WsPayload::Text(_) => Ok(None),
                }
            }
        }

        const BAD_FRAME: &[u8] = &[0xff];

        /// A frame the mock source decodes into a book.
        fn book_frame() -> Message {
            Message::Binary(vec![1].into())
        }

        /// A frame the mock source fails to decode.
        fn bad_frame() -> Message {
            Message::Binary(BAD_FRAME.to_vec().into())
        }

        /// Resolves once the mock server has accepted at least `count` connections.
        async fn await_connections(connections: &AtomicU32, count: u32) {
            while connections.load(Ordering::SeqCst) < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        /// Binds a mock server and returns the listener with the URL a source connects to.
        async fn mock_server() -> (TcpListener, String) {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            (listener, url)
        }

        fn config() -> WsFeedConfig {
            WsFeedConfig { backoff_unit: Duration::from_millis(1), ..default_ws_feed_config() }
        }

        /// Waits until the receiver holds a snapshot and returns its books.
        async fn next_books(
            rx: &mut watch::Receiver<Option<BookSnapshot<ReceivedAt>>>,
        ) -> Arc<HashMap<String, Book>> {
            loop {
                rx.changed().await.expect("feed ended");
                if let Some(snapshot) = &*rx.borrow_and_update() {
                    return Arc::clone(&snapshot.books);
                }
            }
        }

        #[tokio::test]
        async fn rejects_zero_max_book_age() {
            let (tx, _rx) = watch::channel(None);
            let config = WsFeedConfig { max_book_age: Some(Duration::ZERO), ..config() };
            let result = run_ws_feed("mock", config, tx, NoSource).await;
            assert!(matches!(result, Err(FeedError::InvalidInput(_))));
        }

        #[tokio::test]
        async fn reconnects_after_the_server_drops_the_connection() {
            // The server sends one book, drops the first connection, then sends a second book
            // on the reconnect and stays up.
            let (listener, url) = mock_server().await;
            let connection_count = Arc::new(Mutex::new(0u32));
            let connection_count_clone = connection_count.clone();

            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    *connection_count_clone.lock().unwrap() += 1;
                    let count = *connection_count_clone.lock().unwrap();

                    tokio::spawn(async move {
                        if let Ok(ws_stream) = accept_async(stream).await {
                            let (mut ws_sender, _ws_receiver) = ws_stream.split();
                            let _ = ws_sender.send(book_frame()).await;
                            if count == 1 {
                                tokio::time::sleep(Duration::from_millis(100)).await;
                                let _ = ws_sender.close().await;
                            } else {
                                tokio::time::sleep(Duration::from_secs(10)).await;
                            }
                        }
                    });
                }
            });

            let (tx, mut rx) = watch::channel(None);
            let _feed = tokio::spawn(run_ws_feed("mock", config(), tx, MockSource { url }));

            // One subscription observes a book, survives the server-side drop, and observes
            // the book sent on the reconnected connection.
            timeout(Duration::from_secs(5), next_books(&mut rx))
                .await
                .expect("no first book");
            timeout(Duration::from_secs(5), next_books(&mut rx))
                .await
                .expect("no book after reconnect");

            assert_eq!(*connection_count.lock().unwrap(), 2);
        }

        #[tokio::test]
        async fn answers_pings_while_the_consumer_stalls() {
            // The socket must be polled (and pings answered) by the feed task itself,
            // independent of how often the consumer reads the watch.
            let (listener, url) = mock_server().await;
            let pong_count = Arc::new(Mutex::new(0u32));
            let pong_count_clone = pong_count.clone();

            tokio::spawn(async move {
                if let Ok((stream, _)) = listener.accept().await {
                    if let Ok(mut ws_stream) = accept_async(stream).await {
                        let _ = ws_stream.send(book_frame()).await;
                        loop {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            if ws_stream
                                .send(Message::Ping(vec![1, 2, 3].into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            // Drain incoming frames without blocking the ping cadence.
                            while let Ok(Some(Ok(message))) =
                                timeout(Duration::from_millis(10), ws_stream.next()).await
                            {
                                if matches!(message, Message::Pong(_)) {
                                    *pong_count_clone.lock().unwrap() += 1;
                                }
                            }
                        }
                    }
                }
            });

            let (tx, rx) = watch::channel(None);
            let _feed = tokio::spawn(run_ws_feed("mock", config(), tx, MockSource { url }));

            // Never read the receiver: a stalled consumer must not stall the socket.
            tokio::time::sleep(Duration::from_millis(1500)).await;
            drop(rx);

            let pongs = *pong_count.lock().unwrap();
            assert!(pongs >= 5, "expected at least 5 pongs while consumer stalled, got {pongs}");
        }

        #[tokio::test]
        async fn withdraws_stale_book_while_connection_is_silent() {
            // The server sends one book, stays connected but silent, and sends a second book
            // only when told to: the published snapshot must be withdrawn after max_book_age
            // and come back with the next frame.
            let (listener, url) = mock_server().await;
            let (resume_tx, resume_rx) = tokio::sync::oneshot::channel::<()>();

            tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let ws_stream = accept_async(stream).await.unwrap();
                let (mut ws_sender, _ws_receiver) = ws_stream.split();
                let _ = ws_sender.send(book_frame()).await;
                let _ = resume_rx.await;
                let _ = ws_sender.send(book_frame()).await;
                tokio::time::sleep(Duration::from_secs(10)).await;
            });

            let (tx, mut rx) = watch::channel(None);
            let config =
                WsFeedConfig { max_book_age: Some(Duration::from_millis(100)), ..config() };
            let _feed = tokio::spawn(run_ws_feed("mock", config, tx, MockSource { url }));

            timeout(Duration::from_secs(5), next_books(&mut rx))
                .await
                .expect("no first book");

            timeout(Duration::from_secs(5), async {
                loop {
                    rx.changed().await.expect("feed ended");
                    if rx.borrow_and_update().is_none() {
                        return;
                    }
                }
            })
            .await
            .expect("stale book was not withdrawn");

            resume_tx.send(()).unwrap();
            timeout(Duration::from_secs(5), next_books(&mut rx))
                .await
                .expect("no book after the stream resumed");
        }

        #[rstest]
        #[case::bookless_connections_count_as_failures(false, true)]
        #[case::a_book_per_connection_keeps_the_feed_alive(true, false)]
        #[tokio::test]
        async fn only_decoded_books_reset_the_failure_streak(
            #[case] server_sends_a_book: bool,
            #[case] expect_give_up: bool,
        ) {
            // Every connection is served the same way and then closed by the server. A
            // connection that delivered a frame without a book must count as a failure even
            // though the handshake and the read succeeded.
            let (listener, url) = mock_server().await;
            let connections = Arc::new(AtomicU32::new(0));
            let connections_clone = Arc::clone(&connections);

            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    connections_clone.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(async move {
                        let Ok(mut ws_stream) = accept_async(stream).await else { return };
                        let frame = if server_sends_a_book {
                            book_frame()
                        } else {
                            Message::Text("notice".into())
                        };
                        let _ = ws_stream.send(frame).await;
                        let _ = ws_stream.close(None).await;
                    });
                }
            });

            let (tx, _rx) = watch::channel(None);
            let config = WsFeedConfig { max_consecutive_failures: Some(2), ..config() };
            let feed = tokio::spawn(run_ws_feed("mock", config, tx, MockSource { url }));

            if expect_give_up {
                let result = timeout(Duration::from_secs(5), feed)
                    .await
                    .expect("feed did not give up")
                    .unwrap();
                match result {
                    Err(FeedError::ConnectionError(reason)) => {
                        assert!(reason.contains("no pricing data"), "unexpected reason: {reason}")
                    }
                    other => panic!("expected a connection error, got {other:?}"),
                }
                assert_eq!(connections.load(Ordering::SeqCst), 2);
            } else {
                timeout(Duration::from_secs(5), await_connections(&connections, 4))
                    .await
                    .expect("feed stopped reconnecting");
                assert!(
                    !feed.is_finished(),
                    "feed gave up although every connection delivered a book"
                );
            }
        }

        /// What the mock server does on the first connection to make it unusable.
        #[derive(Clone, Copy)]
        enum Disconnect {
            /// Stays connected but sends nothing past `read_idle_timeout`.
            IdleTimeout,
            /// Sends a close frame.
            CloseFrame,
            /// Sends a frame the source cannot decode.
            UndecodableFrame,
        }

        #[rstest]
        #[case::idle_timeout(Disconnect::IdleTimeout)]
        #[case::close_frame(Disconnect::CloseFrame)]
        #[case::undecodable_frame(Disconnect::UndecodableFrame)]
        #[tokio::test]
        async fn reconnects_when_the_connection_becomes_unusable(#[case] disconnect: Disconnect) {
            // The first connection never delivers a book; the second does. The only way the
            // book reaches the receiver is a reconnect.
            let (listener, url) = mock_server().await;
            let connections = Arc::new(AtomicU32::new(0));
            let connections_clone = Arc::clone(&connections);

            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let count = connections_clone.fetch_add(1, Ordering::SeqCst) + 1;
                    tokio::spawn(async move {
                        let Ok(mut ws_stream) = accept_async(stream).await else { return };
                        if count == 1 {
                            match disconnect {
                                Disconnect::IdleTimeout => {}
                                Disconnect::CloseFrame => {
                                    let _ = ws_stream.close(None).await;
                                }
                                Disconnect::UndecodableFrame => {
                                    let _ = ws_stream.send(bad_frame()).await;
                                }
                            }
                        } else {
                            let _ = ws_stream.send(book_frame()).await;
                        }
                        // Hold the connection so the client, not a server-side drop, ends it.
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    });
                }
            });

            let (tx, mut rx) = watch::channel(None);
            // Only the idle case may rely on the idle timeout; the others must reconnect on
            // their own signal well before it.
            let read_idle_timeout = match disconnect {
                Disconnect::IdleTimeout => Duration::from_millis(50),
                Disconnect::CloseFrame | Disconnect::UndecodableFrame => Duration::from_secs(60),
            };
            let config = WsFeedConfig { read_idle_timeout, ..config() };
            let _feed = tokio::spawn(run_ws_feed("mock", config, tx, MockSource { url }));

            timeout(Duration::from_secs(2), next_books(&mut rx))
                .await
                .expect("no book after the reconnect");
            assert_eq!(connections.load(Ordering::SeqCst), 2);
        }

        #[tokio::test]
        async fn gives_up_after_max_consecutive_failed_connections() {
            // Bind and immediately drop the listener so connections are refused.
            let (listener, url) = mock_server().await;
            drop(listener);

            let (tx, mut rx) = watch::channel(None);
            let config = WsFeedConfig { max_consecutive_failures: Some(3), ..config() };
            let feed = tokio::spawn(run_ws_feed("mock", config, tx, MockSource { url }));

            let result = timeout(Duration::from_secs(5), feed)
                .await
                .expect("feed did not give up within 5 seconds")
                .unwrap();
            assert!(matches!(result, Err(FeedError::ConnectionError(_))));

            // The feed task dropped the sender on exit.
            assert!(rx.changed().await.is_err());
        }
    }

    mod http_poll_feed {
        use std::{
            future::Future,
            sync::{
                atomic::{AtomicBool, AtomicU32, Ordering},
                Arc as StdArc,
            },
        };

        use super::*;

        /// Lets a closure stand in for a provider's source.
        impl<F, Fut> HttpBookSource for F
        where
            F: Fn() -> Fut + Send + Sync,
            Fut: Future<Output = Result<HashMap<String, Book>, FeedError>> + Send,
        {
            async fn fetch_books(&self) -> Result<HashMap<String, Book>, FeedError> {
                self().await
            }
        }

        fn config(max_consecutive_failures: Option<u32>) -> HttpFeedConfig {
            HttpFeedConfig {
                poll_interval: Duration::from_millis(5),
                request_timeout: Duration::from_millis(50),
                max_consecutive_failures,
                max_missed_polls: None,
            }
        }

        #[tokio::test]
        async fn publishes_book_and_success_resets_failures() {
            // One failed poll, then successes: with a limit of 2 the single failure must not
            // accumulate towards termination once a poll succeeds.
            let polls = StdArc::new(AtomicU32::new(0));
            let polls_clone = StdArc::clone(&polls);
            let (tx, mut rx) = watch::channel(None);
            let feed = run_http_poll_feed("mock", config(Some(2)), tx, move || {
                let n = polls_clone.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        Err(FeedError::ConnectionError("boom".to_string()))
                    } else {
                        Ok(book())
                    }
                }
            });
            let feed = tokio::spawn(feed);

            timeout(Duration::from_secs(1), async {
                loop {
                    rx.changed().await.expect("feed ended");
                    if rx.borrow_and_update().is_some() {
                        return;
                    }
                }
            })
            .await
            .expect("no book published");

            // Several more polls run without termination.
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!feed.is_finished());
        }

        #[tokio::test]
        async fn withdraws_book_after_missed_polls_and_restores_on_next_success() {
            // One successful poll, then failures until the flag flips: the published snapshot
            // must be withdrawn once max_missed_polls polls in a row have failed and come back
            // with the next successful poll.
            let polls = StdArc::new(AtomicU32::new(0));
            let polls_clone = StdArc::clone(&polls);
            let recovered = StdArc::new(AtomicBool::new(false));
            let recovered_clone = StdArc::clone(&recovered);
            let (tx, mut rx) = watch::channel(None);
            let config = HttpFeedConfig { max_missed_polls: Some(2), ..config(None) };
            let feed = run_http_poll_feed("mock", config, tx, move || {
                let n = polls_clone.fetch_add(1, Ordering::SeqCst);
                let recovered = recovered_clone.load(Ordering::SeqCst);
                async move {
                    if n == 0 || recovered {
                        Ok(book())
                    } else {
                        Err(FeedError::ConnectionError("boom".to_string()))
                    }
                }
            });
            let _feed = tokio::spawn(feed);

            let wait_for = |rx: &mut watch::Receiver<Option<BookSnapshot<ReceivedAt>>>,
                            want_some: bool| {
                let mut rx = rx.clone();
                async move {
                    timeout(Duration::from_secs(1), async {
                        loop {
                            if rx.borrow_and_update().is_some() == want_some {
                                return;
                            }
                            rx.changed().await.expect("feed ended");
                        }
                    })
                    .await
                    .expect("watch did not reach the expected state");
                }
            };
            wait_for(&mut rx, true).await;
            wait_for(&mut rx, false).await;
            recovered.store(true, Ordering::SeqCst);
            wait_for(&mut rx, true).await;
        }

        #[tokio::test]
        async fn rejects_zero_poll_interval() {
            let (tx, _rx) = watch::channel(None);
            let config = HttpFeedConfig { poll_interval: Duration::ZERO, ..config(None) };
            let result = run_http_poll_feed("mock", config, tx, || async { Ok(book()) }).await;
            assert!(matches!(result, Err(FeedError::InvalidInput(_))));
        }

        #[tokio::test]
        async fn gives_up_after_max_consecutive_failed_polls() {
            let (tx, _rx) = watch::channel(None);
            let feed = run_http_poll_feed("mock", config(Some(3)), tx, || async {
                Err(FeedError::ConnectionError("boom".to_string()))
            });
            let feed = tokio::spawn(feed);

            let result = timeout(Duration::from_secs(1), feed)
                .await
                .expect("feed did not give up")
                .unwrap();
            assert!(matches!(result, Err(FeedError::ConnectionError(_))));
        }

        #[tokio::test]
        async fn withdraws_the_published_book_when_it_gives_up() {
            // A book published before the feed gave up must not stay servable: nothing is
            // refreshing it, and a consumer holding the receiver would keep quoting it.
            let polls = StdArc::new(AtomicU32::new(0));
            let polls_clone = StdArc::clone(&polls);
            let (tx, mut rx) = watch::channel(None);
            let feed = run_http_poll_feed("mock", config(Some(2)), tx, move || {
                let n = polls_clone.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n == 0 {
                        Ok(book())
                    } else {
                        Err(FeedError::ConnectionError("boom".to_string()))
                    }
                }
            });
            let feed = tokio::spawn(feed);

            timeout(Duration::from_secs(1), async {
                loop {
                    rx.changed()
                        .await
                        .expect("feed ended before publishing");
                    if rx.borrow_and_update().is_some() {
                        return;
                    }
                }
            })
            .await
            .expect("no book published");

            let result = timeout(Duration::from_secs(1), feed)
                .await
                .expect("feed did not give up")
                .unwrap();
            assert!(matches!(result, Err(FeedError::ConnectionError(_))));
            assert!(rx.borrow().is_none(), "the book must be withdrawn when the feed gives up");
        }

        #[tokio::test]
        async fn withdraws_the_published_book_when_the_feed_is_dropped() {
            // Dropping the feed future — the consumer aborting its task — leaves the receiver
            // alive; the book stops being refreshed and must stop being served with it.
            let (tx, mut rx) = watch::channel(None);
            let feed = run_http_poll_feed("mock", config(None), tx, || async { Ok(book()) });
            let feed = tokio::spawn(feed);

            timeout(Duration::from_secs(1), async {
                loop {
                    rx.changed()
                        .await
                        .expect("feed ended before publishing");
                    if rx.borrow_and_update().is_some() {
                        return;
                    }
                }
            })
            .await
            .expect("no book published");

            feed.abort();
            timeout(Duration::from_secs(1), rx.changed())
                .await
                .expect("the book was not withdrawn")
                .expect("the withdrawal must reach the receiver before the sender drops");
            assert!(rx.borrow().is_none());
        }

        #[tokio::test]
        async fn retries_forever_without_limit() {
            let polls = StdArc::new(AtomicU32::new(0));
            let polls_clone = StdArc::clone(&polls);
            let (tx, _rx) = watch::channel(None);
            let feed = run_http_poll_feed("mock", config(None), tx, move || {
                polls_clone.fetch_add(1, Ordering::SeqCst);
                async { Err(FeedError::ConnectionError("boom".to_string())) }
            });
            let feed = tokio::spawn(feed);

            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(polls.load(Ordering::SeqCst) > 5, "should keep polling");
            assert!(!feed.is_finished());
        }

        #[tokio::test]
        async fn hung_poll_counts_as_failure() {
            let (tx, _rx) = watch::channel(None);
            let feed = run_http_poll_feed(
                "mock",
                HttpFeedConfig {
                    poll_interval: Duration::from_millis(5),
                    request_timeout: Duration::from_millis(5),
                    max_consecutive_failures: Some(2),
                    max_missed_polls: None,
                },
                tx,
                || async {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Ok(HashMap::new())
                },
            );
            let feed = tokio::spawn(feed);

            let result = timeout(Duration::from_secs(1), feed)
                .await
                .expect("feed did not give up")
                .unwrap();
            assert!(
                matches!(result, Err(FeedError::ConnectionError(msg)) if msg.contains("timed out"))
            );
        }

        #[tokio::test]
        async fn stops_when_all_receivers_drop() {
            let (tx, rx) = watch::channel(None);
            let feed = run_http_poll_feed("mock", config(None), tx, || async { Ok(book()) });
            let feed = tokio::spawn(feed);

            drop(rx);
            let result = timeout(Duration::from_secs(1), feed)
                .await
                .expect("feed did not stop after receivers dropped")
                .unwrap();
            assert!(result.is_ok());
        }
    }
}
