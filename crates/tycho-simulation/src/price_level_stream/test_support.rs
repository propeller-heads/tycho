//! Helpers shared by the price level stream tests.

use std::{
    collections::HashMap,
    future::Future,
    net::SocketAddr,
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures::{future::BoxFuture, SinkExt};
use num_bigint::BigUint;
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use super::{
    config::PriceLevelStreamConfig,
    titan::{TitanBook, TitanOverrideMessage, TitanPriceLevelMessage},
};

/// The FermiSwapper router, one of the default venues.
pub(super) const PAMM: &str = "0x5979458912f80b96d30d4220af8e2e4925a33320";
/// The KipseliPropAMMWrapper router: Kipseli's canonical public identity, and one of the default
/// venues.
pub(super) const KIPSELI: &str = "0x71e790dd841c8a9061487cb3e78c288e75ce0b3d";
/// The address Titan streams Kipseli's books under, aliased onto [`KIPSELI`].
pub(super) const KIPSELI_STREAMED: &str = "0xc9a956b5196dfe110debe8781857bc8b97d32091";
pub(super) const WBTC: &str = "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599";
pub(super) const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
pub(super) const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
pub(super) const USDT: &str = "0xdac17f958d2ee523a2206206994597c13d831ec7";
pub(super) const CBBTC: &str = "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf";

pub(super) fn address(hex: &str) -> Bytes {
    Bytes::from_str(hex).expect("hex address")
}

pub(super) fn token(address: &str, symbol: &str, decimals: u32) -> Token {
    Token::new(
        &Bytes::from_str(address).unwrap(),
        symbol,
        decimals,
        0,
        &[Some(10_000)],
        Chain::Ethereum,
        100,
    )
}

pub(super) fn token_map(tokens: impl IntoIterator<Item = Token>) -> HashMap<Bytes, Token> {
    tokens
        .into_iter()
        .map(|token| (token.address.clone(), token))
        .collect()
}

/// The bound assets of the streamed symbols the tests price, with the decimals of their Ethereum
/// deployments, as a consumer's token metadata.
pub(super) fn tokens() -> HashMap<Bytes, Token> {
    token_map([token(WBTC, "WBTC", 8), token(USDC, "USDC", 6), token(WETH, "WETH", 18)])
}

/// Every bound asset the 2026-10-08 captures quote, as a consumer's token metadata: [`tokens`]
/// plus USDT and cbBTC, so no captured book goes unserved for want of a token the consumer admits.
/// `NVDAx`, the one other symbol the capture quotes, is bound by nothing and stays unserved.
pub(super) fn captured_tokens() -> HashMap<Bytes, Token> {
    token_map([
        token(WBTC, "WBTC", 8),
        token(USDC, "USDC", 6),
        token(WETH, "WETH", 18),
        token(USDT, "USDT", 6),
        token(CBBTC, "cbBTC", 8),
    ])
}

pub(super) type FakeConnection = WebSocketStream<TcpStream>;

/// A scripted stand-in for Titan's WebSocket endpoint on `127.0.0.1`. Every accepted
/// connection runs `handler(connection_index, socket)`.
pub(super) struct FakeTitan {
    addr: SocketAddr,
    pub(super) connections: Arc<AtomicUsize>,
    accept_task: Option<JoinHandle<()>>,
}

impl FakeTitan {
    pub(super) async fn spawn<H, Fut>(handler: H) -> Self
    where
        H: Fn(usize, FakeConnection) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener
            .local_addr()
            .expect("local addr");
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = connections.clone();
        let handler: Arc<dyn Fn(usize, FakeConnection) -> BoxFuture<'static, ()> + Send + Sync> =
            Arc::new(move |index, socket| Box::pin(handler(index, socket)));
        let accept_task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let index = counter.fetch_add(1, Ordering::SeqCst);
                let handler = handler.clone();
                tokio::spawn(async move {
                    let Ok(socket) = tokio_tungstenite::accept_async(socket).await else {
                        return;
                    };
                    handler(index, socket).await;
                });
            }
        });
        Self { addr, connections, accept_task: Some(accept_task) }
    }

    /// A fake that accepts TCP connections and never completes the WebSocket handshake, so a
    /// client's connect hangs until its own [`connect_timeout`] fires.
    ///
    /// [`connect_timeout`]: super::stream::PriceLevelStreamBuilder::connect_timeout
    pub(super) async fn spawn_hung() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener
            .local_addr()
            .expect("local addr");
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = connections.clone();
        let accept_task = tokio::spawn(async move {
            let mut accepted = Vec::new();
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                // Held rather than answered: dropping the socket would let the client's connect
                // fail at once instead of hanging, which is the opposite of the case.
                accepted.push(socket);
            }
        });
        Self { addr, connections, accept_task: Some(accept_task) }
    }

    pub(super) fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Stops accepting: the listener is dropped, so every later connect is refused at TCP
    /// level. Connections already handed to a handler keep running.
    pub(super) fn shutdown(&mut self) {
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
    }
}

