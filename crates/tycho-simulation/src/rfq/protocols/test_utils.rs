use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Arc, Mutex},
};

use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::Duration,
};
use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::{
    models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        token::Token,
        Chain, ChangeType,
    },
    Bytes,
};

use crate::rfq::protocols::component::BOOKS_ATTRIBUTE;

pub fn token(address: &str, symbol: &str, decimals: u32) -> Token {
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

pub fn weth() -> Token {
    token("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", "WETH", 18)
}

pub fn usdc() -> Token {
    token("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", "USDC", 6)
}

pub fn wbtc() -> Token {
    token("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599", "WBTC", 8)
}

/// A venue component carrying `books`, and the tokens it names.
pub fn venue_snapshot(
    protocol_system: &str,
    tokens: &[Token],
    books: &impl Serialize,
) -> (ComponentWithState, HashMap<Bytes, Token>) {
    let books = serde_json::to_vec(books).unwrap();
    let snapshot = ComponentWithState {
        state: ProtocolComponentState {
            attributes: HashMap::from([(BOOKS_ATTRIBUTE.to_string(), books.into())]),
            component_id: protocol_system.to_string(),
            balances: HashMap::new(),
        },
        component: ProtocolComponent {
            id: protocol_system.to_string(),
            protocol_system: protocol_system.to_string(),
            protocol_type_name: protocol_system.to_string(),
            chain: Chain::Ethereum,
            tokens: tokens
                .iter()
                .map(|token| token.address.clone())
                .collect(),
            contract_addresses: Vec::new(),
            static_attributes: HashMap::new(),
            change: ChangeType::Creation,
            creation_tx: Bytes::default(),
            created_at: chrono::NaiveDateTime::default(),
        },
        component_tvl: None,
        entrypoints: Vec::new(),
    };
    let tokens = tokens
        .iter()
        .map(|token| (token.address.clone(), token.clone()))
        .collect();
    (snapshot, tokens)
}

/// Reads one HTTP request off the stream and returns its body.
pub async fn read_request_body(stream: &mut TcpStream) -> String {
    let mut raw = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = stream.read(&mut buf).await.unwrap();
        raw.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&raw);
        if let Some(header_end) = text.find("\r\n\r\n") {
            let content_length: usize = text
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .unwrap_or(0);
            if raw.len() >= header_end + 4 + content_length {
                return text[header_end + 4..].to_string();
            }
        }
        if n == 0 {
            return String::new();
        }
    }
}

/// The effectiveTrader value of a request body.
pub fn effective_trader_of(request_body: &str) -> String {
    let start = request_body
        .find("\"effectiveTrader\":\"")
        .expect("request carries no effectiveTrader") +
        "\"effectiveTrader\":\"".len();
    request_body[start..start + request_body[start..].find('"').unwrap()].to_string()
}

/// A server that answers every request with `json_response` after `delay_ms`, substituting the
/// request's effectiveTrader for `{{EFFECTIVE_TRADER}}`. Returns its address and the request
/// bodies it received.
pub async fn mock_quote_server(
    delay_ms: u64,
    json_response: &'static str,
) -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let request_log: Arc<Mutex<Vec<String>>> = Arc::default();
    let request_log_server = request_log.clone();

    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let request_log = request_log_server.clone();
            tokio::spawn(async move {
                let body = read_request_body(&mut stream).await;
                let json_response = match body.contains("effectiveTrader") {
                    true => {
                        json_response.replace("{{EFFECTIVE_TRADER}}", &effective_trader_of(&body))
                    }
                    false => json_response.to_string(),
                };
                request_log.lock().unwrap().push(body);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    json_response.len(),
                    json_response
                );
                let _ = stream
                    .write_all(response.as_bytes())
                    .await;
                let _ = stream.flush().await;
                let _ = stream.shutdown().await;
            });
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, request_log)
}
