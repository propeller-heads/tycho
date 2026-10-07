//! Feeds that hold a connection: the shared connection loop, and the source trait a provider
//! implements for it.

use std::time::Duration;

use ::http::Request;
use async_stream::try_stream;
use futures::{Stream, StreamExt};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, Bytes, Message, Utf8Bytes},
};
use tracing::{debug_span, error, info, warn};

use super::{errors::FeedError, failures::FailureTracker, publisher::Publisher};

/// Feed tuning for WebSocket-streaming feeds. Set through the feed builder.
///
/// There is no `Default`: start from the builder's `default_feed_config()`, which holds the
/// values that fit that venue, and change individual fields with struct-update syntax.
#[derive(Clone, Debug)]
pub struct WsFeedConfig {
    /// Deadline for the WebSocket handshake.
    pub connect_timeout: Duration,
    /// Reconnect when no frame arrives within this window.
    pub read_idle_timeout: Duration,
    /// How long the feed waits before reconnecting after one failed connection. Each further
    /// consecutive failure waits twice the previous, up to `max_backoff_exp` doublings.
    pub backoff_unit: Duration,
    /// How many times the reconnect wait may double before it stops growing, capping it at
    /// `backoff_unit * 2^max_backoff_exp`.
    pub max_backoff_exp: u32,
    /// Consecutive failures (connections without decoded pricing data) before the feed gives
    /// up and resolves with an error. `None` retries forever.
    pub max_consecutive_failures: Option<u32>,
    /// Withdraw the published snapshot (publish `None`) when no new one has arrived for this long,
    /// so consumers stop using data nobody is updating; the next decoded one restores it.
    /// Measured from the last publish on the feed's clock, so it also covers reconnect waits.
    /// Must not be zero: the feed future resolves with `InvalidInput` right away. `None` keeps
    /// the last snapshot until the feed ends.
    pub max_snapshot_age: Option<Duration>,
}

/// The values every WebSocket feed builder starts from: 10 s handshake deadline, 60 s idle
/// window, a reconnect wait of 1 s doubling to at most 32 s, retry forever, and withdrawal after
/// 60 s without a snapshot, which is the idle window — the moment the loop declares the socket
/// dead it also stops serving the snapshot.
pub(crate) fn default_ws_feed_config() -> WsFeedConfig {
    WsFeedConfig {
        connect_timeout: Duration::from_secs(10),
        read_idle_timeout: Duration::from_secs(60),
        backoff_unit: Duration::from_secs(1),
        max_backoff_exp: 5,
        max_consecutive_failures: None,
        max_snapshot_age: Some(Duration::from_secs(60)),
    }
}

/// The connection loop shared by WebSocket-based feeds.
///
/// Keeps `source` connected on the terms `config` sets and publishes every snapshot decoded off
/// the socket, withdrawing one that goes `max_snapshot_age` without a refresh. What each knob
/// buys is on its own field; this is where they are checked and handed to the pieces that act on
/// them.
///
/// Resolves `Err` when the feed gives up: on a fatal error from the source, whatever the failure
/// budget, after `max_consecutive_failures` failed connections where one is configured, or right
/// away with `InvalidInput` for a zero `max_snapshot_age`. Resolves `Ok(())` as soon as the last
/// receiver goes away, mid-connect or mid-read included, since nothing it read from then on could
/// reach anyone; reconnecting itself never runs out, so a feed is otherwise stopped by dropping
/// its future, which withdraws the published snapshot on the way out.
pub(crate) async fn run_ws_feed<S: WsSource>(
    config: WsFeedConfig,
    publisher: Publisher<S::Snapshot>,
    source: S,
) -> Result<(), FeedError> {
    // Where the config comes apart, and the only place that sees all of it: the loop waits on
    // the two deadlines and spends the failure budget, the publisher withdraws what has gone
    // stale.
    let WsFeedConfig {
        connect_timeout,
        read_idle_timeout,
        backoff_unit,
        max_backoff_exp,
        max_consecutive_failures,
        max_snapshot_age,
    } = config;

    if max_snapshot_age.is_some_and(|age| age.is_zero()) {
        return Err(FeedError::InvalidInput("max_snapshot_age must not be zero".to_string()));
    }

    let failures = FailureTracker::new(max_consecutive_failures, backoff_unit, max_backoff_exp);
    let snapshots = streamed_snapshots(connect_timeout, read_idle_timeout, failures, source);
    publisher
        .publishing(max_snapshot_age, snapshots)
        .await
}

