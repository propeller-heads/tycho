//! Ethereum `newHeads` notifications that trigger complete snapshots.

use ::http::Request;
use futures::{future::BoxFuture, stream::BoxStream, SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, Message},
    WebSocketStream,
};
use tycho_common::Bytes;

use crate::snapshot_feed::{
    errors::FeedError,
    failures::FailureTracker,
    ws::{connection_events, WsConnection, WsEvent, WsFeedConfig, WsPayload, WsSource},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcHeader {
    pub number: u64,
    pub hash: Bytes,
    pub timestamp: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HeadEvent {
    Connected,
    Head(RpcHeader),
    Reorg(RpcHeader),
}

pub fn head_events(
    ws_url: String,
    expected_chain_id: u64,
    config: &WsFeedConfig,
) -> BoxStream<'static, Result<HeadEvent, FeedError>> {
    let WsFeedConfig {
        connect_timeout,
        read_idle_timeout,
        backoff_unit,
        max_backoff_exp,
        max_consecutive_failures,
        max_snapshot_age: _,
    } = *config;
    if connect_timeout.is_zero() || read_idle_timeout.is_zero() || backoff_unit.is_zero() {
        return Box::pin(futures::stream::once(async {
            Err(FeedError::Fatal("EVM WebSocket durations must be positive".into()))
        }));
    }
    let failures = FailureTracker::new(max_consecutive_failures, backoff_unit, max_backoff_exp);
    let source = HeadSource { ws_url, expected_chain_id, subscription: None, previous: None };
    Box::pin(connection_events(connect_timeout, read_idle_timeout, failures, source).map(|event| {
        match event {
            Ok(WsEvent::Snapshot(head)) => Ok(head),
            Ok(WsEvent::Disconnected(error)) => Err(error),
            Err(error) => Err(FeedError::Fatal(error.to_string())),
        }
    }))
}

struct HeadSource {
    ws_url: String,
    expected_chain_id: u64,
    subscription: Option<String>,
    previous: Option<RpcHeader>,
}

impl WsSource for HeadSource {
    type Snapshot = HeadEvent;

    fn request(&self) -> Result<Request<()>, FeedError> {
        self.ws_url
            .as_str()
            .into_client_request()
            .map_err(|_error| FeedError::Connection("Invalid EVM WebSocket endpoint".into()))
    }

    fn on_connect<'a>(
        &'a mut self,
        socket: &'a mut WsConnection,
    ) -> BoxFuture<'a, Result<Option<HeadEvent>, FeedError>> {
        Box::pin(async move {
            send_json(
                socket,
                json!({"jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": []}),
            )
            .await?;
            let response = read_json_unbounded(socket).await?;
            let chain_id = parse_quantity(response_result(&response, 1)?, "chain ID")?;
            if chain_id != self.expected_chain_id {
                return Err(FeedError::Fatal(format!(
                    "EVM WebSocket endpoint must serve chain ID {}",
                    self.expected_chain_id
                )));
            }
            send_json(
            socket,
            json!({"jsonrpc": "2.0", "id": 2, "method": "eth_subscribe", "params": ["newHeads"]}),
        )
        .await?;
            let response = read_json_unbounded(socket).await?;
            let subscription = response_result(&response, 2)?
                .as_str()
                .filter(|subscription| !subscription.is_empty() && subscription.len() <= 256)
                .ok_or_else(|| FeedError::Parsing("Invalid EVM head subscription ID".into()))?;
            self.subscription = Some(subscription.to_owned());
            self.previous = None;
            Ok(Some(HeadEvent::Connected))
        })
    }

    fn idle_until_snapshot(&self) -> bool {
        true
    }

    fn transport_error(&self, _error: FeedError) -> FeedError {
        FeedError::Connection("EVM WebSocket connection unavailable".into())
    }

    fn decode(&mut self, payload: WsPayload) -> Result<Option<HeadEvent>, FeedError> {
        let WsPayload::Text(text) = payload else {
            return Err(FeedError::Parsing("EVM WebSocket requires JSON text frames".into()));
        };
        let value: Value = serde_json::from_str(text.as_str())
            .map_err(|_error| FeedError::Parsing("Invalid EVM WebSocket JSON".into()))?;
        let subscription = self
            .subscription
            .as_deref()
            .ok_or_else(|| FeedError::Parsing("EVM head subscription is not ready".into()))?;
        let header = parse_notification(&value, subscription)?;
        let reorg = self
            .previous
            .as_ref()
            .is_some_and(|last| header.number <= last.number && header != *last);
        self.previous = Some(header.clone());
        Ok(Some(if reorg { HeadEvent::Reorg(header) } else { HeadEvent::Head(header) }))
    }
}

