//! Titan pAMM price level stream stream format and connection handling.
//!
//! Connects to the Titan `pamm_price_levels_v2` WebSocket (see
//! <https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level>) and yields parsed frames.
//! Each frame carries the books Titan simulated in one build round, targeting the block it is
//! currently building. Consumers decide how fresh a component is from the frame `ts`, not from
//! what a frame omits.
//!
//! All Titan specifics (endpoint, JSON shape, reconnect policy) live in this module; the rest of
//! the price level stream machinery is venue-agnostic.

use std::time::Duration;

use async_stream::stream;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use tokio::time::{sleep, timeout, timeout_at, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};
use tycho_common::Bytes;

use super::telemetry::{self, ReconnectReason, RejectReason};

/// Default Titan pAMM price level WebSocket endpoint. Titan serves the same stream from other
/// regions as well; see <https://docs.titanbuilder.xyz/propamms/takers>.
pub(super) const TITAN_PRICE_LEVEL_URL: &str =
    "wss://eu.data.titanbuilder.xyz/ws/pamm_price_levels_v2";

/// Environment variable that replaces [`TITAN_PRICE_LEVEL_URL`] when the builder is given no
/// endpoint.
pub(super) const TITAN_PRICE_LEVEL_URL_ENV: &str = "TITAN_PAMM_PRICE_LEVEL_URL";

/// Connection tuning for the Titan WebSocket, set through the
/// [`PriceLevelStreamBuilder`](super::stream::PriceLevelStreamBuilder).
#[derive(Clone, Copy, Debug)]
pub(super) struct ConnectionSettings {
    /// Longest a single connection attempt may take before it is aborted and retried, so a hung
    /// TCP/TLS handshake cannot block the stream forever.
    pub connect_timeout: Duration,
    /// Longest gap between *parsed* Titan frames tolerated before the socket is treated as dead
    /// and re-established. Titan pushes one frame per second and sends no keepalives, so a
    /// half-open socket is indistinguishable from silence; the 10 s default reconnects before
    /// the 24 s `stale_after` default removes any component. Pings, binary frames, and
    /// unparsable text do not reset the gap.
    pub read_idle_timeout: Duration,
    /// Cap on the exponential reconnect backoff (`2^attempt` seconds, at most this).
    pub max_backoff: Duration,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_idle_timeout: Duration::from_secs(10),
            max_backoff: Duration::from_secs(32),
        }
    }
}

/// The reconnect backoff after `attempt` consecutive failures: `2^attempt` seconds, capped at
/// `max_backoff`.
fn backoff(attempt: u32, max_backoff: Duration) -> Duration {
    2u64.checked_pow(attempt)
        .map_or(Duration::MAX, Duration::from_secs)
        .min(max_backoff)
}

/// The instant `idle_timeout` from now. The timeout is clamped to what the clock can represent,
/// so an absurd setting disables the watchdog instead of panicking on the addition.
fn deadline_after(idle_timeout: Duration) -> Instant {
    Instant::now() + idle_timeout.min(Duration::from_secs(u32::MAX.into()))
}

/// A parsed price level stream frame: every book Titan simulated in one build round, targeting
/// the block currently being built.
///
/// Books are quoted in human units, so a level is a decimal price and a cumulative volume rather
/// than a pair of token amounts, and tokens arrive as addresses, with symbols alongside them as
/// display labels: a consumer resolves the addresses against its own token metadata before it can
/// price anything.
///
/// Frames are best effort, not complete snapshots: a venue or pair can be absent from one frame
/// and present in the next (observed on 7.7% of frames in a 15 minute capture), so absence must
/// never be read as retirement.
#[derive(Debug, Deserialize)]
pub(super) struct TitanPriceLevelMessage {
    /// The L1 block number the quotes target (the block currently being built).
    pub block: u64,
    /// The beacon slot the frame was built for, when Titan reports one. No freshness decision
    /// rests on it, so a frame without it is still usable.
    pub slot: Option<u64>,
    /// When Titan built this frame, in nanoseconds since the Unix epoch. Frames re-emitted
    /// within one build round share a timestamp, so it is a freshness marker, not an identity.
    pub ts: u64,
    /// One book per venue and pair; a venue appears once for every pair it quotes.
    pub books: Vec<TitanBook>,
}

