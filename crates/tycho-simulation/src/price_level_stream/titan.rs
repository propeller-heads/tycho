//! Titan pAMM price level stream wire format and connection handling.
//!
//! Connects to the Titan `pamm_price_levels` WebSocket (see
//! <https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level>) and yields parsed frames.
//! Each frame carries the quote ladders Titan simulated in one build round, targeting the block
//! it is currently building. Consumers decide how fresh a component is from the frame
//! `timestamp`, not from what a frame omits.
//!
//! All Titan specifics (endpoint, JSON shape, reconnect policy) live in this module; the rest of
//! the price level stream machinery is venue-agnostic.

use std::time::{Duration, Instant};

use async_stream::stream;
use futures::{Stream, StreamExt};
use num_bigint::BigUint;
use serde::{Deserialize, Deserializer};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};
use tycho_common::Bytes;

use super::telemetry;

/// Default Titan pAMM price level WebSocket endpoint. Titan serves the same stream from other
/// regions as well; see <https://docs.titanbuilder.xyz/propamms/takers>.
pub(super) const TITAN_PRICE_LEVEL_URL: &str = "wss://eu.rpc.titanbuilder.xyz/ws/pamm_price_levels";

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
pub(super) fn backoff(attempt: u32, max_backoff: Duration) -> Duration {
    let exponential = 2u64
        .checked_pow(attempt)
        .map(Duration::from_secs)
        .unwrap_or(Duration::MAX);
    exponential.min(max_backoff)
}

/// A parsed price level stream frame: the quote ladders of every pAMM Titan simulated in one
/// build round, targeting the block currently being built.
///
/// Frames are best effort, not complete snapshots: a venue or pair can be absent from one frame
/// and present in the next (observed on 7.7% of frames in a 15 minute capture), so absence must
/// never be read as retirement.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitanPriceLevelMessage {
    /// The L1 block number the quotes target (the block currently being built).
    pub block_number: u64,
    /// When Titan built this frame, in nanoseconds since the Unix epoch. Frames re-emitted
    /// within one build round share a timestamp, so it is a freshness marker, not an identity.
    pub timestamp: u64,
    /// Per-pAMM quote ladders.
    pub pamms: Vec<TitanPammLevels>,
}

/// One pAMM's quote ladders within a frame. A frame can omit pairs the venue trades.
#[derive(Debug, Deserialize)]
pub(super) struct TitanPammLevels {
    /// The pAMM venue address.
    pub pamm: Bytes,
    pub pairs: Vec<TitanPairLevels>,
}

/// The quote ladder of one trade direction of one pair.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitanPairLevels {
    pub token_in: Bytes,
    pub token_out: Bytes,
    pub order_book: Vec<TitanPriceLevel>,
}

/// A single quote: swapping exactly `amount_in` delivers `amount_out` in total.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitanPriceLevel {
    #[serde(deserialize_with = "quantity")]
    pub amount_in: BigUint,
    #[serde(deserialize_with = "quantity")]
    pub amount_out: BigUint,
}