impl Drop for FakeTitan {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Nanoseconds since the Unix epoch, for building frames Titan would stamp right now.
pub(super) fn wall_nanos_now() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after epoch")
            .as_nanos(),
    )
    .expect("unix nanoseconds fit in u64")
}

/// The FermiSwap venue at [`PAMM`], registered under the protocol name `fermiswap`.
pub(super) fn fermiswap() -> PriceLevelStreamConfig {
    PriceLevelStreamConfig::new("fermiswap", address(PAMM), BigUint::from(120_000u64))
}

/// A [`FakeTitan`] handler that sends `first` once, then `repeat` every `interval` until the
/// socket closes.
pub(super) fn frame_then_repeat(
    first: Message,
    repeat: Message,
    interval: Duration,
) -> impl Fn(usize, FakeConnection) -> BoxFuture<'static, ()> + Send + Sync + 'static {
    move |_, mut socket| {
        let first = first.clone();
        let repeat = repeat.clone();
        Box::pin(async move {
            if socket.send(first).await.is_err() {
                return;
            }
            while socket
                .send(repeat.clone())
                .await
                .is_ok()
            {
                tokio::time::sleep(interval).await;
            }
        })
    }
}

/// A valid frame carrying the FermiSwap WBTC/USDC book two-sided: 1 WBTC sells for 99,900 USDC
/// and 100,100 USDC buys 1 WBTC.
pub(super) fn frame_text(block: u64, ts: u64) -> String {
    format!(
        r#"{{"slot": 1, "block": {block}, "ts": {ts}, "books": [
            {{"maker": "Fermi", "quoteAddress": "{PAMM}", "token1": "WBTC", "token2": "USDC",
              "bids": [{{"price": 99900.0, "cumVolume": 1.0}}],
              "asks": [{{"price": 100100.0, "cumVolume": 1.0}}]}}
        ]}}"#
    )
}

/// A frame captured verbatim from the live stream (2026-10-08); the file name carries the frame's
/// `ts` field.
const CAPTURED_MESSAGE: &str =
    include_str!("test_responses/pamm_price_levels_v2_1791475493806326998.json");

/// The books of the captured live frame.
pub(super) fn captured_books() -> Vec<TitanBook> {
    serde_json::from_str::<TitanPriceLevelMessage>(CAPTURED_MESSAGE)
        .expect("the captured frame parses")
        .books
}

/// Every captured book `maker` quoted, in capture order.
pub(super) fn captured_books_of(maker: &str) -> Vec<TitanBook> {
    let books: Vec<TitanBook> = captured_books()
        .into_iter()
        .filter(|book| book.maker.as_deref() == Some(maker))
        .collect();
    assert!(!books.is_empty(), "no captured book of {maker}");
    books
}

/// The captured book `maker` quoted for `token1`/`token2`.
pub(super) fn captured_book(maker: &str, token1: &str, token2: &str) -> TitanBook {
    captured_books_of(maker)
        .into_iter()
        .find(|book| book.token1 == token1 && book.token2 == token2)
        .expect("captured book")
}