async fn send_json<S>(socket: &mut WebSocketStream<S>, value: Value) -> Result<(), FeedError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .map_err(|_error| FeedError::Connection("EVM WebSocket request failed".into()))
}

async fn read_json_unbounded<S>(socket: &mut WebSocketStream<S>) -> Result<Value, FeedError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let message = socket
            .next()
            .await
            .ok_or_else(|| FeedError::Connection("EVM WebSocket stream ended".into()))?
            .map_err(|_error| FeedError::Connection("EVM WebSocket read failed".into()))?;
        match message {
            Message::Text(text) => {
                return serde_json::from_str(text.as_str())
                    .map_err(|_error| FeedError::Parsing("Invalid EVM WebSocket JSON".into()))
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => return Err(FeedError::Connection("EVM WebSocket closed".into())),
            Message::Binary(_) | Message::Frame(_) => {
                return Err(FeedError::Parsing("EVM WebSocket requires JSON text frames".into()))
            }
        }
    }
}

fn response_result(response: &Value, expected_id: u64) -> Result<&Value, FeedError> {
    if response.get("jsonrpc") != Some(&json!("2.0")) ||
        response.get("id") != Some(&json!(expected_id))
    {
        return Err(FeedError::Parsing("Invalid EVM WebSocket response envelope".into()));
    }
    if response.get("error").is_some() {
        return Err(FeedError::Connection("EVM WebSocket RPC request was rejected".into()));
    }
    response
        .get("result")
        .ok_or_else(|| FeedError::Parsing("Missing EVM WebSocket RPC result".into()))
}

fn parse_notification(notification: &Value, subscription: &str) -> Result<RpcHeader, FeedError> {
    if notification.get("jsonrpc") != Some(&json!("2.0")) ||
        notification.get("method") != Some(&json!("eth_subscription")) ||
        notification.get("id").is_some() ||
        notification.get("error").is_some()
    {
        return Err(FeedError::Parsing("Invalid EVM head notification envelope".into()));
    }
    let params = notification
        .get("params")
        .ok_or_else(|| FeedError::Parsing("Missing EVM head notification params".into()))?;
    if params
        .get("subscription")
        .and_then(Value::as_str) !=
        Some(subscription)
    {
        return Err(FeedError::Parsing("Unexpected EVM head subscription ID".into()));
    }
    let header = params
        .get("result")
        .ok_or_else(|| FeedError::Parsing("Missing EVM head notification result".into()))?;
    let number = parse_quantity(&header["number"], "head number")?;
    let timestamp = parse_quantity(&header["timestamp"], "head timestamp")?;
    let hash = header["hash"]
        .as_str()
        .and_then(|hash| hash.strip_prefix("0x"))
        .filter(|digits| digits.len() == 64)
        .and_then(|digits| hex::decode(digits).ok())
        .ok_or_else(|| FeedError::Parsing("Invalid EVM head hash".into()))?;
    Ok(RpcHeader { number, hash: Bytes::from(hash), timestamp })
}