/// Deserializes a JSON quantity into a `BigUint`, accepting only the documented wire format:
/// `0x`-prefixed hex strings.
fn quantity<'de, D>(deserializer: D) -> Result<BigUint, D::Error>
where
    D: Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    let digits = text.strip_prefix("0x").ok_or_else(|| {
        serde::de::Error::custom(format!("quantity must be a 0x-prefixed hex string: {text}"))
    })?;
    BigUint::parse_bytes(digits.as_bytes(), 16)
        .ok_or_else(|| serde::de::Error::custom(format!("invalid quantity: {text}")))
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
                    let mut last_parsed = Instant::now();
                    loop {
                        // The idle timeout counts from the last parsed frame, so control frames
                        // and unparsable text cannot keep a socket that sends no frames alive.
                        let remaining = settings
                            .read_idle_timeout
                            .saturating_sub(last_parsed.elapsed());
                        if remaining.is_zero() {
                            warn!(
                                idle_secs = settings.read_idle_timeout.as_secs(),
                                "No parsed Titan frame within idle timeout; reconnecting"
                            );
                            telemetry::record_reconnect("idle_timeout");
                            break;
                        }
                        let message = match timeout(remaining, ws_stream.next()).await {
                            Ok(Some(message)) => message,
                            // Stream ended: the server hung up without sending a close frame.
                            Ok(None) => {
                                warn!("Titan price level stream ended; reconnecting");
                                telemetry::record_reconnect("ended");
                                break;
                            }
                            // No parsed frame within the idle window: assume a stalled socket.
                            Err(_elapsed) => {
                                warn!(
                                    idle_secs = settings.read_idle_timeout.as_secs(),
                                    "No parsed Titan frame within idle timeout; reconnecting"
                                );
                                telemetry::record_reconnect("idle_timeout");
                                break;
                            }
                        };

                        match message {
                            Ok(Message::Text(text)) => {
                                match serde_json::from_str::<TitanPriceLevelMessage>(text.as_str())
                                {
                                    // A parsed frame proves the connection is healthy: reset
                                    // both the reconnect backoff and the idle timeout.
                                    Ok(message) => {
                                        attempt = 0;
                                        last_parsed = Instant::now();
                                        yield message;
                                    }
                                    // Unparseable frame: log and keep the connection.
                                    Err(e) => {
                                        warn!(error = %e, "Failed to parse Titan price level message");
                                        telemetry::record_frame_rejected("parse_error");
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
                                telemetry::record_reconnect("closed");
                                break;
                            }
                            // Transport/protocol error (broken pipe, invalid frame, ...) —
                            // reconnect.
                            Err(e) => {
                                warn!(error = %e, "Titan price level stream read error; reconnecting");
                                telemetry::record_reconnect("read_error");
                                break;
                            }
                        }
                    }
                }
                // Connection refused / TLS error — fall through to backoff and retry.
                Ok(Err(e)) => {
                    warn!(error = %e, "Failed to connect to Titan price level stream; retrying");
                    telemetry::record_reconnect("connect_failed");
                }
                // Handshake did not complete within the timeout — retry after backoff.
                Err(_elapsed) => {
                    warn!(
                        timeout_secs = settings.connect_timeout.as_secs(),
                        "Titan price level connect timed out; retrying"
                    );
                    telemetry::record_reconnect("connect_timeout");
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
    use std::{str::FromStr, sync::atomic::Ordering};

    use futures::SinkExt;

    use super::{
        super::{
            telemetry::{
                recorded::{counter_value, record_async},
                FRAMES_REJECTED, RECONNECTS,
            },
            test_support::{frame_text, frame_then_repeat, wall_nanos_now, FakeTitan},
        },
        *,
    };

    /// Sample message from the Titan docs
    /// (<https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level>).
    const SAMPLE_MESSAGE: &str = r#"{
        "slot": 14581462,
        "blockNumber": 25345763,
        "timestamp": 1781801564588230787,
        "pamms": [
            {
                "pamm": "0x5979458912f80b96d30d4220af8e2e4925a33320",
                "pairs": [
                    {
                        "tokenIn": "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599",
                        "tokenOut": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                        "orderBook": [
                            {
                                "amountIn": "0x989680",
                                "amountOut": "0x174b67393",
                                "variant": "Simulated"
                            },
                            {
                                "amountIn": "0x1312d00",
                                "amountOut": "0x2e968e726",
                                "variant": "Interpolated"
                            }
                        ]
                    }
                ]
            }
        ]
    }"#;

    #[test]
    fn parses_documented_sample_message() {
        let message: TitanPriceLevelMessage = serde_json::from_str(SAMPLE_MESSAGE).unwrap();
        assert_eq!(message.block_number, 25345763);
        assert_eq!(message.timestamp, 1781801564588230787);
        assert_eq!(message.pamms.len(), 1);

        let pamm = &message.pamms[0];
        assert_eq!(
            pamm.pamm,
            Bytes::from_str("0x5979458912f80b96d30d4220af8e2e4925a33320").unwrap()
        );
        assert_eq!(pamm.pairs.len(), 1);

        let pair = &pamm.pairs[0];
        assert_eq!(
            pair.token_in,
            Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap()
        );
        assert_eq!(pair.order_book.len(), 2);
        assert_eq!(pair.order_book[0].amount_in, BigUint::from(0x989680u64));
        assert_eq!(pair.order_book[0].amount_out, BigUint::from(0x174b67393u64));
    }

    /// The wire `timestamp` is the only per-frame freshness signal, so a frame without it is
    /// unusable and must not parse.
    #[test]
    fn rejects_frame_without_timestamp() {
        let json = r#"{"slot": 1, "blockNumber": 2, "pamms": []}"#;
        assert!(serde_json::from_str::<TitanPriceLevelMessage>(json).is_err());
    }

    #[test]
    fn rejects_quantities_that_are_not_hex_strings() {
        for json in [
            r#"{"amountIn": "0xzz", "amountOut": "0x1"}"#,
            r#"{"amountIn": "0x", "amountOut": "0x1"}"#,
            r#"{"amountIn": "1000", "amountOut": "0x1"}"#,
            r#"{"amountIn": 1000, "amountOut": "0x1"}"#,
        ] {
            assert!(serde_json::from_str::<TitanPriceLevel>(json).is_err(), "accepted: {json}");
        }
    }

    /// A frame captured verbatim from the live stream (2026-07-15); the file name carries the
    /// frame's `timestamp` field.
    const CAPTURED_MESSAGE: &str =
        include_str!("test_responses/pamm_price_levels_1784126589047308938.json");

    #[test]
    fn parses_captured_live_message() {
        let message: TitanPriceLevelMessage =
            serde_json::from_str(CAPTURED_MESSAGE).expect("valid JSON");

        assert_eq!(message.block_number, 25538727);
        assert_eq!(message.pamms.len(), 2);

        // FermiSwapper router and KipseliPropAMMWrapper router, the venue keys observed live.
        let fermiswap = &message.pamms[0];
        assert_eq!(
            fermiswap.pamm,
            Bytes::from_str("0x5979458912f80b96d30d4220af8e2e4925a33320").unwrap()
        );
        assert_eq!(fermiswap.pairs.len(), 16);
        let kipseli = &message.pamms[1];
        assert_eq!(
            kipseli.pamm,
            Bytes::from_str("0x71e790dd841c8a9061487cb3e78c288e75ce0b3d").unwrap()
        );
        assert_eq!(kipseli.pairs.len(), 4);

        // First level of the first ladder (WBTC -> USDC): 0xc350 -> 0x1f27427.
        let first = &fermiswap.pairs[0];
        assert_eq!(
            first.token_in,
            Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap()
        );
        assert_eq!(first.order_book[0].amount_in, BigUint::from(0xc350u64));
        assert_eq!(first.order_book[0].amount_out, BigUint::from(0x1f27427u64));

        // Every ladder is non-trivial and every quantity parsed to a positive amount.
        for pamm in &message.pamms {
            for pair in &pamm.pairs {
                assert!(pair.order_book.len() >= 64, "unexpectedly short ladder");
                for level in &pair.order_book {
                    assert!(level.amount_in > BigUint::ZERO);
                    assert!(level.amount_out > BigUint::ZERO);
                }
            }
        }
    }

    /// A frame captured verbatim from the live stream (2026-09-05). It carries `timestamp`,
    /// `slot`, and an undocumented per-venue `maker` field the parser must ignore.
    const CAPTURED_MESSAGE_2026_09_05: &str =
        include_str!("test_responses/pamm_price_levels_1788624558231060482.json");

    #[test]
    fn parses_captured_live_message_with_timestamp_and_extra_fields() {
        let message: TitanPriceLevelMessage =
            serde_json::from_str(CAPTURED_MESSAGE_2026_09_05).expect("valid JSON");
        assert_eq!(message.timestamp, 1788624558231060482);
        assert_eq!(message.block_number, 25912232);
        assert_eq!(message.pamms.len(), 7);
        let fermiswap = message
            .pamms
            .iter()
            .find(|pamm| {
                pamm.pamm == Bytes::from_str("0x5979458912f80b96d30d4220af8e2e4925a33320").unwrap()
            })
            .expect("fermiswap present");
        assert_eq!(fermiswap.pairs.len(), 12);
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

    #[test]
    fn ping_only_traffic_does_not_count_as_liveness() {
        let (connections, snapshot) = record_async(async {
            let fake = FakeTitan::spawn(frame_then_repeat(
                frame(),
                Message::Ping(Vec::new().into()),
                Duration::from_millis(10),
            ))
            .await;
            let stream = messages(fake.url(), fast_settings());
            tokio::pin!(stream);
            next_frame(&mut stream).await;
            // The second frame can only come from a new connection, which the idle timeout
            // opens once the pings fail to reset it.
            next_frame(&mut stream).await;
            fake.connections.load(Ordering::SeqCst)
        });
        assert!(connections >= 2, "no reconnect on ping-only traffic");
        assert!(counter_value(&snapshot, RECONNECTS, &[("reason", "idle_timeout")]) >= 1);
    }

    #[test]
    fn malformed_text_does_not_count_as_liveness() {
        let (connections, snapshot) = record_async(async {
            let fake = FakeTitan::spawn(frame_then_repeat(
                frame(),
                Message::Text("nonsense".into()),
                Duration::from_millis(10),
            ))
            .await;
            let stream = messages(fake.url(), fast_settings());
            tokio::pin!(stream);
            next_frame(&mut stream).await;
            // The second frame can only come from a new connection, which the idle timeout
            // opens once the malformed text fails to reset it.
            next_frame(&mut stream).await;
            fake.connections.load(Ordering::SeqCst)
        });
        assert!(connections >= 2, "no reconnect on malformed text");
        assert!(counter_value(&snapshot, RECONNECTS, &[("reason", "idle_timeout")]) >= 1);
        assert!(counter_value(&snapshot, FRAMES_REJECTED, &[("reason", "parse_error")]) >= 1);
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