/// A data-bearing WebSocket frame. Control frames never reach a [`WsSource`]: the loop
/// reconnects on `Close`, tokio-tungstenite answers pings while the stream is polled, and
/// tungstenite never yields raw frames on the read side.
pub(crate) enum WsPayload {
    Text(Utf8Bytes),
    Binary(Bytes),
}

/// The provider-specific half of a WebSocket feed: it builds the handshake request and decodes
/// data frames into complete snapshots. [`run_ws_feed`] owns the source and drives it by
/// reference.
pub(crate) trait WsSource: Send {
    type Snapshot: Send + Sync + 'static;

    fn request(&self) -> Result<Request<()>, FeedError>;

    /// `Ok(None)` for frames that carry no snapshot.
    fn decode(&mut self, payload: WsPayload) -> Result<Option<Self::Snapshot>, FeedError>;
}

/// Every snapshot decoded off the socket, and as a last item the error the feed gives up on.
///
/// Connects with the source's request, reads frames, and hands every data frame to the source to
/// decode, in a `ws_frame` span. A decode error, read error, idle timeout, stream end or server
/// close ends the connection and `failures` decides how long the reconnect waits. Only a decoded
/// snapshot counts as success — a connection that never produced one is one failure.
///
/// Connecting, reading and backing off happen in here, so the one who awaits the next snapshot
/// waits exactly as long as it takes — which is how a published one can go stale on time while a
/// connection is silent or a reconnect is hanging.
fn streamed_snapshots<S: WsSource>(
    connect_timeout: Duration,
    read_idle_timeout: Duration,
    mut failures: FailureTracker,
    mut source: S,
) -> impl Stream<Item = Result<S::Snapshot, FeedError>> {
    try_stream! {
        loop {
            let request = source.request()?;
            let connected = timeout(connect_timeout, connect_async(request)).await;
            // Why this connection ended, in the words of whoever found out. Every end is a
            // failure to this loop — a WebSocket that closes is not serving snapshots — so the
            // reason is reported once, here, with the streak it belongs to.
            let ended = match connected {
                Ok(Ok((mut ws_stream, _))) => {
                    info!("connected");
                    loop {
                        match read_next_snapshot(read_idle_timeout, &mut ws_stream, &mut source).await {
                            ConnectionRead::Snapshot(snapshot) => {
                                // A completed handshake says nothing about whether the
                                // connection works, so only a decoded snapshot clears the counter.
                                let cleared = failures.record_success();
                                if cleared > 0 {
                                    info!(after_failures = cleared, "snapshots received again");
                                }
                                yield snapshot;
                            }
                            ConnectionRead::Reconnect(reason) => break reason,
                            ConnectionRead::Fatal(e) => Err(e)?,
                        }
                    }
                }
                Ok(Err(e)) => FeedError::Connection(format!("connect failed: {e}")),
                Err(_) => FeedError::Connection(format!(
                    "connect timed out after {}s",
                    connect_timeout.as_secs()
                )),
            };

            let retry = failures
                .record_failure()
                .inspect(|retry| {
                    warn!(
                        consecutive = retry.consecutive,
                        backoff = ?retry.backoff,
                        reason = %ended,
                        "connection ended, reconnecting"
                    )
                })
                // The feed ends with the failure that ended it, classified as whoever found out
                // classified it: a run of unreadable frames is not a connection failure, and how
                // long the feed tolerated it is for the log rather than for the caller to act on.
                .map_err(|consecutive| {
                    ended.in_context(format_args!("gave up after {consecutive} consecutive failures"))
                })?;
            sleep(retry.backoff).await;
        }
    }
}

/// What one read of a WebSocket connection produced, and with it what the caller does next.
enum ConnectionRead<T> {
    /// A frame decoded into a complete snapshot: publish it and read on.
    Snapshot(T),
    /// This connection will serve no more snapshots — it was closed, went silent, or answered with
    /// something the source cannot read — so the caller opens another. The reason is carried
    /// rather than logged, so one warning can report it together with the failure streak it
    /// belongs to, and so the feed that eventually gives up gives up on this error rather than
    /// on a retyped copy of its message.
    Reconnect(FeedError),
    /// The source reported a failure another connection would answer the same way, so the
    /// caller gives up.
    Fatal(FeedError),
}

