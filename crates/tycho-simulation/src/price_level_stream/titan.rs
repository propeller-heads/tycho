//! Titan pAMM price level stream wire format and connection handling.
//!
//! Connects to the Titan `pamm_price_levels` WebSocket (see
//! <https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level>) and yields parsed frames.
//! Each frame is a complete snapshot of quote ladders per pair per pAMM, targeting the block
//! Titan is currently building; consumers keep the newest frame and treat older ones as
//! superseded.
//!
//! All Titan specifics (endpoint, JSON shape, reconnect policy) live in this module; the rest of
//! the price level stream machinery is venue-agnostic.

use std::time::Duration;

use async_stream::stream;
use futures::{Stream, StreamExt};
use num_bigint::BigUint;
use serde::{Deserialize, Deserializer};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};
use tycho_common::Bytes;

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
    /// Longest gap between Titan messages tolerated before the socket is treated as dead and
    /// re-established. Titan pushes several updates per second, so a multi-second silence means
    /// a stalled or half-open connection that [`StreamExt::next`] would otherwise wait on
    /// forever.
    pub read_idle_timeout: Duration,
    /// Cap on the exponential reconnect backoff (`2^attempt` seconds, at most this).
    pub max_backoff: Duration,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_idle_timeout: Duration::from_secs(30),
            max_backoff: Duration::from_secs(32),
        }
    }
}

/// The reconnect backoff after `attempt` consecutive failures: `2^attempt` seconds, capped at
/// `max_backoff`.
fn backoff(attempt: u32, max_backoff: Duration) -> Duration {
    let exponential = 2u64
        .checked_pow(attempt)
        .map(Duration::from_secs)
        .unwrap_or(Duration::MAX);
    exponential.min(max_backoff)
}

/// A parsed price level stream frame: a complete snapshot of all streamed pAMMs' quote ladders
/// for the block currently being built.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TitanPriceLevelMessage {
    /// The L1 block number the quotes target (the block currently being built).
    pub block_number: u64,
    /// Per-pAMM quote snapshots.
    pub pamms: Vec<TitanPammLevels>,
}

/// One pAMM's complete pair snapshot within a frame.
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
/// only once a frame is actually received, so a socket that connects and immediately drops still
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
                    loop {
                        // Bound the wait so a half-open connection (no data and no close frame)
                        // is detected and retried instead of blocking on `next()` indefinitely.
                        let message = match timeout(settings.read_idle_timeout, ws_stream.next())
                            .await
                        {
                            Ok(Some(message)) => message,
                            // Stream ended: the server hung up without sending a close frame.
                            Ok(None) => {
                                warn!("Titan price level stream ended; reconnecting");
                                break;
                            }
                            // No traffic within the idle window: assume a stalled socket.
                            Err(_elapsed) => {
                                warn!(
                                    idle_secs = settings.read_idle_timeout.as_secs(),
                                    "No Titan message within idle timeout; reconnecting"
                                );
                                break;
                            }
                        };

                        match message {
                            // Expected case: a JSON snapshot frame. Receiving one proves the
                            // connection is healthy, so reset the reconnect backoff.
                            Ok(Message::Text(text)) => {
                                attempt = 0;
                                match serde_json::from_str::<TitanPriceLevelMessage>(text.as_str())
                                {
                                    Ok(message) => yield message,
                                    // Unparseable frame: log and keep the connection.
                                    Err(e) => {
                                        warn!(error = %e, "Failed to parse Titan price level message")
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
                                break;
                            }
                            // Transport/protocol error (broken pipe, invalid frame, ...) —
                            // reconnect.
                            Err(e) => {
                                warn!(error = %e, "Titan price level stream read error; reconnecting");
                                break;
                            }
                        }
                    }
                }
                // Connection refused / TLS error — fall through to backoff and retry.
                Ok(Err(e)) => {
                    warn!(error = %e, "Failed to connect to Titan price level stream; retrying");
                }
                // Handshake did not complete within the timeout — retry after backoff.
                Err(_elapsed) => {
                    warn!(
                        timeout_secs = settings.connect_timeout.as_secs(),
                        "Titan price level connect timed out; retrying"
                    );
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
    use std::str::FromStr;

    use super::*;

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
}
