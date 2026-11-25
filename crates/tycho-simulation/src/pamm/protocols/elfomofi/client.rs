//! The JSON-RPC calls the feed makes: the node's chain id, once, and the helper's books.

use alloy::{
    eips::BlockId,
    primitives::Bytes as AlloyBytes,
    providers::{Provider, RootProvider},
    rpc::types::{TransactionInput, TransactionRequest},
    sol_types::SolCall,
    transports::{RpcError, TransportErrorKind},
};
use reqwest::Url;
use tokio::sync::OnceCell;

use crate::{
    pamm::protocols::elfomofi::models::{getAllOrderbooksCall, ElfomofiSupportedChain},
    snapshot_feed::errors::FeedError,
};

/// Talks to one chain's node. `Debug` output and every error leave the RPC URL out, since it
/// commonly carries an API key.
#[derive(derive_more::Debug)]
pub(super) struct ElfomofiClient {
    #[debug(skip)]
    provider: RootProvider,
    /// The URL as given and as parsed, which may differ (a trailing slash), so either spelling is
    /// removed from errors.
    #[debug(skip)]
    rpc_urls: [String; 2],
    chain: ElfomofiSupportedChain,
    /// Set once the node has answered with the expected chain id; never set by a mismatch, which
    /// ends the feed.
    #[debug(skip)]
    chain_verified: OnceCell<()>,
}

impl ElfomofiClient {
    /// Fails for a URL that does not parse or is not `http`/`https`.
    pub(super) fn new(rpc_url: &str, chain: ElfomofiSupportedChain) -> Result<Self, FeedError> {
        let invalid = |reason: &dyn std::fmt::Display| {
            FeedError::InvalidInput(format!("ElFomoFi RPC URL (omitted): {reason}"))
        };
        // A URL parse error names what is wrong, never the URL.
        let url = Url::parse(rpc_url).map_err(|error| invalid(&error))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(invalid(&"the scheme must be http or https"));
        }
        Ok(Self {
            rpc_urls: [rpc_url.to_string(), url.to_string()],
            provider: RootProvider::new_http(url),
            chain,
            chain_verified: OnceCell::new(),
        })
    }

    /// Checks the node serves the configured chain, asking it on the first call that gets an
    /// answer only. A node on another chain is [`FeedError::Fatal`]: no retry changes it.
    pub(super) async fn verify_chain(&self) -> Result<(), FeedError> {
        self.chain_verified
            .get_or_try_init(|| async {
                let served = self
                    .provider
                    .get_chain_id()
                    .await
                    .map_err(|e| self.rpc_error("eth_chainId", &e))?;
                let expected = self.chain.chain().id();
                if served != expected {
                    return Err(FeedError::Fatal(format!(
                        "ElFomoFi: the RPC node serves chain {served}, not {} ({expected})",
                        self.chain.chain()
                    )));
                }
                Ok(())
            })
            .await
            .map(drop)
    }

    /// The helper's `getAllOrderbooks()` at the latest block, as returned.
    pub(super) async fn orderbooks(&self) -> Result<AlloyBytes, FeedError> {
        let request = TransactionRequest::default()
            .to(self.chain.helper())
            .input(TransactionInput::new(
                getAllOrderbooksCall {}
                    .abi_encode()
                    .into(),
            ));
        self.provider
            .call(request)
            .block(BlockId::latest())
            .await
            .map_err(|e| self.rpc_error("eth_call", &e))
    }

    /// A failed call as a retryable [`FeedError::Connection`], keeping what classifies it (the
    /// JSON-RPC error code, the HTTP status, or the transport failure) and dropping the URL.
    fn rpc_error(&self, method: &str, error: &RpcError<TransportErrorKind>) -> FeedError {
        let detail = match error {
            RpcError::ErrorResp(payload) => {
                format!("JSON-RPC error {}: {}", payload.code, payload.message)
            }
            RpcError::Transport(TransportErrorKind::HttpError(http)) => {
                format!("HTTP {}: {}", http.status, http.body)
            }
            other @ (RpcError::Transport(_) |
            RpcError::NullResp |
            RpcError::UnsupportedFeature(_) |
            RpcError::LocalUsageError(_) |
            RpcError::SerError(_) |
            RpcError::DeserError { .. }) => other.to_string(),
        };
        let mut detail = detail;
        for url in &self.rpc_urls {
            detail = detail.replace(url.as_str(), "<rpc url>");
        }
        FeedError::Connection(format!("ElFomoFi {method}: {detail}"))
    }
}