/// Reads frames off one WebSocket connection until one decodes into a snapshot, or until the
/// connection becomes unusable. Frames that carry no snapshot are read past.
async fn read_next_snapshot<S: WsSource>(
    read_idle_timeout: Duration,
    ws_stream: &mut (impl Stream<Item = Result<Message, tungstenite::Error>> + Unpin),
    source: &mut S,
) -> ConnectionRead<S::Snapshot> {
    loop {
        let received = timeout(read_idle_timeout, ws_stream.next()).await;
        let message = match received {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(e))) => {
                return ConnectionRead::Reconnect(FeedError::Connection(format!("read failed: {e}")))
            }
            Ok(None) => {
                return ConnectionRead::Reconnect(FeedError::Connection("stream ended".to_string()))
            }
            Err(_) => {
                return ConnectionRead::Reconnect(FeedError::Connection(format!(
                    "no frame within the {}s read_idle_timeout",
                    read_idle_timeout.as_secs()
                )))
            }
        };

        let payload = match message {
            Message::Text(text) => WsPayload::Text(text),
            Message::Binary(data) => WsPayload::Binary(data),
            // Pings are answered by tokio-tungstenite as the stream is polled; pongs need no
            // reaction.
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(Some(frame)) => {
                return ConnectionRead::Reconnect(FeedError::Connection(format!(
                    "closed by server: {frame}"
                )))
            }
            Message::Close(None) => {
                return ConnectionRead::Reconnect(FeedError::Connection(
                    "closed by server without a close frame".to_string(),
                ))
            }
            // Documented as write-side only; a read yielding one is a tungstenite bug, not a
            // provider problem.
            Message::Frame(frame) => {
                error!(%frame, "ignoring raw frame on the read side");
                continue;
            }
        };

        let decoded = debug_span!("ws_frame").in_scope(|| source.decode(payload));
        return match decoded {
            Ok(None) => continue,
            Ok(Some(snapshot)) => ConnectionRead::Snapshot(snapshot),
            Err(e) => {
                if e.is_fatal() {
                    ConnectionRead::Fatal(e)
                } else {
                    ConnectionRead::Reconnect(e.in_context("decode failed"))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };

    use futures::{SinkExt, StreamExt};
    use rstest::rstest;
    use tokio::{net::TcpListener, sync::watch, time::Instant};
    use tokio_tungstenite::{accept_async, tungstenite::client::IntoClientRequest};

    use super::{super::expect_to_finish, *};

    /// A source that is never reached: the config is rejected before any connection.
    struct NoSource;

    impl WsSource for NoSource {
        type Snapshot = u32;

        fn request(&self) -> Result<Request<()>, FeedError> {
            unreachable!("the loop must reject the config before building a request")
        }

        fn decode(&mut self, _payload: WsPayload) -> Result<Option<u32>, FeedError> {
            unreachable!()
        }
    }

    /// A source for a mock server: a binary frame is one complete snapshot unless it is the
    /// [`bad_frame`], which fails to decode; text frames carry nothing.
    struct MockSource {
        url: String,
    }

    impl WsSource for MockSource {
        type Snapshot = u32;

        fn request(&self) -> Result<Request<()>, FeedError> {
            self.url
                .as_str()
                .into_client_request()
                .map_err(|e| FeedError::Fatal(e.to_string()))
        }

        fn decode(&mut self, payload: WsPayload) -> Result<Option<u32>, FeedError> {
            match payload {
                WsPayload::Binary(data) if data.as_ref() == BAD_FRAME => {
                    Err(FeedError::Parsing("undecodable frame".to_string()))
                }
                WsPayload::Binary(data) if data.as_ref() == FATAL_FRAME => {
                    Err(FeedError::Fatal("key rejected".to_string()))
                }
                WsPayload::Binary(_) => Ok(Some(1)),
                WsPayload::Text(_) => Ok(None),
            }
        }
    }

    const BAD_FRAME: &[u8] = &[0xff];
    const FATAL_FRAME: &[u8] = &[0xfe];

    /// A frame the mock source decodes into a snapshot.
    fn snapshot_frame() -> Message {
        Message::Binary(vec![1].into())
    }

    /// A frame the mock source fails to decode.
    fn bad_frame() -> Message {
        Message::Binary(BAD_FRAME.to_vec().into())
    }

    /// A frame the mock source rejects as fatal.
    fn fatal_frame() -> Message {
        Message::Binary(FATAL_FRAME.to_vec().into())
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

    /// Delivers `script` to [`read_next_book`] as a connection would, each frame after its own
    /// wait, and then stays silent forever. Nothing here touches a socket, so on a paused clock
    /// the frames arrive at exactly the times the script names.
    fn scripted_frames(
        script: Vec<(Duration, Message)>,
    ) -> impl Stream<Item = Result<Message, tungstenite::Error>> {
        try_stream! {
            for (silence, message) in script {
                sleep(silence).await;
                yield message;
            }
            std::future::pending::<()>().await;
        }
    }

    /// Waits until the receiver holds a snapshot.
    async fn next_snapshot(rx: &mut watch::Receiver<Option<u32>>) {
        loop {
            rx.changed().await.expect("feed ended");
            if rx.borrow_and_update().is_some() {
                return;
            }
        }
    }

    #[tokio::test]
    async fn gives_up_on_a_fatal_decode_error_despite_an_unlimited_failure_budget() {
        // A provider that answers with something reconnecting cannot fix — a rejected key,
        // say — ends the feed rather than another connection attempt, and the snapshot it had
        // published goes with it.
        let (listener, url) = mock_server().await;
        let connections = Arc::new(AtomicU32::new(0));
        let connections_clone = Arc::clone(&connections);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                connections_clone.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let Ok(mut ws_stream) = accept_async(stream).await else { return };
                    let _ = ws_stream.send(snapshot_frame()).await;
                    let _ = ws_stream.send(fatal_frame()).await;
                    std::future::pending::<()>().await;
                });
            }
        });

        let (publisher, mut rx) = Publisher::channel();
        let feed = tokio::spawn(run_ws_feed(config(), publisher, MockSource { url }));

        let result = expect_to_finish("feed did not give up", feed)
            .await
            .unwrap();

        assert!(matches!(result, Err(FeedError::Fatal(_))));
        assert_eq!(connections.load(Ordering::SeqCst), 1, "the feed reconnected after the fatal");
        assert!(rx.borrow_and_update().is_none(), "the snapshot must be withdrawn");
    }

    #[tokio::test]
    async fn rejects_zero_max_snapshot_age() {
        let (publisher, _rx) = Publisher::channel();
        let config = WsFeedConfig { max_snapshot_age: Some(Duration::ZERO), ..config() };
        let result = run_ws_feed(config, publisher, NoSource).await;
        assert!(matches!(result, Err(FeedError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn reconnects_after_the_server_drops_the_connection() {
        // The server sends one snapshot, drops the first connection, then sends a second on
        // the reconnect and stays up.
        let (listener, url) = mock_server().await;
        let connection_count = Arc::new(AtomicU32::new(0));
        let connection_count_clone = Arc::clone(&connection_count);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let count = connection_count_clone.fetch_add(1, Ordering::SeqCst) + 1;

                tokio::spawn(async move {
                    if let Ok(ws_stream) = accept_async(stream).await {
                        let (mut ws_sender, _ws_receiver) = ws_stream.split();
                        let _ = ws_sender.send(snapshot_frame()).await;
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

        let (publisher, mut rx) = Publisher::channel();
        let _feed = tokio::spawn(run_ws_feed(config(), publisher, MockSource { url }));

        // One stream observes a snapshot, survives the server-side drop, and observes
        // the one sent on the reconnected connection.
        expect_to_finish("no first snapshot", next_snapshot(&mut rx)).await;
        expect_to_finish("no snapshot after reconnect", next_snapshot(&mut rx)).await;

        assert_eq!(connection_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn answers_pings_while_the_consumer_stalls() {
        // The socket must be polled (and pings answered) by the feed task itself,
        // independent of how often the consumer reads the watch.
        let (listener, url) = mock_server().await;
        let pong_count = Arc::new(AtomicU32::new(0));
        let pong_count_clone = Arc::clone(&pong_count);

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                if let Ok(mut ws_stream) = accept_async(stream).await {
                    let _ = ws_stream.send(snapshot_frame()).await;
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
                                pong_count_clone.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                }
            }
        });

        let (publisher, rx) = Publisher::channel();
        let _feed = tokio::spawn(run_ws_feed(config(), publisher, MockSource { url }));

        // Never read the receiver: a stalled consumer must not stall the socket.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        drop(rx);

        let pongs = pong_count.load(Ordering::SeqCst);
        assert!(pongs >= 5, "expected at least 5 pongs while consumer stalled, got {pongs}");
    }

    #[tokio::test]
    async fn withdraws_a_stale_snapshot_while_a_reconnect_hangs() {
        // The server sends one snapshot, drops the connection, and then accepts TCP without ever
        // completing a handshake, so the reconnect sits in connect_timeout. The published
        // snapshot must be withdrawn while that connect is still in flight, long before the
        // 30 s connect deadline it is waiting on.
        let (listener, url) = mock_server().await;
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws_stream = accept_async(stream).await.unwrap();
            let (mut ws_sender, _ws_receiver) = ws_stream.split();
            let _ = ws_sender.send(snapshot_frame()).await;
            let _ = ws_sender.close().await;
            // Further connects complete at the TCP level from the backlog and then hang.
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });

        let (publisher, mut rx) = Publisher::channel();
        let config = WsFeedConfig {
            max_snapshot_age: Some(Duration::from_millis(100)),
            connect_timeout: Duration::from_secs(30),
            ..config()
        };
        let _feed = tokio::spawn(run_ws_feed(config, publisher, MockSource { url }));

        expect_to_finish("no first snapshot", next_snapshot(&mut rx)).await;

        // The deadline is the assertion: a withdrawal that waited for the connect to give up
        // would arrive at 30 s, six times later than this.
        timeout(Duration::from_secs(5), async {
            loop {
                rx.changed().await.expect("feed ended");
                if rx.borrow_and_update().is_none() {
                    return;
                }
            }
        })
        .await
        .expect("stale snapshot was not withdrawn while the reconnect hung");
    }

    #[tokio::test]
    async fn withdraws_a_stale_snapshot_while_the_connection_is_silent() {
        // The server sends one snapshot, stays connected but silent, and sends a second one
        // only when told to: the published snapshot must be withdrawn after max_snapshot_age
        // and come back with the next frame.
        let (listener, url) = mock_server().await;
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws_stream = accept_async(stream).await.unwrap();
            let (mut ws_sender, _ws_receiver) = ws_stream.split();
            let _ = ws_sender.send(snapshot_frame()).await;
            let _ = resume_rx.await;
            let _ = ws_sender.send(snapshot_frame()).await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        });

        let (publisher, mut rx) = Publisher::channel();
        let config =
            WsFeedConfig { max_snapshot_age: Some(Duration::from_millis(100)), ..config() };
        let _feed = tokio::spawn(run_ws_feed(config, publisher, MockSource { url }));

        expect_to_finish("no first snapshot", next_snapshot(&mut rx)).await;

        // The deadline is the assertion: the snapshot must go on its own age, well before the 60 s
        // idle window in which the loop declares the silent socket dead.
        timeout(Duration::from_secs(5), async {
            loop {
                rx.changed().await.expect("feed ended");
                if rx.borrow_and_update().is_none() {
                    return;
                }
            }
        })
        .await
        .expect("stale snapshot was not withdrawn");

        resume_tx.send(()).unwrap();
        expect_to_finish("no snapshot after the stream resumed", next_snapshot(&mut rx)).await;
    }

    #[rstest]
    #[case::snapshotless_connections_count_as_failures(false, true)]
    #[case::a_snapshot_per_connection_keeps_the_feed_alive(true, false)]
    #[tokio::test]
    async fn only_decoded_snapshots_reset_the_failure_streak(
        #[case] server_sends_a_snapshot: bool,
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
                    let frame = if server_sends_a_snapshot {
                        snapshot_frame()
                    } else {
                        Message::Text("notice".into())
                    };
                    let _ = ws_stream.send(frame).await;
                    let _ = ws_stream.close(None).await;
                });
            }
        });

        let (publisher, _rx) = Publisher::channel();
        let config = WsFeedConfig { max_consecutive_failures: Some(2), ..config() };
        let feed = tokio::spawn(run_ws_feed(config, publisher, MockSource { url }));

        if expect_give_up {
            let result = expect_to_finish("feed did not give up", feed)
                .await
                .unwrap();
            match result {
                Err(FeedError::Connection(reason)) => {
                    // Both snapshotless connections counted, and the error names the last one's
                    // end — the server's close, not a failure to connect.
                    assert!(
                        reason.contains("2 consecutive failures") &&
                            reason.contains("closed by server"),
                        "unexpected reason: {reason}"
                    )
                }
                other => panic!("expected a connection error, got {other:?}"),
            }
            assert_eq!(connections.load(Ordering::SeqCst), 2);
        } else {
            expect_to_finish("feed stopped reconnecting", await_connections(&connections, 4)).await;
            assert!(
                !feed.is_finished(),
                "feed gave up although every connection delivered a snapshot"
            );
        }
    }

    #[rstest]
    #[case::silent_from_the_first_read(vec![], Duration::from_secs(60))]
    #[case::a_ping_buys_another_window(
        vec![(Duration::from_secs(40), Message::Ping(Vec::new().into()))],
        Duration::from_secs(100)
    )]
    #[case::so_does_a_frame_that_carries_no_snapshot(
        vec![(Duration::from_secs(40), Message::Text("notice".into()))],
        Duration::from_secs(100)
    )]
    #[tokio::test(start_paused = true)]
    async fn a_connection_is_given_up_on_one_whole_idle_window_after_its_last_frame(
        #[case] script: Vec<(Duration, Message)>,
        #[case] give_up_at: Duration,
    ) {
        // The deadline is per read, so anything arriving restarts it — a ping the server sends
        // to keep the socket warm, or a notice the source decodes into no snapshot — and the loop
        // gives up only once a whole window has passed with nothing at all.
        let read_idle_timeout = Duration::from_secs(60);
        let frames = scripted_frames(script);
        tokio::pin!(frames);
        let start = Instant::now();

        let read = read_next_snapshot(
            read_idle_timeout,
            &mut frames,
            &mut MockSource { url: String::new() },
        )
        .await;

        assert_eq!(start.elapsed(), give_up_at);
        match read {
            ConnectionRead::Reconnect(FeedError::Connection(reason)) => {
                assert_eq!(reason, "no frame within the 60s read_idle_timeout")
            }
            _ => panic!("a silent connection must be given up on, not read further"),
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
        // The first connection never delivers a snapshot; the second does. The only way the
        // snapshot reaches the receiver is a reconnect.
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
                        let _ = ws_stream.send(snapshot_frame()).await;
                    }
                    // Hold the connection so the client, not a server-side drop, ends it.
                    tokio::time::sleep(Duration::from_secs(10)).await;
                });
            }
        });

        let (publisher, mut rx) = Publisher::channel();
        // Only the idle case may rely on the idle timeout; the others must reconnect on
        // their own signal well before it.
        let read_idle_timeout = match disconnect {
            Disconnect::IdleTimeout => Duration::from_millis(50),
            Disconnect::CloseFrame | Disconnect::UndecodableFrame => Duration::from_secs(60),
        };
        let config = WsFeedConfig { read_idle_timeout, ..config() };
        let _feed = tokio::spawn(run_ws_feed(config, publisher, MockSource { url }));

        timeout(Duration::from_secs(2), next_snapshot(&mut rx))
            .await
            .expect("no snapshot after the reconnect");
        assert_eq!(connections.load(Ordering::SeqCst), 2);
    }

    /// The feed ends with the failure that ended it, classified as the source classified it: a
    /// run of frames the source cannot read is a parsing failure, not a connection one, however
    /// the connection carrying them behaved.
    #[tokio::test]
    async fn giving_up_reports_what_the_source_made_of_the_last_frame() {
        let (listener, url) = mock_server().await;
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let Ok(mut ws_stream) = accept_async(stream).await else { return };
                    let _ = ws_stream.send(bad_frame()).await;
                });
            }
        });

        let (publisher, _rx) = Publisher::channel();
        let config = WsFeedConfig { max_consecutive_failures: Some(2), ..config() };
        let feed = tokio::spawn(run_ws_feed(config, publisher, MockSource { url }));

        let result = expect_to_finish("feed did not give up", feed)
            .await
            .unwrap();

        match result {
            Err(FeedError::Parsing(reason)) => assert!(
                reason.contains("2 consecutive failures") && reason.contains("undecodable frame"),
                "unexpected reason: {reason}"
            ),
            other => panic!("expected a parsing error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gives_up_after_max_consecutive_failed_connections() {
        // Bind and immediately drop the listener so connections are refused.
        let (listener, url) = mock_server().await;
        drop(listener);

        let (publisher, mut rx) = Publisher::channel();
        let config = WsFeedConfig { max_consecutive_failures: Some(3), ..config() };
        let feed = tokio::spawn(run_ws_feed(config, publisher, MockSource { url }));

        let result = expect_to_finish("feed did not give up", feed)
            .await
            .unwrap();
        assert!(matches!(result, Err(FeedError::Connection(_))));

        // The feed task dropped the sender on exit.
        assert!(rx.changed().await.is_err());
    }
}
