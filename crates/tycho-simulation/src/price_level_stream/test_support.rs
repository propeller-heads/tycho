//! Helpers shared by the price level stream tests.

use std::{
    collections::{BTreeMap, HashMap},
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
    overrides::{AccountOverride, PammSimulationOverrides},
    titan::{
        self, EntryRejection, TitanBook, TitanPriceLevelMessage, TitanQuoteStreamMessage,
        VenueOverrides,
    },
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
/// Quoted by Bebop in both price level captures and by TaurusFi in the later one, and admitted by
/// no token map these tests build, so a book naming it is skipped as `unadmitted_token`.
pub(super) const NVDAX: &str = "0xc845b2894dbddd03858fd2d643b4ef725fe0849d";

pub(super) fn address(hex: &str) -> Bytes {
    Bytes::from_str(hex).expect("hex address")
}

/// The address Titan streams beside `symbol`, for the assets these tests build books from.
///
/// Test ergonomics only: a test names a pair the way a reader reads it, while the book it builds
/// carries the address the parser resolves. Nothing in production resolves a symbol.
pub(super) fn symbol_address(symbol: &str) -> &'static str {
    match symbol {
        "WBTC" => WBTC,
        "USDC" => USDC,
        "WETH" => WETH,
        "USDT" => USDT,
        "cbBTC" => CBBTC,
        "NVDAx" => NVDAX,
        other => panic!("no test address for {other}"),
    }
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

/// Token metadata for every bound asset the 2026-10-16 captures quote: [`tokens`] plus USDT and
/// cbBTC. The captures also quote NVDAx, AAVE, LINK and QNT, which this map leaves out, so those
/// books stay unserved.
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
                // Keep the socket open without answering: dropping it would make the client's
                // connect fail at once instead of hanging.
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

/// Nanoseconds since the Unix epoch, for a message `ts` Titan would write right now.
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
pub(super) fn message_then_repeat(
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

/// A valid message carrying the FermiSwap WBTC/USDC book two-sided: 1 WBTC sells for 99,900 USDC
/// and 100,100 USDC buys 1 WBTC.
pub(super) fn message_text(block: u64, ts: u64) -> String {
    format!(
        r#"{{"slot": 1, "block": {block}, "ts": {ts}, "books": [
            {{"maker": "Fermi", "quoteAddress": "{PAMM}", "token1": "WBTC", "token2": "USDC",
              "token1Address": "{WBTC}", "token2Address": "{USDC}",
              "bids": [{{"price": 99900.0, "cumVolume": 1.0}}],
              "asks": [{{"price": 100100.0, "cumVolume": 1.0}}]}}
        ]}}"#
    )
}

/// A message captured verbatim from the live stream (2026-10-16); the file name carries the
/// message's `ts` field.
const CAPTURED_MESSAGE: &str =
    include_str!("test_responses/pamm_price_levels_v2_1792115918290990905.json");

/// The books of the captured live message.
pub(super) fn captured_books() -> Vec<TitanBook> {
    serde_json::from_str::<TitanPriceLevelMessage>(CAPTURED_MESSAGE)
        .expect("the captured message parses")
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

/// A message with `block` and `ts`, carrying every captured book of `maker`, each verbatim from
/// the capture — so a stream reading it parses what Titan actually sent, including the venue
/// address Titan streams the books under.
pub(super) fn captured_message_text(maker: &str, block: u64, ts: u64) -> String {
    let message: serde_json::Value =
        serde_json::from_str(CAPTURED_MESSAGE).expect("the captured message parses");
    let books: Vec<serde_json::Value> = message["books"]
        .as_array()
        .expect("the capture carries books")
        .iter()
        .filter(|book| book["maker"] == maker)
        .cloned()
        .collect();
    assert!(!books.is_empty(), "no captured book of {maker}");
    serde_json::json!({ "slot": 1, "block": block, "ts": ts, "books": books }).to_string()
}

/// The beacon slot both 2026-10-16 captures were taken in.
pub(super) const CAPTURED_SLOT: u64 = 15_440_992;

/// The price level message of the 2026-10-16 capture whose books the captured quote stream message
/// priced. Of its 30 books on admitted tokens, 27 find their venue's entry in that quote stream
/// message: 16 at their own quote sequence and 11 at an earlier one of the same slot.
/// [`CAPTURED_MESSAGE`], captured earlier, belongs to the previous slot and matches none of them.
const MATCHING_MESSAGE: &str =
    include_str!("test_responses/pamm_price_levels_v2_1792115920294943036.json");

/// The quote stream message captured verbatim from the live `pamm_quote_stream_v2` socket
/// (2026-10-16), one entry per venue Titan priced.
const CAPTURED_QUOTE_STREAM_MESSAGE: &str =
    include_str!("test_responses/pamm_quote_stream_v2_1792115920248724389.json");

/// The books of [`MATCHING_MESSAGE`], verbatim.
pub(super) fn matching_books() -> Vec<TitanBook> {
    serde_json::from_str::<TitanPriceLevelMessage>(MATCHING_MESSAGE)
        .expect("the captured message parses")
        .books
}

/// [`MATCHING_MESSAGE`] with its `block` and `ts` replaced, as raw JSON: every book verbatim, so a
/// stream reading it matches exactly what Titan sent, while its `ts` passes the freshness checks
/// of the reading host's clock.
pub(super) fn matching_message_text(block: u64, ts: u64) -> String {
    let message = with_integer_field(MATCHING_MESSAGE, "block", block);
    with_integer_field(&message, "ts", ts)
}

/// [`CAPTURED_QUOTE_STREAM_MESSAGE`], parsed.
pub(super) fn captured_quote_stream_message() -> TitanQuoteStreamMessage {
    serde_json::from_str(CAPTURED_QUOTE_STREAM_MESSAGE)
        .expect("the captured quote stream message parses")
}

/// [`CAPTURED_QUOTE_STREAM_MESSAGE`] as raw JSON, verbatim.
pub(super) fn captured_quote_stream_text() -> &'static str {
    CAPTURED_QUOTE_STREAM_MESSAGE
}

