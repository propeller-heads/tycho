use std::{collections::HashMap, error::Error as StdError, fmt};

use tokio::time::{sleep, Duration};
use tracing::{debug, info, warn};
use tycho_simulation::{
    tycho_client::{
        feed::synchronizer::Snapshot,
        rpc::{
            AllTokensParams, HttpRPCClientOptions, ProtocolComponentsPaginatedParams, RPCClient,
            TracedEntryPointsParams,
        },
        HttpRPCClient, SnapshotParameters,
    },
    tycho_common::{
        models::{
            blockchain::{EntryPointWithTracingParams, TracingResult},
            protocol::ProtocolComponent,
            token::Token,
            Chain, ComponentId,
        },
        Bytes,
    },
};

/// Custom error type for RPC operations
#[derive(Debug)]
pub enum RpcError {
    ClientError(String),
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcError::ClientError(msg) => write!(f, "RPC client error: {msg}"),
        }
    }
}

impl StdError for RpcError {}

impl From<Box<dyn StdError>> for RpcError {
    fn from(error: Box<dyn StdError>) -> Self {
        RpcError::ClientError(error.to_string())
    }
}

impl From<tycho_simulation::tycho_client::RPCError> for RpcError {
    fn from(error: tycho_simulation::tycho_client::RPCError) -> Self {
        RpcError::ClientError(error.to_string())
    }
}

/// Client for interacting with the Tycho RPC server
pub struct TychoClient {
    http_client: HttpRPCClient,
}

impl TychoClient {
    pub fn new(host: &str, auth_key: Option<String>) -> Result<Self, RpcError> {
        let options = HttpRPCClientOptions::new().with_auth_key(auth_key);
        let http_client =
            HttpRPCClient::new(host, options).map_err(|e| RpcError::ClientError(e.to_string()))?;
        Ok(Self { http_client })
    }

    /// Gets protocol components from the RPC server
    pub async fn get_protocol_components(
        &self,
        protocol_system: &str,
        chain: Chain,
    ) -> Result<Vec<ProtocolComponent>, RpcError> {
        let chunk_size = 100;
        let concurrency = 1;

        self.http_client
            .get_protocol_components_paginated(
                ProtocolComponentsPaginatedParams::new(chain, protocol_system, concurrency)
                    .with_chunk_size(chunk_size),
            )
            .await
            .map_err(RpcError::from)
    }

    pub async fn get_tokens(
        &self,
        chain: Chain,
        min_quality: Option<i32>,
        max_days_since_last_trade: Option<u64>,
    ) -> Result<HashMap<Bytes, Token>, RpcError> {
        debug!("Loading tokens from Tycho...");

        let concurrency = 1;

        #[allow(clippy::mutable_key_type)]
        let mut params = AllTokensParams::new(chain, concurrency).with_chunk_size(3_000);
        if let Some(q) = min_quality {
            params = params.with_min_quality(q);
        }
        if let Some(d) = max_days_since_last_trade {
            params = params.with_traded_n_days_ago(d);
        }
        let res = self
            .http_client
            .get_all_tokens(params)
            .await?
            .into_iter()
            .map(|mut token| {
                // Set default gas if empty
                // TODO: Check if this interferes with simulation logic
                if token.gas.is_empty() {
                    token.gas = vec![Some(44000_u64)];
                }
                (token.address.clone(), token)
            })
            .collect::<HashMap<_, Token>>();

        Ok(res)
    }

    /// Gets traced entry points from the RPC server
    pub async fn get_traced_entry_points(
        &self,
        protocol_system: &str,
        component_ids: Vec<String>,
        chain: Chain,
    ) -> Result<HashMap<String, Vec<(EntryPointWithTracingParams, TracingResult)>>, RpcError> {
        self.http_client
            .get_traced_entry_points(
                TracedEntryPointsParams::new(chain, protocol_system)
                    .with_component_ids(component_ids),
            )
            .await
            .map(|page| page.into_data())
            .map_err(RpcError::from)
    }

    pub async fn get_snapshots(
        &self,
        chain: Chain,
        block_number: u64,
        protocol_system: &str,
        components: &HashMap<ComponentId, ProtocolComponent>,
        contract_ids: &[Bytes],
        entrypoints: &HashMap<String, Vec<(EntryPointWithTracingParams, TracingResult)>>,
    ) -> Result<Snapshot, RpcError> {
        let params =
            SnapshotParameters::new(chain, protocol_system, components, contract_ids, block_number)
                .entrypoints(entrypoints);

        let chunk_size = 100;
        let concurrency = 1;

        self.http_client
            .get_snapshots(&params, Some(chunk_size), concurrency)
            .await
            .map_err(RpcError::from)
    }

    /// Waits for the protocol to be synced and have components available
    pub async fn wait_for_protocol_sync(
        &self,
        protocol_system: &str,
        chain: Chain,
    ) -> Result<(), RpcError> {
        loop {
            match self
                .get_protocol_components(protocol_system, chain)
                .await
            {
                Ok(components) => {
                    info!("Found {} components for protocol {}", components.len(), protocol_system);
                    if !components.is_empty() {
                        return Ok(());
                    }
                    info!(
                        "Protocol {} found but no components available yet, waiting...",
                        protocol_system
                    );
                }
                Err(e) => {
                    warn!(
                        "Failed to get protocol components for {}: {}. Retrying in 15 minutes...",
                        protocol_system, e
                    );
                }
            }

            sleep(Duration::from_secs(15 * 60)).await;
        }
    }
}
