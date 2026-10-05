use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use chrono::NaiveDateTime;
use diesel_async::{
    pooled_connection::deadpool::Pool, scoped_futures::ScopedFutureExt, AsyncConnection,
    AsyncPgConnection,
};
use tracing::instrument;
use tycho_common::{
    models::{
        self,
        blockchain::{
            Block, EntryPoint, EntryPointWithTracingParams, TracedEntryPoint, TracedEntryPoints,
            TracingParams, TracingResult, Transaction,
        },
        contract::{Account, AccountBalance, AccountDelta},
        protocol::{
            ComponentBalance, ProtocolComponent, ProtocolComponentState,
            ProtocolComponentStateDelta, QualityRange,
        },
        token::Token,
        Address, Chain, ComponentId, ContractId, EntryPointId, ExtractionState, PaginationParams,
        ProtocolType, TxHash,
    },
    storage::{
        BlockIdentifier, BlockOrTimestamp, ChainGateway, ContractStateGateway, EntryPointFilter,
        EntryPointGateway, ExtractionStateGateway, Gateway, ProtocolGateway, StorageError, Version,
        WithTotal,
    },
    Bytes,
};

use super::{PostgresError, PostgresGateway};

#[derive(Clone)]
pub struct DirectGateway {
    pool: Pool<AsyncPgConnection>,
    state_gateway: PostgresGateway,
    chain: Chain,
}

impl DirectGateway {
    #[allow(private_interfaces)]
    pub fn new(
        pool: Pool<AsyncPgConnection>,
        state_gateway: PostgresGateway,
        chain: Chain,
    ) -> Self {
        DirectGateway { pool, state_gateway, chain }
    }

    pub async fn get_delta(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        end_version: &BlockOrTimestamp,
    ) -> Result<
        (
            Vec<models::contract::AccountDelta>,
            Vec<models::protocol::ProtocolComponentStateDelta>,
            Vec<models::protocol::ComponentBalance>,
        ),
        StorageError,
    > {
        if start_version.is_none() {
            tracing::warn!("Get delta called with start_version = None, this might be a bug in one of the extractors")
        }

        // Fetch the delta from the database
        let mut db = self.pool.get().await.unwrap();
        let accounts_delta = self
            .state_gateway
            .get_accounts_delta(chain, start_version, end_version, &mut db)
            .await?;
        let protocol_delta = self
            .state_gateway
            .get_protocol_states_delta(chain, start_version, end_version, &mut db)
            .await?;
        let balance_deltas = self
            .state_gateway
            .get_balance_deltas(chain, start_version, end_version, &mut db)
            .await?;

        Ok((accounts_delta, protocol_delta, balance_deltas))
    }
}

#[async_trait]
impl ExtractionStateGateway for DirectGateway {
    #[instrument(skip_all)]
    async fn get_state(&self, name: &str, chain: &Chain) -> Result<ExtractionState, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_state(name, chain, &mut conn)
            .await
    }
    #[instrument(skip_all)]
    async fn save_state(&self, new: &ExtractionState) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .save_state(&new.clone(), &mut conn)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl ChainGateway for DirectGateway {
    #[instrument(skip_all)]
    async fn upsert_block(&self, new: &[Block]) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .upsert_block(new.to_vec().as_slice(), &mut conn)
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_block(&self, id: &BlockIdentifier) -> Result<Block, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_block(id, &mut conn)
            .await
    }

    async fn upsert_tx(&self, new: &[Transaction]) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .upsert_tx(new.to_vec().as_slice(), &mut conn)
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_tx(&self, hash: &TxHash) -> Result<Transaction, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_tx(hash, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn revert_state(&self, to: &BlockIdentifier) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .revert_state(to, &mut conn)
            .await
    }
}