/// The accepted entries of `message`. It skips the entries refused as `empty` (no pair, or no
/// account), of which the captures hold one, and panics on any other refusal.
pub(super) fn captured_venue_overrides(message: TitanQuoteStreamMessage) -> Vec<VenueOverrides> {
    titan::validate(message)
        .into_iter()
        .filter_map(|outcome| match outcome {
            Ok(entry) => Some(entry),
            Err(EntryRejection::Empty) => None,
            Err(reason) => panic!("refused a captured entry: {reason:?}"),
        })
        .collect()
}

/// The account the synthetic quote stream entries of the tests override.
pub(super) const SYNTHETIC_ACCOUNT: &str = "0xba443ecec71dc7123bd68c2082d13e3299cde896";

/// The overrides of the Goliath entry of [`CAPTURED_QUOTE_STREAM_MESSAGE`], transcribed by hand:
/// the venue `0x4367985a…` as Titan took it at `timestamp` 1792115920127487452, in beacon slot
/// 15440992. The matching price level message names this entry, so tests can pin a match to values
/// read off the capture rather than to the parser's own output.
///
/// It has two accounts: one overrides a balance and a nonce only, the other also writes a single
/// 32-byte storage word. `block_timestamp` is the slot as a Unix second.
pub(super) fn test_overrides() -> PammSimulationOverrides {
    PammSimulationOverrides {
        accounts: BTreeMap::from([
            (
                address("0x48165e588ec68e724315d3e0fe7b0f3b9b6c7a6d"),
                AccountOverride {
                    balance: Some(address("0x1b6008ea9711c3f3")),
                    nonce: Some(0xe40),
                    state_diff: BTreeMap::new(),
                },
            ),
            (
                address("0xb261ef0a97bc7cf3af2025ab6b3792e6062d3d4a"),
                AccountOverride {
                    // The entry writes this balance `0x0`, one odd digit, which decodes to a
                    // single zero byte.
                    balance: Some(address("0x00")),
                    nonce: Some(1),
                    state_diff: BTreeMap::from([(
                        address(
                            "0x0000000000000000000000000000000000000000000000000000000000000000",
                        ),
                        address(
                            "0x000000006ad184d800003c06d28e200000000000000000000000000090db0604",
                        ),
                    )]),
                },
            ),
        ]),
        block_number: Some(26_201_954),
        block_timestamp: Some(1_792_115_927),
        built_at_ns: 1_792_115_920_127_487_452,
    }
}

/// `message` with the value of every `"key": <digits>` field replaced by `value`, as text.
///
/// It edits the text instead of round-tripping through a `serde_json::Value`: a `Value` holds no
/// integer wider than `u64`, so the capture's quote sequences, all above `u64::MAX`, would come
/// back as `f64` and match nothing.
fn with_integer_field(message: &str, key: &str, value: u64) -> String {
    let field = format!("\"{key}\":");
    let mut replaced = String::with_capacity(message.len());
    let mut rest = message;
    let mut fields = 0;
    while let Some(start) = rest.find(&field) {
        let (head, tail) = rest.split_at(start + field.len());
        replaced.push_str(head);
        // Titan sends compact JSON; skip spaces after the colon so that formatted JSON works too.
        let spaces = tail.len() - tail.trim_start_matches(' ').len();
        let (padding, tail) = tail.split_at(spaces);
        replaced.push_str(padding);
        let digits = tail.len() -
            tail.trim_start_matches(|character: char| character.is_ascii_digit())
                .len();
        assert!(digits > 0, "\"{key}\" is not an integer field");
        replaced.push_str(&value.to_string());
        let tail = &tail[digits..];
        rest = tail;
        fields += 1;
    }
    assert!(fields > 0, "the captured message carries no \"{key}\" field");
    replaced.push_str(rest);
    replaced
}
