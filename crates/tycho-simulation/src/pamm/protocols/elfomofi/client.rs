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
pub(super) use crate::snapshot_feed::evm_heads::RpcHeader;

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
    // Provider messages can contain their credential-bearing URL. Only expose the code.
    match error.as_error_resp() {
        Some(response) => FeedError::Connection(format!(
            "ElFomoFi RPC {method} failed with code {}",
            response.code
        )),
        None => FeedError::Connection(format!("ElFomoFi RPC {method} failed")),
    }
}

pub fn unix_now() -> Result<u64, FeedError> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_error| FeedError::Fatal("System clock precedes UNIX epoch".into()))
}