#[async_trait]
impl ContractStateGateway for DirectGateway {
    #[instrument(skip_all)]
    async fn get_contract(
        &self,
        id: &ContractId,
        version: Option<&Version>,
        include_slots: bool,
    ) -> Result<Account, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_contract(id, version, include_slots, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_contracts(
        &self,
        chain: &Chain,
        addresses: Option<&[Address]>,
        version: Option<&Version>,
        include_slots: bool,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<Account>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_contracts(chain, addresses, version, include_slots, pagination_params, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn insert_contract(&self, new: &Account) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .insert_contract(&new.clone(), &mut conn)
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn update_contracts(&self, new: &[(TxHash, AccountDelta)]) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        let binding = new.to_vec();
        let collected_changes: Vec<(TxHash, &models::contract::AccountDelta)> = binding
            .iter()
            .map(|(tx, update)| (tx.clone(), update))
            .collect();
        let changes_slice = collected_changes.as_slice();
        self.state_gateway
            .update_contracts(&self.chain, changes_slice, &mut conn)
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn delete_contract(&self, id: &ContractId, at_tx: &TxHash) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .delete_contract(id, at_tx, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_accounts_delta(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        end_version: &BlockOrTimestamp,
    ) -> Result<Vec<AccountDelta>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_accounts_delta(chain, start_version, end_version, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn add_account_balances(
        &self,
        account_balances: &[AccountBalance],
    ) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .add_account_balances(account_balances.to_vec().as_slice(), &self.chain, &mut conn)
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_account_balances(
        &self,
        chain: &Chain,
        addresses: Option<&[Address]>,
        version: Option<&Version>,
    ) -> Result<HashMap<Address, HashMap<Address, AccountBalance>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_account_balances(chain, addresses, version, false, &mut conn)
            .await
    }
}

#[async_trait]
impl ProtocolGateway for DirectGateway {
    #[instrument(skip_all)]
    async fn get_protocol_components(
        &self,
        chain: &Chain,
        system: Option<String>,
        ids: Option<&[&str]>,
        min_tvl: Option<f64>,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<ProtocolComponent>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_protocol_components(chain, system, ids, min_tvl, pagination_params, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_token_owners(
        &self,
        chain: &Chain,
        tokens: &[Address],
        min_balance: Option<f64>,
    ) -> Result<HashMap<Address, (ComponentId, Bytes)>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_token_owners(chain, tokens, min_balance, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn add_protocol_components(&self, new: &[ProtocolComponent]) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .add_protocol_components(new.to_vec().as_slice(), &mut conn)
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn delete_protocol_components(
        &self,
        to_delete: &[ProtocolComponent],
        block_ts: NaiveDateTime,
    ) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .delete_protocol_components(to_delete, block_ts, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn add_protocol_types(
        &self,
        new_protocol_types: &[ProtocolType],
    ) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .add_protocol_types(new_protocol_types, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_protocol_states(
        &self,
        chain: &Chain,
        at: Option<Version>,
        system: Option<String>,
        ids: Option<&[&str]>,
        retrieve_balances: bool,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<ProtocolComponentState>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_protocol_states(
                chain,
                at,
                system,
                ids,
                retrieve_balances,
                pagination_params,
                &mut conn,
            )
            .await
    }

    #[instrument(skip_all)]
    async fn update_protocol_states(
        &self,
        new: &[(TxHash, ProtocolComponentStateDelta)],
    ) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        let deltas = new.to_vec();
        let collected_changes: Vec<(TxHash, &models::protocol::ProtocolComponentStateDelta)> =
            deltas
                .iter()
                .map(|(tx, update)| (tx.clone(), update))
                .collect();
        let changes_slice = collected_changes.as_slice();
        self.state_gateway
            .update_protocol_states(&self.chain, changes_slice, &mut conn)
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_tokens(
        &self,
        chain: Chain,
        address: Option<&[&Address]>,
        quality: QualityRange,
        traded_n_days_ago: Option<NaiveDateTime>,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<Token>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_tokens(chain, address, quality, traded_n_days_ago, pagination_params, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn add_component_balances(
        &self,
        component_balances: &[ComponentBalance],
    ) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .add_component_balances(component_balances.to_vec().as_slice(), &self.chain, &mut conn)
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn add_tokens(&self, tokens: &[Token]) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .add_tokens(tokens.to_vec().as_slice(), &mut conn)
            .await?;
        Ok(())
    }

    /// Updates tokens without using the write cache.
    ///
    /// This method is currently only used by the tycho-ethereum job and therefore does
    /// not use the write cache. It creates a single transaction and executes all
    /// updates immediately.
    ///
    /// ## Note
    /// This is a short term solution. Ideally we should have a simple gateway version
    /// for these use cases that creates a single transactions and emits them immediately.
    #[instrument(skip_all)]
    async fn update_tokens(&self, tokens: &[Token]) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;

        conn.transaction(|conn| {
            async {
                self.state_gateway
                    .update_tokens(tokens, conn)
                    .await?;
                Result::<(), PostgresError>::Ok(())
            }
            .scope_boxed()
        })
        .await
        .map_err(|e| StorageError::Unexpected(format!("Failed to update tokens: {}", e.0)))
    }

    #[instrument(skip_all)]
    async fn get_protocol_states_delta(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        end_version: &BlockOrTimestamp,
    ) -> Result<Vec<ProtocolComponentStateDelta>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_protocol_states_delta(chain, start_version, end_version, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_balance_deltas(
        &self,
        chain: &Chain,
        start_version: Option<&BlockOrTimestamp>,
        target_version: &BlockOrTimestamp,
    ) -> Result<Vec<ComponentBalance>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_balance_deltas(chain, start_version, target_version, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_component_balances(
        &self,
        chain: &Chain,
        ids: Option<&[&str]>,
        version: Option<&Version>,
    ) -> Result<HashMap<String, HashMap<Bytes, ComponentBalance>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_component_balances(chain, ids, version, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_token_prices(&self, chain: &Chain) -> Result<HashMap<Bytes, f64>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_token_prices(chain, &mut conn)
            .await
    }

    /// TODO: add to transaction instead
    #[instrument(skip_all)]
    async fn upsert_component_tvl(
        &self,
        chain: &Chain,
        tvl_values: &HashMap<String, f64>,
    ) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .upsert_component_tvl(chain, tvl_values, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_protocol_systems(
        &self,
        chain: &Chain,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<Vec<String>>, StorageError> {
        self.state_gateway
            .get_protocol_systems(chain, pagination_params)
            .await
    }

    #[instrument(skip_all)]
    async fn get_component_tvls(
        &self,
        chain: &Chain,
        system: Option<String>,
        ids: Option<&[&str]>,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<HashMap<String, f64>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_component_tvls(chain, system, ids, pagination_params, &mut conn)
            .await
    }
}

#[async_trait]
impl EntryPointGateway for DirectGateway {
    #[instrument(skip_all)]
    async fn insert_entry_points(
        &self,
        entry_points: &HashMap<models::ComponentId, HashSet<models::blockchain::EntryPoint>>,
    ) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .insert_entry_points(&entry_points.clone(), &self.chain, &mut conn)
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn insert_entry_point_tracing_params(
        &self,
        entry_points_params: &HashMap<EntryPointId, HashSet<(TracingParams, ComponentId)>>,
    ) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .insert_entry_point_tracing_params(&entry_points_params.clone(), &self.chain, &mut conn)
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_entry_points(
        &self,
        filter: EntryPointFilter,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<HashMap<ComponentId, HashSet<EntryPoint>>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_entry_points(filter, pagination_params, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_entry_points_tracing_params(
        &self,
        filter: EntryPointFilter,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<HashMap<ComponentId, HashSet<EntryPointWithTracingParams>>>, StorageError>
    {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_entry_points_tracing_params(filter, pagination_params, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn upsert_traced_entry_points(
        &self,
        traced_entry_points: &[TracedEntryPoint],
    ) -> Result<(), StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .upsert_traced_entry_points(traced_entry_points.to_vec().as_slice(), &mut conn)
            .await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_traced_entry_points(
        &self,
        entry_points: &HashSet<EntryPointId>,
    ) -> Result<HashMap<EntryPointId, HashMap<TracingParams, TracingResult>>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_tracing_results(entry_points, &mut conn)
            .await
    }

    #[instrument(skip_all)]
    async fn get_traced_entry_points_by_component(
        &self,
        filter: EntryPointFilter,
        pagination_params: Option<&PaginationParams>,
    ) -> Result<WithTotal<TracedEntryPoints>, StorageError> {
        let mut conn =
            self.pool.get().await.map_err(|e| {
                StorageError::Unexpected(format!("Failed to retrieve connection: {e}"))
            })?;
        self.state_gateway
            .get_traced_entry_points_by_component(filter, pagination_params, &mut conn)
            .await
    }
}

impl Gateway for DirectGateway {}
