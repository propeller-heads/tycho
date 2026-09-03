//! Archive-node access: block headers, event logs over a block range and full storage dumps.

use std::collections::HashMap;

use alloy::{
    primitives::{Address, B256},
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::{Filter, Log},
};
use anyhow::{anyhow, Context, Result};
use tracing::debug;
use tycho_common::{
    models::Chain,
    traits::{AccountExtractor as _, StorageSnapshotRequest},
};
use tycho_ethereum::{
    rpc::EthereumRpcClient, services::account_extractor::EVMAccountExtractor, BytesCodec,
};

/// Most providers cap `eth_getLogs` at 10k blocks per request.
const LOG_CHUNK: u64 = 10_000;

pub struct BlockHeader {
    pub number: u64,
    pub hash: B256,
    pub timestamp: u64,
}

/// Storage of several contracts as of the end of one block.
pub struct Dump {
    pub block_number: u64,
    pub block_hash: B256,
    pub timestamp: u64,
    pub storage: HashMap<Address, HashMap<B256, B256>>,
}

pub struct Rpc {
    provider: RootProvider,
    client: EthereumRpcClient,
}

impl Rpc {
    pub fn connect(url: &str) -> Result<Self> {
        let provider = ProviderBuilder::default().connect_http(url.parse().context("RPC URL")?);
        let client = EthereumRpcClient::new(url).map_err(|e| anyhow!("{e}"))?;

        Ok(Self { provider, client })
    }

    /// Every log of `address` with `topic0` as first topic in `[from_block, to_block]`, in chain
    /// order, fetched in chunks the provider accepts.
    pub async fn logs(
        &self,
        address: Address,
        topic0: B256,
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<Log>> {
        let mut logs = Vec::new();
        let mut start = from_block;
        while start <= to_block {
            let end = (start + LOG_CHUNK - 1).min(to_block);
            let filter = Filter::new()
                .address(address)
                .event_signature(topic0)
                .from_block(start)
                .to_block(end);
            let chunk = self
                .provider
                .get_logs(&filter)
                .await
                .with_context(|| format!("eth_getLogs for blocks {start}..={end}"))?;
            debug!(start, end, logs = chunk.len(), "fetched logs");
            logs.extend(chunk);

            start = end + 1;
        }

        Ok(logs)
    }

    pub async fn block_header(&self, block_number: u64) -> Result<BlockHeader> {
        let block = EVMAccountExtractor::new(&self.client, Chain::Ethereum)
            .get_block_data(block_number)
            .await
            .map_err(|e| anyhow!("fetching block {block_number}: {e}"))?;

        Ok(BlockHeader {
            number: block.number,
            hash: B256::from_slice(&block.hash),
            timestamp: block.ts.and_utc().timestamp() as u64,
        })
    }

    /// Full storage of `addresses` after `block_number`, via `debug_storageRangeAt`.
    pub async fn dump_storage(&self, block_number: u64, addresses: &[Address]) -> Result<Dump> {
        let extractor = EVMAccountExtractor::new(&self.client, Chain::Ethereum);
        let block = extractor
            .get_block_data(block_number)
            .await
            .map_err(|e| anyhow!("fetching block {block_number}: {e}"))?;

        let requests: Vec<_> = addresses
            .iter()
            .map(|address| StorageSnapshotRequest { address: address.to_bytes(), slots: None })
            .collect();
        let accounts = extractor
            .get_accounts_at_block(&block, &requests)
            .await
            .map_err(|e| anyhow!("dumping storage at block {block_number}: {e:?}"))?;

        let mut storage = HashMap::with_capacity(addresses.len());
        for (address, delta) in accounts {
            let slots = delta
                .slots
                .into_iter()
                .filter_map(|(slot, value)| {
                    value.map(|value| (B256::from_slice(&slot), B256::from_slice(&value)))
                })
                .collect::<HashMap<_, _>>();
            debug!(%address, slots = slots.len(), "dumped storage");
            storage.insert(Address::from_bytes(&address), slots);
        }

        Ok(Dump {
            block_number: block.number,
            block_hash: B256::from_slice(&block.hash),
            timestamp: block.ts.and_utc().timestamp() as u64,
            storage,
        })
    }
}