fn parse_quantity(value: &Value, name: &str) -> Result<u64, FeedError> {
    let digits = value
        .as_str()
        .and_then(|value| value.strip_prefix("0x"))
        .filter(|digits| {
            !digits.is_empty() &&
                digits
                    .bytes()
                    .all(|digit| digit.is_ascii_hexdigit()) &&
                (digits.len() == 1 || !digits.starts_with('0'))
        })
        .ok_or_else(|| FeedError::Parsing(format!("Invalid EVM {name} quantity")))?;
    u64::from_str_radix(digits, 16)
        .map_err(|_quantity| FeedError::Parsing(format!("Invalid EVM {name} quantity")))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        net::TcpListener,
        task::JoinHandle,
        time::{sleep, timeout},
    };
    use tokio_tungstenite::accept_async;

    use super::*;

    const SUBSCRIPTION: &str = "0xsubscription";

    fn notification() -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "eth_subscription",
            "params": {
                "subscription": SUBSCRIPTION,
                "result": {"number": "0x42", "hash": format!("0x{}", "ab".repeat(32)), "timestamp": "0x1234"},
            },
        })
    }

    async fn server() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let url = format!("ws://{}/secret-provider-key", listener.local_addr().unwrap());
        (listener, url)
    }

    async fn acknowledge<S>(socket: &mut WebSocketStream<S>)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        acknowledge_chain(socket, 8453).await;
    }

    async fn acknowledge_chain<S>(socket: &mut WebSocketStream<S>, chain_id: u64)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        assert_eq!(
            read_json_unbounded(socket)
                .await
                .unwrap(),
            json!({"jsonrpc": "2.0", "id": 1, "method": "eth_chainId", "params": []}),
        );
        send_json(socket, json!({"jsonrpc": "2.0", "id": 1, "result": format!("0x{chain_id:x}")}))
            .await
            .unwrap();
        assert_eq!(
            read_json_unbounded(socket)
                .await
                .unwrap(),
            json!({"jsonrpc": "2.0", "id": 2, "method": "eth_subscribe", "params": ["newHeads"]}),
        );
        send_json(socket, json!({"jsonrpc": "2.0", "id": 2, "result": SUBSCRIPTION}))
            .await
            .unwrap();
    }

    fn events(url: String) -> BoxStream<'static, Result<HeadEvent, FeedError>> {
        head_events(
            url,
            8453,
            &WsFeedConfig {
                connect_timeout: Duration::from_secs(1),
                read_idle_timeout: Duration::from_secs(1),
                backoff_unit: Duration::from_millis(5),
                ..crate::snapshot_feed::ws::default_ws_feed_config()
            },
        )
    }

    async fn finish(task: JoinHandle<()>) {
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn new_head_arrives_immediately_without_polling_interval() {
        let (listener, url) = server().await;
        let task = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            acknowledge(&mut socket).await;
            send_json(&mut socket, notification())
                .await
                .unwrap();
        });
        let mut stream = events(url);
        assert_eq!(stream.next().await.unwrap().unwrap(), HeadEvent::Connected);
        let HeadEvent::Head(header) = timeout(Duration::from_millis(250), stream.next())
            .await
            .expect("head must arrive without waiting for a two-second polling tick")
            .unwrap()
            .unwrap()
        else {
            panic!("expected a block head");
        };
        assert_eq!(header.number, 66);
        assert_eq!(header.hash, Bytes::from(vec![0xab; 32]));
        assert_eq!(header.timestamp, 0x1234);
        finish(task).await;
    }

    #[tokio::test]
    async fn replacement_heads_invalidate_before_fetching_the_new_head() {
        for replacement_number in ["0x42", "0x41"] {
            let (listener, url) = server().await;
            let task = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = accept_async(stream).await.unwrap();
                acknowledge(&mut socket).await;
                send_json(&mut socket, notification())
                    .await
                    .unwrap();
                let mut replacement = notification();
                replacement["params"]["result"]["number"] = json!(replacement_number);
                replacement["params"]["result"]["hash"] = json!(format!("0x{}", "cd".repeat(32)));
                send_json(&mut socket, replacement)
                    .await
                    .unwrap();
            });
            let mut stream = events(url);
            assert_eq!(stream.next().await.unwrap().unwrap(), HeadEvent::Connected);
            assert!(matches!(stream.next().await.unwrap().unwrap(), HeadEvent::Head(_)));
            let HeadEvent::Reorg(replacement) = stream.next().await.unwrap().unwrap() else {
                panic!("replacement head missing")
            };
            assert_eq!(replacement.hash, Bytes::from(vec![0xcd; 32]));
            finish(task).await;
        }
    }

    #[tokio::test]
    async fn reconnect_emits_recovery_marker_and_resubscribes() {
        let (listener, url) = server().await;
        let task = tokio::spawn(async move {
            let (first, _peer) = listener.accept().await.unwrap();
            let mut first = accept_async(first).await.unwrap();
            acknowledge(&mut first).await;
            first.close(None).await.unwrap();
            let (second, _peer) = listener.accept().await.unwrap();
            let mut second = accept_async(second).await.unwrap();
            acknowledge(&mut second).await;
            send_json(&mut second, notification())
                .await
                .unwrap();
        });
        let mut stream = events(url);
        assert_eq!(stream.next().await.unwrap().unwrap(), HeadEvent::Connected);
        assert!(matches!(stream.next().await.unwrap(), Err(FeedError::Connection(_))));
        assert_eq!(stream.next().await.unwrap().unwrap(), HeadEvent::Connected);
        assert!(matches!(stream.next().await.unwrap().unwrap(), HeadEvent::Head(_)));
        finish(task).await;
    }

    #[tokio::test]
    async fn subscriptions_without_heads_do_not_reset_the_failure_budget() {
        let (listener, url) = server().await;
        let task = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = accept_async(stream).await.unwrap();
                acknowledge(&mut socket).await;
                socket.close(None).await.unwrap();
            }
        });
        let mut stream = head_events(
            url,
            8453,
            &WsFeedConfig {
                connect_timeout: Duration::from_secs(1),
                max_consecutive_failures: Some(2),
                backoff_unit: Duration::from_millis(1),
                ..crate::snapshot_feed::ws::default_ws_feed_config()
            },
        );
        assert_eq!(stream.next().await.unwrap().unwrap(), HeadEvent::Connected);
        assert!(matches!(stream.next().await.unwrap(), Err(FeedError::Connection(_))));
        assert_eq!(stream.next().await.unwrap().unwrap(), HeadEvent::Connected);
        assert!(matches!(stream.next().await.unwrap(), Err(FeedError::Fatal(_))));
        assert!(stream.next().await.is_none());
        finish(task).await;
    }

    #[tokio::test]
    async fn subscription_setup_shares_the_connection_deadline() {
        let (listener, url) = server().await;
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            read_json_unbounded(&mut socket)
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let mut stream = head_events(
            url,
            8453,
            &WsFeedConfig {
                connect_timeout: Duration::from_millis(20),
                max_consecutive_failures: Some(1),
                ..crate::snapshot_feed::ws::default_ws_feed_config()
            },
        );
        let error = timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("connect timed out"));
        task.abort();
    }

    #[tokio::test]
    async fn wrong_chain_is_rejected_before_subscribing() {
        for expected in [8453, 56, 4663] {
            let (listener, url) = server().await;
            let task = tokio::spawn(async move {
                let (stream, _peer) = listener.accept().await.unwrap();
                let mut socket = accept_async(stream).await.unwrap();
                assert_eq!(
                    read_json_unbounded(&mut socket)
                        .await
                        .unwrap()["method"],
                    "eth_chainId"
                );
                let wrong = if expected == 8453 { 56 } else { 8453 };
                send_json(
                    &mut socket,
                    json!({"jsonrpc": "2.0", "id": 1, "result": format!("0x{wrong:x}")}),
                )
                .await
                .unwrap();
            });
            let mut stream = head_events(
                url,
                expected,
                &WsFeedConfig {
                    connect_timeout: Duration::from_secs(1),
                    read_idle_timeout: Duration::from_secs(1),
                    backoff_unit: Duration::from_millis(5),
                    ..crate::snapshot_feed::ws::default_ws_feed_config()
                },
            );
            let error = stream
                .next()
                .await
                .unwrap()
                .unwrap_err();
            assert!(matches!(error, FeedError::Fatal(_)));
            assert!(error
                .to_string()
                .contains(&expected.to_string()));
            finish(task).await;
        }
    }

    #[tokio::test]
    async fn each_configured_chain_can_subscribe_to_its_own_heads() {
        for chain_id in [8453, 56, 4663] {
            let (listener, url) = server().await;
            let task = tokio::spawn(async move {
                let (stream, _peer) = listener.accept().await.unwrap();
                let mut socket = accept_async(stream).await.unwrap();
                acknowledge_chain(&mut socket, chain_id).await;
                send_json(&mut socket, notification())
                    .await
                    .unwrap();
            });
            let mut stream = head_events(
                url,
                chain_id,
                &WsFeedConfig {
                    connect_timeout: Duration::from_secs(1),
                    read_idle_timeout: Duration::from_secs(1),
                    backoff_unit: Duration::from_millis(5),
                    ..crate::snapshot_feed::ws::default_ws_feed_config()
                },
            );
            assert_eq!(stream.next().await.unwrap().unwrap(), HeadEvent::Connected);
            assert!(matches!(stream.next().await.unwrap().unwrap(), HeadEvent::Head(_)));
            finish(task).await;
        }
    }

    #[tokio::test]
    async fn rejected_rpc_does_not_leak_provider_error_or_url() {
        let (listener, url) = server().await;
        let task = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            read_json_unbounded(&mut socket)
                .await
                .unwrap();
            send_json(
                &mut socket,
                json!({"jsonrpc": "2.0", "id": 1, "error": {"message": "secret-provider-key"}}),
            )
            .await
            .unwrap();
        });
        let mut stream = events(url);
        let error = stream
            .next()
            .await
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(error.contains("RPC request was rejected"));
        assert!(!error.contains("secret-provider-key"));
        finish(task).await;
    }

    #[tokio::test]
    async fn answers_ping_and_keeps_head_subscription() {
        let (listener, url) = server().await;
        let task = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            acknowledge(&mut socket).await;
            socket
                .send(Message::Ping(vec![1, 2, 3].into()))
                .await
                .unwrap();
            assert_eq!(socket.next().await.unwrap().unwrap(), Message::Pong(vec![1, 2, 3].into()));
            send_json(&mut socket, notification())
                .await
                .unwrap();
        });
        let mut stream = events(url);
        assert_eq!(stream.next().await.unwrap().unwrap(), HeadEvent::Connected);
        assert!(matches!(stream.next().await.unwrap().unwrap(), HeadEvent::Head(_)));
        finish(task).await;
    }

    #[tokio::test]
    async fn ping_traffic_does_not_extend_head_idle_timeout() {
        let (listener, url) = server().await;
        let task = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            acknowledge(&mut socket).await;
            loop {
                if socket
                    .send(Message::Ping(vec![1].into()))
                    .await
                    .is_err()
                {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        });
        let mut stream = head_events(
            url,
            8453,
            &WsFeedConfig {
                connect_timeout: Duration::from_secs(1),
                read_idle_timeout: Duration::from_millis(30),
                backoff_unit: Duration::from_millis(5),
                ..crate::snapshot_feed::ws::default_ws_feed_config()
            },
        );
        assert_eq!(stream.next().await.unwrap().unwrap(), HeadEvent::Connected);
        let error = timeout(Duration::from_millis(250), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("read_idle_timeout"));
        task.abort();
    }

    #[test]
    fn malformed_or_unrelated_notifications_are_rejected() {
        let original = notification();
        for (pointer, value) in [
            ("/jsonrpc", json!("1.0")),
            ("/method", json!("some_other_subscription")),
            ("/params/subscription", json!("another-subscription")),
            ("/params/result/number", json!("0x00")),
            ("/params/result/number", json!("0x+42")),
            ("/params/result/number", json!("0x10000000000000000")),
            ("/params/result/number", json!(66)),
            ("/params/result/hash", json!("0xab")),
            ("/params/result/hash", json!(format!("0x{}", "zz".repeat(32)))),
            ("/params/result/timestamp", json!(null)),
        ] {
            let mut invalid = original.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            assert!(parse_notification(&invalid, SUBSCRIPTION).is_err(), "accepted {pointer}");
        }
        let mut invalid = original.clone();
        invalid["id"] = json!(1);
        assert!(parse_notification(&invalid, SUBSCRIPTION).is_err());
        let mut invalid = original;
        invalid["error"] = json!({"message": "provider-secret"});
        assert!(parse_notification(&invalid, SUBSCRIPTION).is_err());
    }

    #[test]
    fn responses_require_matching_request_id_and_jsonrpc_version() {
        for response in [
            json!({"jsonrpc": "1.0", "id": 1, "result": "0x2105"}),
            json!({"jsonrpc": "2.0", "id": 2, "result": "0x2105"}),
            json!({"jsonrpc": "2.0", "id": "1", "result": "0x2105"}),
            json!({"jsonrpc": "2.0", "id": 1}),
        ] {
            assert!(response_result(&response, 1).is_err());
        }
    }
    #[tokio::test(start_paused = true)]
    async fn configured_connection_failure_budget_ends_the_feed_with_an_error() {
        use crate::snapshot_feed::{
            http::default_http_feed_config, trigger::run_triggered_feed, Publisher,
        };
        let events = head_events(
            "invalid://no-websocket".into(),
            8453,
            &WsFeedConfig {
                max_consecutive_failures: Some(2),
                backoff_unit: Duration::from_secs(1),
                ..crate::snapshot_feed::ws::default_ws_feed_config()
            },
        )
        .map(|event| event.map(|_| crate::snapshot_feed::trigger::TriggerEvent::Fetch));
        let (publisher, snapshots) = Publisher::<u32>::channel();
        let result = run_triggered_feed(
            default_http_feed_config(),
            publisher,
            || async { Err::<u32, _>(FeedError::Parsing("unexpected fetch".into())) },
            events,
        )
        .await;
        let error = result.expect_err("a bounded connection failure must fail, not run out");
        assert!(matches!(error, FeedError::Fatal(_)));
        assert!(error
            .to_string()
            .contains("after 2 consecutive failures"));
        assert!(snapshots.borrow().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_backoff_exponent_keeps_reconnects_at_the_configured_interval() {
        let mut events = head_events(
            "invalid://no-websocket".into(),
            8453,
            &WsFeedConfig {
                max_consecutive_failures: Some(4),
                backoff_unit: Duration::from_secs(2),
                max_backoff_exp: 0,
                ..crate::snapshot_feed::ws::default_ws_feed_config()
            },
        );
        let started = tokio::time::Instant::now();
        for seconds in [0, 2, 4] {
            assert!(matches!(events.next().await.unwrap(), Err(FeedError::Connection(_))));
            assert_eq!(started.elapsed(), Duration::from_secs(seconds));
        }
        assert!(matches!(events.next().await.unwrap(), Err(FeedError::Fatal(_))));
        assert_eq!(started.elapsed(), Duration::from_secs(6));
        assert!(events.next().await.is_none());
    }
}
