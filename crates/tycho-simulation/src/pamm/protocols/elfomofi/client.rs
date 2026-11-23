use std::{fmt, sync::Arc, time::SystemTime};

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    primitives::{Address, Bytes as AlloyBytes, TxKind, B256},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::{TransactionInput, TransactionRequest},
    transports::TransportError,
};
use tokio::sync::OnceCell;
use tycho_common::{models::Chain, Bytes};

use super::models::helper_for;
use crate::snapshot_feed::errors::FeedError;
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RpcHeader {
    pub number: u64,
    pub hash: Bytes,
    pub timestamp: u64,
}

#[derive(Clone)]
pub(super) struct ElfomofiClient {
    chain: Chain,
    provider: RootProvider,
    verified_chain: Arc<OnceCell<()>>,
}

impl fmt::Debug for ElfomofiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ElfomofiClient")
            .field("chain", &self.chain)
            .finish_non_exhaustive()
    }
}

impl ElfomofiClient {
    pub fn new(chain: Chain, rpc_url: &str) -> Result<Self, FeedError> {
        helper_for(chain)?;
        let url = reqwest::Url::parse(rpc_url)
            .map_err(|_error| FeedError::InvalidInput("Invalid ElFomoFi RPC URL".into()))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(FeedError::InvalidInput("ElFomoFi RPC URL must use HTTP or HTTPS".into()));
        }
        Ok(Self {
            chain,
            provider: ProviderBuilder::default().connect_http(url),
            verified_chain: Arc::new(OnceCell::new()),
        })
    }

    pub(super) async fn verify_chain(&self) -> Result<(), FeedError> {
        self.verified_chain
            .get_or_try_init(|| async {
                let chain_id = self
                    .provider
                    .get_chain_id()
                    .await
                    .map_err(|error| rpc_error("eth_chainId", &error))?;
                if chain_id != self.chain.id() {
                    return Err(FeedError::Fatal(format!(
                        "ElFomoFi RPC must serve {} chain ID {}",
                        self.chain,
                        self.chain.id()
                    )));
                }
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub(super) async fn header(&self, tag: &str) -> Result<RpcHeader, FeedError> {
        let tag: BlockNumberOrTag = tag
            .parse()
            .map_err(|_error| FeedError::InvalidInput("Invalid ElFomoFi block tag".into()))?;
        let block = self
            .provider
            .get_block_by_number(tag)
            .await
            .map_err(|error| rpc_error("eth_getBlockByNumber", &error))?
            .ok_or_else(|| FeedError::Parsing("ElFomoFi RPC returned no block header".into()))?;
        Ok(RpcHeader {
            number: block.header.number,
            hash: Bytes::from(block.header.hash.as_slice()),
            timestamp: block.header.timestamp,
        })
    }

    pub(super) async fn call_at(
        &self,
        address: &str,
        data: &[u8],
        sender: Option<&Bytes>,
        header: &RpcHeader,
    ) -> Result<Vec<u8>, FeedError> {
        let address: Address = address
            .parse()
            .map_err(|_error| FeedError::InvalidInput("Invalid ElFomoFi call address".into()))?;
        let sender = sender
            .map(|sender| {
                Address::try_from(sender.as_ref()).map_err(|_error| {
                    FeedError::InvalidInput("Invalid ElFomoFi call sender".into())
                })
            })
            .transpose()?;
        let call = TransactionRequest {
            from: sender,
            to: Some(TxKind::Call(address)),
            input: TransactionInput::new(AlloyBytes::copy_from_slice(data)),
            ..Default::default()
        };
        let hash = B256::try_from(header.hash.as_ref()).map_err(|_error| {
            FeedError::Parsing("ElFomoFi block hash must have 32 bytes".into())
        })?;
        self.provider
            .call(call)
            .block(BlockId::hash_canonical(hash))
            .await
            .map(|output| output.to_vec())
            .map_err(|error| rpc_error("eth_call", &error))
    }
}

fn rpc_error(method: &str, error: &TransportError) -> FeedError {
    // Provider error bodies and URLs can contain credentials; retain only the status or kind.
    if let Some(response) = error.as_error_resp() {
        return FeedError::Connection(format!(
            "ElFomoFi RPC {method} failed with code {}",
            response.code
        ));
    }
    if error.is_deser_error() {
        return FeedError::Parsing(format!("ElFomoFi RPC {method} response decoding failed"));
    }
    let kind = if let Some(transport) = error.as_transport_err() {
        if let Some(http) = transport.as_http_error() {
            return FeedError::Connection(format!(
                "ElFomoFi RPC {method} failed with HTTP {}",
                http.status
            ));
        }
        if let Some(request) = transport
            .as_custom()
            .and_then(|error| error.downcast_ref::<alloy::transports::http::reqwest::Error>())
        {
            if request.is_timeout() {
                "request timeout"
            } else if request.is_connect() {
                "connection error"
            } else if request.is_decode() {
                "response decoding error"
            } else if request.is_body() {
                "response body error"
            } else {
                "HTTP request error"
            }
        } else if transport.is_backend_gone() {
            "connection closed"
        } else if transport.is_pubsub_unavailable() {
            "subscriptions unavailable"
        } else {
            "transport error"
        }
    } else if error.is_null_resp() {
        "empty response"
    } else if error.is_ser_error() {
        "request serialization error"
    } else if error.is_unsupported_feature() {
        "unsupported feature"
    } else if error.is_local_usage_error() {
        "invalid request"
    } else {
        "RPC error"
    };
    FeedError::Connection(format!("ElFomoFi RPC {method} failed: {kind}"))
}

pub fn unix_now() -> Result<u64, FeedError> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_error| FeedError::Fatal("System clock precedes UNIX epoch".into()))
}

#[cfg(test)]
mod tests {
    use alloy::{rpc::json_rpc::ErrorPayload, transports::TransportErrorKind};

    use super::*;

    #[test]
    fn error_status_and_kind_survive_without_provider_secrets() {
        let secret = "https://user:password@example.test/private-provider-key";
        let cases = [
            (TransportErrorKind::http_error(429, secret.into()), "HTTP 429"),
            (TransportErrorKind::http_error(503, secret.into()), "HTTP 503"),
            (
                TransportError::ErrorResp(ErrorPayload::internal_error_message(secret.into())),
                "code -32603",
            ),
            (
                TransportError::deser_err(
                    serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
                    secret,
                ),
                "response decoding",
            ),
            (TransportErrorKind::custom_str(secret), "transport error"),
        ];
        for (error, expected) in cases {
            let error = rpc_error("eth_call", &error).to_string();
            assert!(error.contains(expected), "{error}");
            for sensitive in ["password", "private-provider-key", "example.test"] {
                assert!(!error.contains(sensitive), "{error}");
            }
        }
    }

    #[tokio::test]
    async fn request_timeouts_keep_their_kind_without_the_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let endpoint = format!("http://{}/private-provider-key", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (_stream, _address) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let request = alloy::transports::http::reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(20))
            .build()
            .unwrap()
            .get(endpoint)
            .send()
            .await
            .unwrap_err();
        let message = rpc_error("eth_call", &TransportErrorKind::custom(request)).to_string();
        server.abort();
        assert!(message.contains("request timeout"), "{message}");
        assert!(!message.contains("private-provider-key"), "{message}");
        assert!(!message.contains("127.0.0.1"), "{message}");
    }
}