/// A frame stamped with `block`/`ts` carrying every captured book of `maker`, each verbatim from
/// the capture — so a stream reading it parses what Titan actually sent, including the venue
/// address Titan streams the books under.
pub(super) fn captured_frame_text(maker: &str, block: u64, ts: u64) -> String {
    let frame: serde_json::Value =
        serde_json::from_str(CAPTURED_MESSAGE).expect("the captured frame parses");
    let books: Vec<serde_json::Value> = frame["books"]
        .as_array()
        .expect("the capture carries books")
        .iter()
        .filter(|book| book["maker"] == maker)
        .cloned()
        .collect();
    assert!(!books.is_empty(), "no captured book of {maker}");
    serde_json::json!({ "slot": 1, "block": block, "ts": ts, "books": books }).to_string()
}

/// The price level frame of the 2026-10-08 capture whose books [`CAPTURED_OVERRIDES`] priced: six
/// of its twenty books name a pair version an entry of that override frame advertises, which is
/// the join the two sockets exist for. [`CAPTURED_MESSAGE`], captured one frame earlier, predates
/// every one of those entries and joins none of them.
const JOINED_MESSAGE: &str =
    include_str!("test_responses/pamm_price_levels_v2_1791475494807834809.json");

/// The override frame captured verbatim from the live `pamm_quote_stream_v2` socket (2026-10-08),
/// one entry per venue Titan priced in that build round.
const CAPTURED_OVERRIDES: &str =
    include_str!("test_responses/pamm_quote_stream_v2_1791475494921154915.json");

/// The books of [`JOINED_MESSAGE`], verbatim.
pub(super) fn joined_books() -> Vec<TitanBook> {
    serde_json::from_str::<TitanPriceLevelMessage>(JOINED_MESSAGE)
        .expect("the captured frame parses")
        .books
}

/// [`JOINED_MESSAGE`] re-stamped with `block`/`ts`, as raw JSON: every book verbatim, so a stream
/// reading it joins exactly what Titan sent, judged against a clock of the reading host.
pub(super) fn joined_frame_text(block: u64, ts: u64) -> String {
    let frame = restamped(JOINED_MESSAGE, "block", |_| block);
    restamped(&frame, "ts", |_| ts)
}

/// [`CAPTURED_OVERRIDES`], parsed.
pub(super) fn captured_override_frame() -> TitanOverrideMessage {
    serde_json::from_str(CAPTURED_OVERRIDES).expect("the captured override frame parses")
}

/// [`CAPTURED_OVERRIDES`] as raw JSON, shifted so that its newest venue entry is stamped
/// `newest_built_at_ns`.
///
/// The shift is what lets a capture stand in for live traffic a reading host judges against its
/// own clock. It is uniform, so the entries keep the 71.6 ms spread of build stamps the capture
/// holds instead of collapsing onto one instant; everything else, the pair versions included, is
/// verbatim.
pub(super) fn shifted_override_text(newest_built_at_ns: u64) -> String {
    let newest = captured_override_frame()
        .venues
        .iter()
        .filter_map(|venue| venue.timestamp)
        .max()
        .expect("a stamped venue entry");
    restamped(CAPTURED_OVERRIDES, "timestamp", |built_at_ns| {
        newest_built_at_ns - (newest - built_at_ns)
    })
}

/// `frame` with the value of every `"key": <digits>` field replaced by `restamp` of it, as text.
///
/// Text surgery rather than a `serde_json::Value` round trip, which is the obvious way and the
/// wrong one: a `Value` holds no integer wider than a `u64`, so the pair versions of either
/// capture — every one of them above `u64::MAX` — would come back out as an `f64`, and the join
/// the two sockets exist for would quietly stop joining.
fn restamped(frame: &str, key: &str, restamp: impl Fn(u64) -> u64) -> String {
    let field = format!("\"{key}\": ");
    let mut restamped = String::with_capacity(frame.len());
    let mut rest = frame;
    let mut fields = 0;
    while let Some(start) = rest.find(&field) {
        let (head, tail) = rest.split_at(start + field.len());
        restamped.push_str(head);
        let digits = tail.len() -
            tail.trim_start_matches(|character: char| character.is_ascii_digit())
                .len();
        let (value, tail) = tail.split_at(digits);
        let value: u64 = value
            .parse()
            .unwrap_or_else(|_| panic!("\"{key}\" is not an integer field"));
        restamped.push_str(&restamp(value).to_string());
        rest = tail;
        fields += 1;
    }
    assert!(fields > 0, "the captured frame carries no \"{key}\" field");
    restamped.push_str(rest);
    restamped
}