/// One venue's two-sided quote ladder for one pair, priced in `token2` per `token1`.
///
/// Ask prices sit above bid prices, and either side can be empty when the venue quotes only one
/// direction of the pair.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitanBook {
    /// Titan's display name for the venue (e.g. `Fermi`), which the stream sometimes leaves
    /// empty or omits. Venues are identified by `quote_address`, never by this label, and nothing
    /// reads it: it is typed here to document what the stream carries and to pin the shapes the
    /// parser accepts for it, not because any decision needs the value.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "a display label, deliberately not part of any identity the stream records"
        )
    )]
    pub maker: Option<String>,
    /// The venue address Titan streams this book under — a join key across its streams, which is
    /// not necessarily the venue's executable address (see
    /// [`PAMM_ADDRESS_ALIASES`](super::config::PAMM_ADDRESS_ALIASES)).
    pub quote_address: Bytes,
    /// Base token symbol, the unit of `cum_volume`. A display label: the pair is identified by
    /// [`token1_address`](Self::token1_address), never by this, so nothing resolves through it.
    /// It is read for the DEBUG logs that accompany a skipped book.
    pub token1: String,
    /// Quote token symbol, the unit of `price`. A display label, like
    /// [`token1`](Self::token1).
    pub token2: String,
    /// The address of the base token — the asset `cum_volume` is measured in.
    pub token1_address: Bytes,
    /// The address of the quote token — the asset `price` is measured in.
    pub token2_address: Bytes,
    pub asks: Vec<TitanLevel>,
    pub bids: Vec<TitanLevel>,
}

/// A single quote level: selling `cum_volume` of `token1` in total executes at an average of
/// `price`.
///
/// An average, not a limit: `ladders_for_book` computes `amount_out = price · cum_volume`, which
/// holds only under that reading.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitanLevel {
    /// `token2` per `token1`, in human units — both tokens' decimals are already applied.
    pub price: f64,
    /// Volume cumulated from the top of the book, in whole `token1`. Non-decreasing down a side,
    /// but consecutive levels can repeat a volume.
    pub cum_volume: f64,
}

