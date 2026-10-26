//! Block-pinned RPC reads; scheduling belongs to the shared snapshot feed.
use std::{fmt, time::SystemTime};

use reqwest::Client;
use serde_json::{json, Value};
use tycho_common::{models::Chain, Bytes};

use super::models::{ensure_timestamp_fresh, helper_for};
use crate::snapshot_feed::errors::FeedError;

const MAX_RPC_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct ElfomofiClient {
    chain: Chain,
    rpc_url: String,
    http_client: Client,
}
impl fmt::Debug for ElfomofiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ElfomofiClient")
            .field("chain", &self.chain)
            .field("rpc_url", &"[redacted]")
            .finish()
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RpcHeader {
    pub number: u64,
    pub hash: Bytes,
    pub timestamp: u64,
}

impl ElfomofiClient {
    pub fn new(chain: Chain, rpc_url: String) -> Result<Self, FeedError> {
        helper_for(chain)?;
        let url = reqwest::Url::parse(&rpc_url)
            .map_err(|_| FeedError::InvalidInput("Invalid ElFomoFi RPC URL".into()))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(FeedError::InvalidInput("ElFomoFi RPC URL must use HTTP or HTTPS".into()));
        }
        let http_client = Client::builder()
            .build()
            .map_err(|e| FeedError::Connection(e.without_url().to_string()))?;
        Ok(Self { chain, rpc_url, http_client })
    }
    async fn rpc(&self, method: &str, params: Value) -> Result<Value, FeedError> {
        let operation = async {
            let mut response = self
                .http_client
                .post(&self.rpc_url)
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
                .send()
                .await
                .map_err(|error| FeedError::Connection(error.without_url().to_string()))?;
            if !response.status().is_success() {
                return Err(FeedError::Connection(format!(
                    "ElFomoFi RPC {method} returned HTTP {}",
                    response.status()
                )));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| FeedError::Connection(error.without_url().to_string()))?
            {
                if bytes.len().saturating_add(chunk.len()) > MAX_RPC_RESPONSE_BYTES {
                    return Err(FeedError::Parsing("ElFomoFi RPC response exceeds 16 MiB".into()));
                }
                bytes.extend_from_slice(&chunk);
            }
            let response: Value = serde_json::from_slice(&bytes).map_err(|error| {
                FeedError::Parsing(format!("Invalid ElFomoFi RPC JSON: {error}"))
            })?;
            if response.get("jsonrpc") != Some(&json!("2.0")) ||
                response.get("id") != Some(&json!(1))
            {
                return Err(FeedError::Parsing("Invalid ElFomoFi JSON-RPC envelope".into()));
            }
            if let Some(error) = response.get("error") {
                // Provider messages can contain their credential-bearing URL. Only expose code.
                return Err(FeedError::Connection(format!(
                    "ElFomoFi RPC {method} failed with code {}",
                    error
                        .get("code")
                        .unwrap_or(&Value::Null)
                )));
            }
            response
                .get("result")
                .cloned()
                .filter(|result| !result.is_null())
                .ok_or_else(|| FeedError::Parsing(format!("Missing ElFomoFi RPC {method} result")))
        };
        operation.await
    }

    pub(super) async fn verify_chain(&self) -> Result<(), FeedError> {
        let result = self
            .rpc("eth_chainId", json!([]))
            .await?;
        if parse_quantity(&result, "chain ID")? != self.chain.id() {
            return Err(FeedError::Fatal(format!(
                "ElFomoFi RPC must serve {} chain ID {}",
                self.chain,
                self.chain.id()
            )));
        }
        Ok(())
    }

    pub(super) async fn header(&self, tag: &str) -> Result<RpcHeader, FeedError> {
        let result = self
            .rpc("eth_getBlockByNumber", json!([tag, false]))
            .await?;
        let number = parse_quantity(&result["number"], "block number")?;
        let timestamp = parse_quantity(&result["timestamp"], "block timestamp")?;
        let hash = parse_hex_bytes(&result["hash"], "block hash")?;
        if hash.len() != 32 {
            return Err(FeedError::Parsing("ElFomoFi block hash must have 32 bytes".into()));
        }
        Ok(RpcHeader { number, hash, timestamp })
    }

    pub(super) async fn call_at(
        &self,
        address: &str,
        data: &[u8],
        sender: Option<&Bytes>,
        header: &RpcHeader,
    ) -> Result<Vec<u8>, FeedError> {
        let mut call = json!({"to": address, "data": format!("0x{}", hex::encode(data))});
        if let Some(sender) = sender {
            call["from"] = json!(sender.to_string());
        }
        let result = self
            .rpc("eth_call", json!([call, format!("0x{:x}", header.number)]))
            .await?;
        Ok(parse_hex_bytes(&result, "eth_call output")?.to_vec())
    }

    pub(super) async fn verify_header(
        &self,
        expected: &RpcHeader,
        max_age_seconds: u64,
    ) -> Result<(), FeedError> {
        let actual = self
            .header(&format!("0x{:x}", expected.number))
            .await?;
        if actual != *expected {
            return Err(FeedError::Connection(
                "ElFomoFi canonical header changed during RPC quote".into(),
            ));
        }
        ensure_timestamp_fresh(expected.timestamp, unix_now()?, max_age_seconds)
    }
}

pub(crate) fn unix_now() -> Result<u64, FeedError> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| FeedError::Fatal("System clock precedes UNIX epoch".into()))
}

fn parse_quantity(value: &Value, name: &str) -> Result<u64, FeedError> {
    let digits = value
        .as_str()
        .and_then(|value| value.strip_prefix("0x"))
        .filter(|digits| !digits.is_empty())
        .ok_or_else(|| FeedError::Parsing(format!("Invalid ElFomoFi {name} quantity")))?;
    u64::from_str_radix(digits, 16)
        .map_err(|_| FeedError::Parsing(format!("Invalid ElFomoFi {name} quantity")))
}

fn parse_hex_bytes(value: &Value, name: &str) -> Result<Bytes, FeedError> {
    let digits = value
        .as_str()
        .and_then(|value| value.strip_prefix("0x"))
        .ok_or_else(|| FeedError::Parsing(format!("Invalid ElFomoFi {name} hex")))?;
    hex::decode(digits)
        .map(Bytes::from)
        .map_err(|_| FeedError::Parsing(format!("Invalid ElFomoFi {name} hex")))
}
