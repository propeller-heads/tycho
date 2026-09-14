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
    time::{SystemTime, UNIX_EPOCH},
};

use futures::future::BoxFuture;
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_tungstenite::WebSocketStream;
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

/// The FermiSwapper router, one of the default venues.
pub(super) const PAMM: &str = "0x5979458912f80b96d30d4220af8e2e4925a33320";
pub(super) const WBTC: &str = "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599";
pub(super) const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
pub(super) const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";

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

pub(super) fn tokens() -> HashMap<Bytes, Token> {
    [token(WBTC, "WBTC", 8), token(USDC, "USDC", 6), token(WETH, "WETH", 18)]
        .into_iter()
        .map(|token| (token.address.clone(), token))
        .collect()
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
    .expect("fits")
}

/// A valid frame carrying the FermiSwap WBTC/USDC ladder in both directions.
pub(super) fn frame_text(block: u64, timestamp: u64) -> String {
    format!(
        r#"{{"slot": 1, "blockNumber": {block}, "timestamp": {timestamp}, "pamms": [{{"pamm": "{PAMM}", "maker": 12, "pairs": [
            {{"tokenIn": "{WBTC}", "tokenOut": "{USDC}", "orderBook": [{{"amountIn": "0x5f5e100", "amountOut": "0x174876e800", "variant": "Simulated"}}]}},
            {{"tokenIn": "{USDC}", "tokenOut": "{WBTC}", "orderBook": [{{"amountIn": "0x174876e800", "amountOut": "0x5e69ec0", "variant": "Simulated"}}]}}
        ]}}]}}"#
    )
}