/// Yields parsed price level frames from `url` for as long as the returned stream is polled.
///
/// Maintains the connection in the background of the stream itself: any disconnect, read error,
/// server-side close, or idle timeout is retried forever with capped exponential backoff (reset
/// only once a frame is actually parsed, so a socket that connects and immediately drops still
/// backs off instead of busy-looping). Malformed frames are logged and skipped; this stream never
/// terminates.
pub(super) fn messages(
    url: String,
    settings: ConnectionSettings,
) -> impl Stream<Item = TitanPriceLevelMessage> + Send {
    stream! {
        let mut attempt: u32 = 0;
        loop {
            match timeout(settings.connect_timeout, connect_async(url.as_str())).await {
                Ok(Ok((mut ws_stream, _))) => {
                    info!(%url, "Connected to Titan pAMM price level stream");
                    // The deadline moves only when a frame parses, so control frames and
                    // unparsable text cannot keep a socket that sends no frames alive.
                    let mut idle_deadline = deadline_after(settings.read_idle_timeout);
                    loop {
                        let message = match timeout_at(idle_deadline, ws_stream.next()).await {
                            Ok(Some(message)) => message,
                            // Stream ended: the server hung up without sending a close frame.
                            Ok(None) => {
                                warn!("Titan price level stream ended; reconnecting");
                                telemetry::record_reconnect(ReconnectReason::Ended);
                                break;
                            }
                            // No parsed frame within the idle window: assume a stalled socket.
                            Err(_elapsed) => {
                                warn!(
                                    idle_secs = settings.read_idle_timeout.as_secs(),
                                    "No parsed Titan frame within idle timeout; reconnecting"
                                );
                                telemetry::record_reconnect(ReconnectReason::IdleTimeout);
                                break;
                            }
                        };

                        match message {
                            Ok(Message::Text(text)) => {
                                match serde_json::from_str::<TitanPriceLevelMessage>(text.as_str())
                                {
                                    // A parsed frame proves the connection is healthy: reset the
                                    // reconnect backoff. The idle window restarts only once the
                                    // consumer resumes the stream after the yield, so it measures
                                    // the time spent waiting on the socket, not the time the
                                    // consumer spends between polls.
                                    Ok(message) => {
                                        attempt = 0;
                                        yield message;
                                        idle_deadline = deadline_after(settings.read_idle_timeout);
                                    }
                                    // Unparseable frame: log and keep the connection.
                                    Err(e) => {
                                        warn!(error = %e, "Failed to parse Titan price level message");
                                        telemetry::record_frame_rejected(RejectReason::ParseError);
                                    }
                                }
                            }
                            // Titan only sends JSON text; a binary frame is unexpected. Skip it
                            // and keep the (otherwise healthy) connection.
                            Ok(Message::Binary(bytes)) => {
                                warn!(len = bytes.len(), "Ignoring unexpected binary Titan frame");
                            }
                            // Keep-alive frames. tokio-tungstenite answers pings with pongs
                            // automatically while the stream is polled, so there is nothing to
                            // do.
                            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                            // `Frame` is only produced when *sending* raw frames; the read side
                            // never yields it, so this arm is unreachable in practice — kept only
                            // for match exhaustiveness.
                            Ok(Message::Frame(_)) => {}
                            // Server initiated a graceful close — reconnect.
                            Ok(Message::Close(frame)) => {
                                warn!(?frame, "Titan price level stream closed by server; reconnecting");
                                telemetry::record_reconnect(ReconnectReason::Closed);
                                break;
                            }
                            // Transport/protocol error (broken pipe, invalid frame, ...) —
                            // reconnect.
                            Err(e) => {
                                warn!(error = %e, "Titan price level stream read error; reconnecting");
                                telemetry::record_reconnect(ReconnectReason::ReadError);
                                break;
                            }
                        }
                    }
                }
                // Connection refused / TLS error — fall through to backoff and retry.
                Ok(Err(e)) => {
                    warn!(error = %e, "Failed to connect to Titan price level stream; retrying");
                    telemetry::record_reconnect(ReconnectReason::ConnectFailed);
                }
                // Handshake did not complete within the timeout — retry after backoff.
                Err(_elapsed) => {
                    warn!(
                        timeout_secs = settings.connect_timeout.as_secs(),
                        "Titan price level connect timed out; retrying"
                    );
                    telemetry::record_reconnect(ReconnectReason::ConnectTimeout);
                }
            }

            attempt = attempt.saturating_add(1);
            let backoff = backoff(attempt, settings.max_backoff);
            warn!(seconds = backoff.as_secs(), attempt, "Backing off before reconnecting to Titan");
            sleep(backoff).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr, sync::atomic::Ordering};

    use futures::SinkExt;
    use rstest::rstest;

    use super::{
        super::{
            telemetry::{
                recorded::{counter_value, record_async},
                FRAMES_REJECTED, RECONNECTS,
            },
            test_support::{
                frame_text, frame_then_repeat, wall_nanos_now, FakeTitan, PAMM, USDC, WBTC,
            },
        },
        *,
    };

    /// A frame captured verbatim from the live stream (2026-10-08); the file name carries the
    /// frame's `ts` field.
    const CAPTURED_MESSAGE: &str =
        include_str!("test_responses/pamm_price_levels_v2_1792115918290990905.json");

    /// Nanoseconds since the epoch for 2023-11 and 2033-05. A `ts` in seconds or milliseconds
    /// would fall far below the lower bound, so the window pins the scale rather than the date.
    const NANOS_LOWER_BOUND: u64 = 1_700_000_000_000_000_000;
    const NANOS_UPPER_BOUND: u64 = 2_000_000_000_000_000_000;

    #[test]
    fn parses_captured_live_message() {
        let message: TitanPriceLevelMessage =
            serde_json::from_str(CAPTURED_MESSAGE).expect("valid JSON");

        assert_eq!(message.block, 26_201_953);
        assert_eq!(message.slot, Some(15_440_991));
        assert!(
            (NANOS_LOWER_BOUND..NANOS_UPPER_BOUND).contains(&message.ts),
            "ts is not nanosecond-scale: {}",
            message.ts
        );

        // Every venue Titan builds with arrives in one flat book list.
        let venues: HashSet<&Bytes> = message
            .books
            .iter()
            .map(|book| &book.quote_address)
            .collect();
        assert!(venues.len() >= 2, "expected several venues, got {}", venues.len());

        let fermi = message
            .books
            .iter()
            .find(|book| {
                book.quote_address == Bytes::from_str(PAMM).unwrap() &&
                    book.token1 == "WBTC" &&
                    book.token2 == "USDC"
            })
            .expect("Fermi WBTC/USDC book");
        assert_eq!(fermi.maker.as_deref(), Some("Fermi"));
        assert!(!fermi.asks.is_empty());
        assert!(!fermi.bids.is_empty());

        // A level without volume carries no depth, so a zero would mean the ladder lost its
        // quantities in parsing.
        for book in &message.books {
            for level in book.asks.iter().chain(&book.bids) {
                assert!(level.cum_volume > 0.0, "non-positive cumVolume: {level:?}");
                assert!(level.price > 0.0, "non-positive price: {level:?}");
            }
        }
    }

    /// `ts` is the only per-frame freshness signal, so a frame without it is unusable and must
    /// not parse. `slot` carries no freshness decision, so a frame without it still parses.
    #[test]
    fn requires_ts_but_not_slot() {
        let without_ts = r#"{"slot": 1, "block": 2, "books": []}"#;
        assert!(serde_json::from_str::<TitanPriceLevelMessage>(without_ts).is_err());

        let without_slot = r#"{"block": 2, "ts": 3, "books": []}"#;
        let message: TitanPriceLevelMessage =
            serde_json::from_str(without_slot).expect("valid JSON");
        assert_eq!(message.slot, None);
    }

    /// `calculated_from` is a Titan-internal cursor that overflows `u64`, so the parser must
    /// ignore it rather than type it; `maker` is a display label the stream sometimes leaves
    /// empty or omits.
    #[rstest]
    #[case::named_maker(r#""maker": "Fermi","#, Some("Fermi"))]
    #[case::empty_maker(r#""maker": "","#, Some(""))]
    #[case::null_maker(r#""maker": null,"#, None)]
    #[case::absent_maker("", None)]
    fn tolerates_internal_fields_and_unlabelled_makers(
        #[case] maker: &str,
        #[case] expected: Option<&str>,
    ) {
        let json = format!(
            r#"{{
                {maker}
                "quoteAddress": "{PAMM}",
                "token1": "WBTC",
                "token2": "USDC",
                "token1Address": "{WBTC}",
                "token2Address": "{USDC}",
                "calculated_from": 283851543383726791766048878,
                "asks": [{{"price": 80997.89405475458, "cumVolume": 0.00012346}}],
                "bids": []
            }}"#
        );

        let book: TitanBook = serde_json::from_str(&json).expect("valid book");
        assert_eq!(book.maker.as_deref(), expected);
        assert_eq!(book.quote_address, Bytes::from_str(PAMM).unwrap());
        assert_eq!(book.asks.len(), 1);
        assert_eq!(book.asks[0].price, 80997.89405475458);
        assert_eq!(book.asks[0].cum_volume, 0.00012346);
        assert!(book.bids.is_empty());
    }

    #[test]
    fn backoff_grows_exponentially_up_to_the_cap() {
        let max_backoff = ConnectionSettings::default().max_backoff;
        assert_eq!(backoff(1, max_backoff), Duration::from_secs(2));
        assert_eq!(backoff(4, max_backoff), Duration::from_secs(16));
        assert_eq!(backoff(5, max_backoff), max_backoff);
        assert_eq!(backoff(100, max_backoff), max_backoff);
        // Exponent overflow must saturate to the cap rather than panic.
        assert_eq!(backoff(u32::MAX, max_backoff), max_backoff);
    }

    /// Settings that reconnect quickly enough for a test: a 100 ms idle timeout and a 10 ms
    /// backoff.
    fn fast_settings() -> ConnectionSettings {
        ConnectionSettings {
            connect_timeout: Duration::from_secs(1),
            read_idle_timeout: Duration::from_millis(100),
            max_backoff: Duration::from_millis(10),
        }
    }

    fn frame() -> Message {
        Message::Text(frame_text(100, wall_nanos_now()).into())
    }

    /// Polls `stream` until it yields a frame, failing if none arrives within two seconds.
    async fn next_frame(stream: &mut (impl Stream<Item = TitanPriceLevelMessage> + Unpin)) {
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("a frame within two seconds")
            .expect("the stream never ends");
    }

    /// Traffic that is not a parsed frame does not reset the idle timeout: the socket is
    /// dropped and reconnected even though it never went quiet.
    #[rstest]
    #[case::ping_only(Message::Ping(Vec::new().into()), false)]
    #[case::malformed_text(Message::Text("nonsense".into()), true)]
    fn non_frame_traffic_does_not_count_as_liveness(
        #[case] filler: Message,
        #[case] rejected_as_parse_error: bool,
    ) {
        let (connections, snapshot) = record_async(async {
            let fake =
                FakeTitan::spawn(frame_then_repeat(frame(), filler, Duration::from_millis(10)))
                    .await;
            let stream = messages(fake.url(), fast_settings());
            tokio::pin!(stream);
            next_frame(&mut stream).await;
            // The second frame can only come from a new connection, which the idle timeout
            // opens once the filler fails to reset it.
            next_frame(&mut stream).await;
            fake.connections.load(Ordering::SeqCst)
        });
        assert!(connections >= 2, "no reconnect on non-frame traffic");
        assert!(counter_value(&snapshot, RECONNECTS, &[("reason", "idle_timeout")]) >= 1);
        let parse_errors = counter_value(&snapshot, FRAMES_REJECTED, &[("reason", "parse_error")]);
        assert_eq!(parse_errors > 0, rejected_as_parse_error);
    }

    #[test]
    fn server_close_frame_reconnects() {
        let (connections, snapshot) = record_async(async {
            let fake = FakeTitan::spawn(|_, mut socket| async move {
                if socket.send(frame()).await.is_err() {
                    return;
                }
                let _ = socket.send(Message::Close(None)).await;
            })
            .await;
            let stream = messages(fake.url(), fast_settings());
            tokio::pin!(stream);
            next_frame(&mut stream).await;
            // The close frame follows the first frame, so the second frame can only come from a
            // new connection.
            next_frame(&mut stream).await;
            fake.connections.load(Ordering::SeqCst)
        });
        assert_eq!(connections, 2);
        assert_eq!(counter_value(&snapshot, RECONNECTS, &[("reason", "closed")]), 1);
    }

    /// The idle timeout measures upstream silence, not the consumer's pace: a consumer that
    /// pauses for longer than the timeout finds the same connection when it resumes.
    #[test]
    fn slow_consumer_does_not_trigger_the_idle_timeout() {
        let (connections, snapshot) = record_async(async {
            let fake =
                FakeTitan::spawn(frame_then_repeat(frame(), frame(), Duration::from_millis(20)))
                    .await;
            let stream = messages(fake.url(), fast_settings());
            tokio::pin!(stream);
            next_frame(&mut stream).await;
            // Three idle timeouts pass without a poll.
            tokio::time::sleep(Duration::from_millis(300)).await;
            next_frame(&mut stream).await;
            fake.connections.load(Ordering::SeqCst)
        });
        assert_eq!(connections, 1, "reconnected while the consumer was not polling");
        assert_eq!(counter_value(&snapshot, RECONNECTS, &[("reason", "idle_timeout")]), 0);
    }

    #[tokio::test]
    async fn parsed_frames_keep_the_connection_alive() {
        let fake =
            FakeTitan::spawn(frame_then_repeat(frame(), frame(), Duration::from_millis(30))).await;
        let settings =
            ConnectionSettings { read_idle_timeout: Duration::from_millis(150), ..fast_settings() };
        let stream = messages(fake.url(), settings);
        tokio::pin!(stream);
        for _ in 0..10 {
            next_frame(&mut stream).await;
        }
        assert_eq!(fake.connections.load(Ordering::SeqCst), 1);
    }
}
