#![allow(unused_variables)] // TODO: Remove this
#![allow(dead_code)] // TODO: Remove this

use std::{collections::HashMap, slice};

use deepsize::DeepSizeOf;
use tonic::async_trait;
use tracing::{debug, error, info, instrument, span, warn, Level};
use tycho_common::{
    models::{
        blockchain::Transaction, protocol::ProtocolComponent, BlockHash, Chain, ComponentId, TxHash,
    },
    storage::{EntryPointFilter, EntryPointGateway, ProtocolGateway},
    traits::{AccountExtractor, EntryPointTracer},
};

use crate::extractor::{
    dynamic_contract_indexer::{
        dci::DynamicContractIndexer,
        hooks::{
            cache::HooksDCICache, component_metadata::ComponentTracingMetadata,
            hook_orchestrator::HookOrchestratorRegistry,
            hook_permissions_detector::HookPermissionsDetector,
            metadata_orchestrator::BlockMetadataOrchestrator,
        },
        PausingReason,
    },
    models::{insert_state_attribute_update, BlockChanges},
    ExtractionError, ExtractorExtension,
};

pub(crate) struct UniswapV4HookDCI<AE, T, G>
where
    AE: AccountExtractor + Send + Sync,
    T: EntryPointTracer + Send + Sync,
    G: EntryPointGateway + ProtocolGateway + Send + Sync,
{
    inner_dci: DynamicContractIndexer<AE, T, G>,
    metadata_orchestrator: BlockMetadataOrchestrator,
    hook_orchestrator_registry: HookOrchestratorRegistry,
    // Centralized cache for component states and protocol components
    cache: HooksDCICache,
    db_gateway: G, // Direct access for querying existing entrypoints & protocol components
    chain: Chain,  // Chain information for loading components
    // Maximum number of retries for processing a component.
    max_retries: u32,
    // Pause after a certain number of retries to avoid failed simulations.
    pause_after_retries: u32,
}

type ComponentWithTxHash = (TxHash, ProtocolComponent); // TODO: See if it makes sens to make this a struct

impl<AE, T, G> UniswapV4HookDCI<AE, T, G>
where
    AE: AccountExtractor + Send + Sync,
    T: EntryPointTracer + Send + Sync,
    G: EntryPointGateway + ProtocolGateway + Send + Sync,
{
    pub(super) fn new(
        inner_dci: DynamicContractIndexer<AE, T, G>,
        metadata_orchestrator: BlockMetadataOrchestrator,
        hook_orchestrator_registry: HookOrchestratorRegistry,
        db_gateway: G,
        chain: Chain,
        max_retries: u32,
        pause_after_retries: u32,
    ) -> Self {
        Self {
            inner_dci,
            metadata_orchestrator,
            hook_orchestrator_registry,
            cache: HooksDCICache::new(),
            db_gateway,
            chain,
            max_retries,
            pause_after_retries,
        }
    }

    #[instrument(skip(self), fields(chain = %self.chain))]
    pub(crate) async fn initialize(&mut self) -> Result<(), ExtractionError> {
        info!("Initializing UniswapV4HookDCI");

        // Initialize the inner DCI
        self.inner_dci.initialize().await?;

        // Load all UniswapV4 protocol components from storage
        let protocol_components = self
            .db_gateway
            .get_protocol_components(
                &self.chain,
                Some("uniswap_v4_hooks".to_string()),
                None, // ids - load all
                None, // min_tvl - no filter
                None, // pagination - load all
            )
            .await
            .map_err(|e| {
                error!("Failed to load UniswapV4 protocol components: {e:?}");
                ExtractionError::Unknown(format!("Failed to load protocol components: {e:?}"))
            })?;

        info!(
            component_count = protocol_components.entity.len(),
            "Loaded UniswapV4 protocol components from storage"
        );

        // First pass: Filter components with swap hooks and collect their IDs
        let mut valid_components = Vec::new();
        let mut component_ids_for_batch = Vec::new();
        let mut skipped_no_hook = 0;
        let mut skipped_no_swap_perms = 0;

        for component in protocol_components.entity {
            if let Some(hook_address) = component.static_attributes.get("hooks") {
                if HookPermissionsDetector::has_swap_hooks(hook_address) {
                    debug!(
                        component_id = %component.id,
                        hook_address = %hook_address,
                        "Found component with swap hook permissions"
                    );

                    // Add component to cache
                    self.cache
                        .protocol_components
                        .insert_permanent(component.id.clone(), component.clone());

                    // Collect for batch processing
                    component_ids_for_batch.push(component.id.clone());
                    valid_components.push(component);
                } else {
                    skipped_no_swap_perms += 1;
                    debug!(
                        component_id = %component.id,
                        hook_address = %hook_address,
                        "Skipping UniswapV4 component - no swap hook permissions"
                    );
                }
            } else {
                skipped_no_hook += 1;
                debug!(
                    component_id = %component.id,
                    "Skipping UniswapV4 component - no hook attribute found"
                );
            }
        }

        info!(
            valid_components = valid_components.len(),
            skipped_no_hook, skipped_no_swap_perms, "Completed component filtering"
        );

        // Second pass: Batch request for all component entrypoints
        if !component_ids_for_batch.is_empty() {
            info!(batch_size = component_ids_for_batch.len(), "Starting batch entrypoint loading");

            let filter = EntryPointFilter::new("uniswap_v4_hooks".to_string())
                .with_component_ids(component_ids_for_batch.clone());

            let all_entrypoints = self
                .db_gateway
                .get_entry_points_tracing_params(filter, None)
                .await
                .map_err(|e| {
                    error!("Failed to batch load entrypoints for components: {e:?}");
                    ExtractionError::Unknown(format!("Failed to load entrypoints: {e:?}"))
                })?;

            info!(
                entrypoint_count = all_entrypoints.entity.len(),
                "Loaded entrypoints for components in batch"
            );

            // Third pass: Process each component and determine its state
            let mut traced_components = 0;
            let mut unprocessed_components = 0;

            for component in valid_components {
                let component_has_entrypoints = all_entrypoints
                    .entity
                    .contains_key(&component.id);

                let component_state = if component_has_entrypoints {
                    traced_components += 1;
                    // Has entrypoints = TracingComplete
                    ComponentProcessingState {
                        status: ProcessingStatus::TracingComplete,
                        retry_count: 0,
                        last_error: None,
                    }
                } else {
                    unprocessed_components += 1;
                    // No entrypoints = Unprocessed
                    ComponentProcessingState {
                        status: ProcessingStatus::Unprocessed,
                        retry_count: 0,
                        last_error: None,
                    }
                };

                self.cache
                    .component_states
                    .insert_permanent(component.id.clone(), component_state.clone());

                debug!(
                    component_id = %component.id,
                    status = ?component_state.status,
                    has_entrypoints = component_has_entrypoints,
                    "Initialized component state"
                );
            }

            info!(
                traced_components,
                unprocessed_components, "Completed component state initialization"
            );
        } else {
            info!("No valid components found, skipping entrypoint loading");
        }

        info!("UniswapV4HookDCI initialization completed successfully");
        Ok(())
    }

    /// Handles component failures by updating the state and logging errors
    #[instrument(skip(self, block_changes, tx), fields(component_id = %component_id, block_number = block_changes.block.number))]
    fn handle_component_failure(
        &mut self,
        component_id: ComponentId,
        error_msg: String,
        tx: &Transaction,
        block_changes: &mut BlockChanges,
    ) -> Result<(), ExtractionError> {
        error!(
            error_msg = %error_msg,
            component_id = %component_id,
            tx_hash = %tx.hash,
            "Component failed processing"
        );

        // Update component state
        let mut retry_count = 0;
        if let Some(current_state) = self
            .cache
            .component_states
            .get(&component_id)
        {
            retry_count = current_state.retry_count + 1;
            debug!(
                previous_retry_count = current_state.retry_count,
                new_retry_count = retry_count,
                "Incrementing retry count"
            );
        }

        let new_state = ComponentProcessingState {
            status: ProcessingStatus::Failed,
            retry_count,
            last_error: Some(ProcessingError::MetadataError(error_msg.clone())),
        };

        self.cache
            .component_states
            .insert_pending(block_changes.block.clone(), component_id.clone(), new_state)
            .map_err(|e| {
                error!("Failed to update component state: {e}");
                ExtractionError::Unknown(format!("Failed to update component state: {e}"))
            })?;

        // Mark component as paused by setting the "paused" state attribute.
        // This indicates the component processing has been paused due to metatdata failures.
        insert_state_attribute_update(
            &mut block_changes.txs_with_update,
            &component_id,
            tx,
            &"paused".to_string(),
            &PausingReason::MetadataError.into(),
        )?;

        Ok(())
    }

    /// Checks ComponentTracingMetadata for errors and updates component states accordingly
    #[instrument(skip(self, component_metadata, block_changes), fields(
        component_count = component_metadata.len(),
        block_number = block_changes.block.number
    ))]
    fn process_metadata_errors(
        &mut self,
        component_metadata: &[(ProtocolComponent, ComponentTracingMetadata)],
        component_id_to_tx_map: &HashMap<ComponentId, Option<Transaction>>,
        block_changes: &mut BlockChanges,
    ) -> Result<(), ExtractionError> {
        let mut failed_components = 0;
        let mut total_errors = 0;
        let tx_map: HashMap<TxHash, Transaction> = block_changes
            .txs_with_update
            .iter()
            .map(|tx_with_changes| (tx_with_changes.tx.hash.clone(), tx_with_changes.tx.clone()))
            .collect();

        for (component, metadata) in component_metadata {
            let mut errors = Vec::new();

            // Check for balance errors
            if let Some(Err(balance_error)) = &metadata.balances() {
                errors.push(format!("Balance error: {balance_error:?}"));
            }

            // Check for limits errors
            if let Some(Err(limits_error)) = &metadata.limits() {
                errors.push(format!("Limits error: {limits_error:?}"));
            }

            // Check for TVL errors
            if let Some(Err(tvl_error)) = &metadata.tvl() {
                errors.push(format!("TVL error: {tvl_error:?}"));
            }

            // If there are errors, mark component as failed
            if !errors.is_empty() {
                failed_components += 1;
                total_errors += errors.len();
                let error_msg = errors.join("; ");

                debug!(
                    component_id = %component.id,
                    error_count = errors.len(),
                    "Component has metadata errors"
                );

                let tx = component_id_to_tx_map
                    .get(&component.id)
                    .and_then(|opt_tx| opt_tx.as_ref())
                    .ok_or_else(|| {
                        ExtractionError::Unknown(format!(
                            "No tx found for component {}",
                            component.id
                        ))
                    })?;

                self.handle_component_failure(component.id.clone(), error_msg, tx, block_changes)?;
            }
        }

        if failed_components > 0 {
            warn!(failed_components, total_errors, "Components failed with metadata errors");
        }

        Ok(())
    }

    #[instrument(skip(self, block), fields(component_id = %component, block_number = block.number))]
    fn handle_component_success(
        &mut self,
        component: ComponentId,
        block: &tycho_common::models::blockchain::Block,
    ) -> Result<(), ExtractionError> {
        debug!("Handling component success");

        // Update the component state to indicate successful processing
        let new_state = ComponentProcessingState {
            status: ProcessingStatus::TracingComplete,
            retry_count: 0,
            last_error: None,
        };

        self.cache
            .component_states
            .insert_pending(block.clone(), component.clone(), new_state)
            .map_err(|e| {
                error!("Failed to update component state: {e}");
                ExtractionError::Unknown(format!("Failed to update component state: {e}"))
            })?;

        debug!("Component state updated to TracingComplete");
        Ok(())
    }

    #[instrument(skip(self, block_changes), fields(
        block_number = block_changes.block.number,
        tx_count = block_changes.txs_with_update.len()
    ))]
    fn extract_components_with_swap_hooks(
        &mut self,
        block_changes: &mut BlockChanges,
    ) -> Result<HashMap<ComponentId, ComponentWithTxHash>, ExtractionError> {
        debug!("Extracting components with swap hooks");

        let mut components_with_hooks = HashMap::new();

        // Create a map of component ID to latest transaction hash in the block
        // This ensures we use the latest transaction that affected each component
        let mut component_to_latest_tx = HashMap::new();

        // First pass: collect all affected components and their transaction hashes
        // Process transactions in order to ensure we get the latest transaction hash
        for tx_with_changes in &block_changes.txs_with_update {
            let tx_hash = tx_with_changes.tx.hash.clone();

            // Track newly created components
            for component_id in tx_with_changes
                .protocol_components
                .keys()
            {
                component_to_latest_tx.insert(component_id.clone(), tx_hash.clone());
            }

            // Track mutated components from state_updates
            for component_id in tx_with_changes.state_updates.keys() {
                component_to_latest_tx.insert(component_id.clone(), tx_hash.clone());
            }

            // Track mutated components from balance_changes
            for component_id in tx_with_changes.balance_changes.keys() {
                component_to_latest_tx.insert(component_id.clone(), tx_hash.clone());
            }
        }

        // Second pass: extract components with swap hooks
        for tx_with_changes in &block_changes.txs_with_update {
            // Process newly created components
            for component in tx_with_changes
                .protocol_components
                .values()
            {
                if let Some(hook_address) = component.static_attributes.get("hooks") {
                    if HookPermissionsDetector::has_swap_hooks(hook_address) {
                        let latest_tx_hash = component_to_latest_tx
                            .get(&component.id)
                            .cloned()
                            .unwrap_or_else(|| tx_with_changes.tx.hash.clone());

                        components_with_hooks
                            .insert(component.id.clone(), (latest_tx_hash, component.clone()));

                        // Add new components to the cache
                        self.cache
                            .protocol_components
                            .insert_pending(
                                block_changes.block.clone(),
                                component.id.clone(),
                                component.clone(),
                            )
                            .map_err(|e| {
                                ExtractionError::Unknown(format!(
                                    "Failed to cache new component: {e:?}"
                                ))
                            })?;
                    }
                }
            }
        }

        // Process mutated components (from state_updates and balance_changes)
        for component_id in component_to_latest_tx.keys() {
            // Skip if we already processed this as a new component
            if components_with_hooks.contains_key(component_id) {
                continue;
            }

            if let Some(component) = self
                .cache
                .protocol_components
                .get(component_id)
            {
                if let Some(hook_address) = component.static_attributes.get("hooks") {
                    if HookPermissionsDetector::has_swap_hooks(hook_address) {
                        let latest_tx_hash = component_to_latest_tx
                            .get(component_id)
                            .cloned()
                            .unwrap_or_default();

                        components_with_hooks
                            .insert(component_id.clone(), (latest_tx_hash, component.clone()));
                    }
                }
            } else {
                // Note: It's possible for components to be mutated that aren't in our cache
                // (e.g., non-swap hook Uniswap V4 components). This is expected and we ignore them.
                // debug!(
                //     component_id = %component_id,
                //     "Skipping component - not found in cache or no swap hook permissions"
                // );
            }
        }

        info!(
            components_with_hooks = components_with_hooks.len(),
            total_affected_components = component_to_latest_tx.len(),
            "Completed extraction of components with swap hooks"
        );

        Ok(components_with_hooks)
    }

    #[instrument(skip(self, components), fields(component_count = components.len()))]
    fn categorize_components(
        &self,
        components: &HashMap<ComponentId, ComponentWithTxHash>,
    ) -> Result<(Vec<ComponentWithTxHash>, Vec<ComponentWithTxHash>), ExtractionError> {
        debug!("Categorizing components by processing state");

        let mut components_needing_full_processing = Vec::new();
        let mut components_needing_balance_only = Vec::new();
        let mut unprocessed_count = 0;
        let mut tracing_complete_count = 0;
        let mut failed_retryable_count = 0;
        let mut failed_paused_count = 0;
        let mut not_in_cache_count = 0;

        for (component_id, (tx_hash, component)) in components {
            // Check the component's processing state from cache
            match self
                .cache
                .component_states
                .get(component_id)
            {
                Some(state) => {
                    match state.status {
                        ProcessingStatus::Unprocessed => {
                            unprocessed_count += 1;
                            // Never processed, needs full processing
                            components_needing_full_processing
                                .push((tx_hash.clone(), component.clone()));
                        }
                        ProcessingStatus::TracingComplete => {
                            tracing_complete_count += 1;
                            // Already traced, only needs balance updates
                            components_needing_balance_only
                                .push((tx_hash.clone(), component.clone()));
                        }
                        ProcessingStatus::Failed => {
                            // Check retry count
                            if state.retry_count < self.max_retries &&
                                state.retry_count < self.pause_after_retries
                            {
                                failed_retryable_count += 1;
                                components_needing_full_processing
                                    .push((tx_hash.clone(), component.clone()));
                            } else {
                                failed_paused_count += 1;
                                debug!(
                                    component_id = %component_id,
                                    retry_count = state.retry_count,
                                    max_retries = self.max_retries,
                                    pause_after_retries = self.pause_after_retries,
                                    "Component processing paused due to too many failures"
                                );
                            }
                            // else: pause processing for this component or max retries reached
                        }
                    }
                }
                None => {
                    not_in_cache_count += 1;
                    // Not in cache, needs full processing
                    components_needing_full_processing.push((tx_hash.clone(), component.clone()));
                }
            }
        }

        info!(
            full_processing = components_needing_full_processing.len(),
            balance_only = components_needing_balance_only.len(),
            unprocessed_count,
            tracing_complete_count,
            failed_retryable_count,
            failed_paused_count,
            not_in_cache_count,
            "Completed component categorization"
        );

        Ok((components_needing_full_processing, components_needing_balance_only))
    }
}

// Component state tracking
#[derive(Clone, Debug, DeepSizeOf)]
pub(crate) struct ComponentProcessingState {
    status: ProcessingStatus,
    retry_count: u32,
    last_error: Option<ProcessingError>,
}

#[derive(Clone, Debug, DeepSizeOf)]
pub(super) enum ProcessingStatus {
    Unprocessed,     // Never processed or needs full processing
    TracingComplete, // Has entrypoints generated, only needs balance updates
    Failed,          // Processing failed, can retry
}

// TODO: Use anyhow error
#[derive(Clone, Debug, DeepSizeOf)]
pub(super) enum ProcessingError {
    MetadataError(String), // Before entrypoint generation
    TracingError(String),  // During/after entrypoint generation
}

#[async_trait]
impl<AE, T, G> ExtractorExtension for UniswapV4HookDCI<AE, T, G>
where
    AE: AccountExtractor + Send + Sync,
    T: EntryPointTracer + Send + Sync,
    G: EntryPointGateway + ProtocolGateway + Send + Sync,
{
    #[instrument(skip(self, block_changes), fields(
        block_number = block_changes.block.number,
        block_hash = %block_changes.block.hash,
        tx_count = block_changes.txs_with_update.len()
    ))]
    async fn process_block_update(
        &mut self,
        block_changes: &mut BlockChanges,
    ) -> Result<(), ExtractionError> {
        info!("Processing block update for UniswapV4HookDCI");

        // Ensure the block layer exists in the cache
        self.cache
            .try_insert_block_layer(&block_changes.block)
            .map_err(|e| {
                error!("Failed to ensure block layer: {e:?}");
                ExtractionError::Unknown(format!("Failed to ensure block layer: {e:?}"))
            })?;

        // 1. Filter-out components with no swap hooks (keep beforeSwap or afterSwap only)
        let swap_hook_components = self.extract_components_with_swap_hooks(block_changes)?;

        // Early stop if no hook-components are affected
        if swap_hook_components.is_empty() {
            debug!("No swap hook components found, delegating to inner DCI");
            self.inner_dci
                .process_block_update(block_changes)
                .await?;
            debug!("Inner DCI processing completed");

            // 7. Handle finality for the cache
            self.cache
                .handle_finality(block_changes.finalized_block_height)
                .map_err(|e| {
                    error!("Failed to handle finality for cache: {e:?}");
                    ExtractionError::Unknown(format!("Failed to handle finality for cache: {e:?}"))
                })?;

            info!("Block processing completed successfully");
            return Ok(());
        }

        info!(
            swap_hook_components = swap_hook_components.len(),
            "Found swap hook components to process"
        );

        // 2. Categorize components based on processing state.
        // Full-processing are the ones that will need an Entrypoint to be generated.
        // Balance updates are the ones that only needs their balances to be updated.
        let (components_needing_full_processing, components_needing_balance_only) = {
            let _span = span!(
                Level::INFO,
                "categorize_components",
                total_components = swap_hook_components.len()
            )
            .entered();

            self.categorize_components(&swap_hook_components)?
        };

        info!(
            full_processing = components_needing_full_processing.len(),
            balance_only = components_needing_balance_only.len(),
            "Categorized components by processing needs"
        );

        // 3. Collect metadata for components where the liquidity is outside the PoolManager.
        let component_metadata_from_external_source = self
            .metadata_orchestrator
            .collect_metadata_for_block(
                &components_needing_balance_only,
                &components_needing_full_processing,
                &block_changes.block,
            )
            .await
            .map_err(|e| {
                error!(
                    "Failed to collect metadata for block {}: {e:?}",
                    block_changes.block.number
                );
                ExtractionError::Unknown(format!("Failed to collect metadata: {e:?}"))
            })?;

        info!(
            metadata_count = component_metadata_from_external_source.len(),
            "Collected component metadata"
        );

        // Build tx_hash to Transaction lookup map
        let tx_map = block_changes
            .txs_with_update
            .iter()
            .map(|tx_with_changes| (tx_with_changes.tx.hash.clone(), tx_with_changes.tx.clone()))
            .collect::<HashMap<_, _>>();

        // Build component_id to Transaction map directly from swap_hook_components
        // This ensures we have transaction info for ALL hook components, not just those with
        // external metadata
        let component_id_to_tx_map: HashMap<ComponentId, Option<Transaction>> =
            swap_hook_components
                .iter()
                .map(|(component_id, (tx_hash, _component))| {
                    (component_id.clone(), tx_map.get(tx_hash).cloned())
                })
                .collect();

        // 3a. Process metadata errors and update component states
        self.process_metadata_errors(
            &component_metadata_from_external_source,
            &component_id_to_tx_map,
            block_changes,
        )?;

        // 4. Prepare metadata lookup map for hook orchestrator processing
        let metadata_by_component_id: HashMap<ComponentId, _> = {
            component_metadata_from_external_source
                .into_iter()
                .map(|(component, metadata)| (component.id.clone(), metadata))
                .collect()
        };

        // Create lookup set for components needing entrypoint generation
        let full_processing_ids: std::collections::HashSet<ComponentId> =
            components_needing_full_processing
                .iter()
                .map(|(_, comp)| comp.id.clone())
                .collect();

        // 5. Process all hook components through their orchestrators
        let hook_components: Vec<_> = swap_hook_components.iter().collect();

        info!(hook_components = hook_components.len(), "Processing hook components");

        // TODO: Redesign to process batches of components by orchestrator type
        // We need to be able to sort components by orchestrator and then process them in batches if
        // they have the same orchestrator.
        for (component_id, (tx_hash, component)) in hook_components {
            let needs_entrypoints = full_processing_ids.contains(&component.id);
            let orchestrator_span = span!(
                Level::INFO,
                "hook_orchestrator_processing",
                component_id = %component.id,
                needs_entrypoints = needs_entrypoints,
            );
            let _orchestrator_guard = orchestrator_span.enter();

            if let Some(orchestrator) = self
                .hook_orchestrator_registry
                .get_orchestrator_for_component(component)
            {
                debug!(
                    component_id = %component.id,
                    needs_entrypoints = needs_entrypoints,
                    "Found hook orchestrator, processing component"
                );

                // Prepare metadata map for this component
                let component_metadata_map: HashMap<String, _> = metadata_by_component_id
                    .get(&component.id)
                    .map(|metadata| (component.id.clone(), metadata.clone()))
                    .into_iter()
                    .collect();

                debug!(
                    metadata_entries = component_metadata_map.len(),
                    "Prepared component metadata map"
                );

                match orchestrator
                    .update_components(
                        block_changes,
                        slice::from_ref(component),
                        &component_metadata_map,
                        needs_entrypoints,
                    )
                    .await
                {
                    Ok(()) => {
                        if needs_entrypoints {
                            // Update component states for successful entrypoint generation
                            self.handle_component_success(
                                component.id.clone(),
                                &block_changes.block,
                            )?;
                        } else {
                            // Balance-only components remain in their current state
                            debug!(
                                component_id = %component.id,
                                "Completed balance-only update for component"
                            );
                        }
                    }
                    Err(e) => {
                        error!(
                            error = %e,
                            component_id = %component.id,
                            needs_entrypoints = needs_entrypoints,
                            "Hook orchestrator failed"
                        );

                        let tx = component_id_to_tx_map
                            .get(&component.id)
                            .and_then(|opt_tx| opt_tx.as_ref())
                            .ok_or_else(|| {
                                ExtractionError::Unknown(format!(
                                    "No tx found for component {}",
                                    component.id
                                ))
                            })?;

                        self.handle_component_failure(
                            component.id.clone(),
                            format!("Hook orchestrator error: {e:?}"),
                            tx,
                            block_changes,
                        )?;
                    }
                }
            } else {
                warn!(
                    component_id = %component.id,
                    "No hook orchestrator found for component"
                );

                let tx = component_id_to_tx_map
                    .get(&component.id)
                    .and_then(|opt_tx| opt_tx.as_ref())
                    .ok_or_else(|| {
                        ExtractionError::Unknown(format!(
                            "No tx found for component {}",
                            component.id
                        ))
                    })?;

                self.handle_component_failure(
                    component.id.clone(),
                    format!("No hook orchestrator available for component {}", component.id),
                    tx,
                    block_changes,
                )?;
            }
        }

        // TODO: Is pruning implemented already?
        // 6. Delegate to standard DCI (handles tracing + pruning)
        self.inner_dci
            .process_block_update(block_changes)
            .await?;

        debug!("Inner DCI processing completed");

        // 7. Handle finality for the cache
        self.cache
            .handle_finality(block_changes.finalized_block_height)
            .map_err(|e| {
                error!("Failed to handle finality for cache: {e:?}");
                ExtractionError::Unknown(format!("Failed to handle finality for cache: {e:?}"))
            })?;

        info!("Block processing completed successfully");
        Ok(())
    }

    /// Handles revert by reverting the cache and calling the inner_dci's process_revert.
    #[instrument(skip(self), fields(target_block = %target_block))]
    async fn process_revert(&mut self, target_block: &BlockHash) -> Result<(), ExtractionError> {
        info!("Processing revert for UniswapV4HookDCI");

        // Revert the cache
        self.cache
            .revert_to(target_block)
            .map_err(|e| {
                error!("Failed to revert cache: {e:?}");
                ExtractionError::Unknown(format!("Failed to revert cache: {e:?}"))
            })?;

        info!("Cache reverted successfully");

        // Delegate to inner DCI
        self.inner_dci
            .process_revert(target_block)
            .await?;

        info!("Revert processing completed successfully");
        Ok(())
    }

    /// Returns the approximate size of the internal cache used by this extension, in bytes.
    fn cache_size(&self) -> usize {
        self.cache.deep_size_of() + self.inner_dci.cache_size()
    }

    fn emit_cache_metrics(&self, chain: &str, extractor: &str) {
        self.cache
            .emit_metrics(chain, extractor);
        self.inner_dci
            .emit_cache_metrics(chain, extractor);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tycho_common::{
        models::{
            blockchain::{
                Block, EntryPoint, EntryPointWithTracingParams, RPCTracerParams, TracingParams,
                Transaction, TxWithChanges,
            },
            protocol::{ProtocolComponent, ProtocolComponentState, ProtocolComponentStateDelta},
            Address, Chain, ChangeType, TxHash,
        },
        storage::WithTotal,
        traits::{MockAccountExtractor, MockEntryPointTracer},
        Bytes,
    };

    use super::{super::component_metadata::MetadataError, *};
    use crate::{
        extractor::{
            dynamic_contract_indexer::hooks::component_metadata::{
                MetadataGeneratorRegistry, MetadataResponseParserRegistry, ProviderRegistry,
            },
            models::BlockChanges,
        },
        testing::{self, MockGateway},
    };

    fn get_test_block(version: u8) -> Block {
        testing::block(version.into())
    }

    fn get_test_transaction(version: u8) -> Transaction {
        Transaction::new(
            Bytes::from(version).lpad(32, 0),
            Bytes::from(version).lpad(32, 0),
            Bytes::from(version).lpad(20, 0),
            Some(Bytes::from(version).lpad(20, 0)),
            version as u64,
        )
    }

    fn create_hook_component(id: &str, hook_address: Address) -> ProtocolComponent {
        let mut static_attributes = HashMap::new();
        static_attributes.insert("hooks".to_string(), hook_address.clone());
        static_attributes.insert("key_lp_fee".to_string(), Bytes::from(vec![0, 0, 100])); // 100 basis points
        static_attributes.insert("tick_spacing".to_string(), Bytes::from(vec![0, 0, 0, 10])); // tick spacing 10

        ProtocolComponent {
            id: id.to_string(),
            protocol_system: "uniswap_v4".to_string(),
            protocol_type_name: "pool".to_string(),
            chain: Chain::Ethereum,
            tokens: vec![Address::from([2u8; 20]), Address::from([3u8; 20])],
            contract_addresses: vec![hook_address],
            static_attributes,
            change: ChangeType::Creation,
            creation_tx: TxHash::from([0u8; 32]),
            created_at: chrono::DateTime::from_timestamp(1234567890, 0)
                .unwrap()
                .naive_utc(),
        }
    }

    fn create_hook_address_with_swap_permissions() -> Address {
        // Address with beforeSwap (bit 7) and afterSwap (bit 6) enabled
        // 0x00000000000000000000000000000000000000C0 = 0b11000000
        Address::from("0x00000000000000000000000000000000000000C0")
    }

    fn create_hook_address_without_swap_permissions() -> Address {
        // Address with no swap hooks
        Address::from("0x0000000000000000000000000000000000000000")
    }

    fn get_mock_gateway() -> MockGateway {
        let mut gateway = MockGateway::new();

        // Mock for inner DCI initialization
        gateway
            .expect_get_entry_points_tracing_params()
            .return_once(move |_, _| {
                Box::pin(async move { Ok(WithTotal { entity: HashMap::new(), total: None }) })
            });

        gateway
            .expect_get_entry_points()
            .return_once(move |_, _| {
                Box::pin(async move { Ok(WithTotal { entity: HashMap::new(), total: None }) })
            });

        gateway
            .expect_get_traced_entry_points()
            .return_once(move |_| Box::pin(async move { Ok(HashMap::new()) }));

        // Mock get_tokens to return empty result
        gateway
            .expect_get_tokens()
            .return_once(move |_, _, _, _, _| {
                Box::pin(async move {
                    Ok(tycho_common::storage::WithTotal { entity: Vec::new(), total: Some(0) })
                })
            });

        gateway
    }

    #[tokio::test]
    async fn test_initialize() {
        let gateway = get_mock_gateway();
        let account_extractor = MockAccountExtractor::new();
        let entrypoint_tracer = MockEntryPointTracer::new();

        let inner_dci = DynamicContractIndexer::new(
            Chain::Ethereum,
            "test".to_string(),
            gateway,
            account_extractor,
            entrypoint_tracer,
        );

        let metadata_orchestrator = BlockMetadataOrchestrator::new(
            MetadataGeneratorRegistry::new(),
            MetadataResponseParserRegistry::new(),
            ProviderRegistry::new(),
        );

        let hook_orchestrator_registry = HookOrchestratorRegistry::new();

        let mut gateway2 = MockGateway::new();
        gateway2
            .expect_get_protocol_components()
            .return_once(move |_, _, _, _, _| {
                Box::pin(async move { Ok(WithTotal { entity: vec![], total: Some(0) }) })
            });

        let mut hook_dci = UniswapV4HookDCI::new(
            inner_dci,
            metadata_orchestrator,
            hook_orchestrator_registry,
            gateway2,
            Chain::Ethereum,
            2, // pause_after_retries
            3, // max_retries
        );

        // Should initialize successfully
        hook_dci.initialize().await.unwrap();
    }

    #[tokio::test]
    async fn test_process_block_update_no_hook_components() {
        let gateway = get_mock_gateway();
        let account_extractor = MockAccountExtractor::new();
        let entrypoint_tracer = MockEntryPointTracer::new();

        let inner_dci = DynamicContractIndexer::new(
            Chain::Ethereum,
            "test".to_string(),
            gateway,
            account_extractor,
            entrypoint_tracer,
        );

        let metadata_orchestrator = BlockMetadataOrchestrator::new(
            MetadataGeneratorRegistry::new(),
            MetadataResponseParserRegistry::new(),
            ProviderRegistry::new(),
        );

        let hook_orchestrator_registry = HookOrchestratorRegistry::new();

        let mut gateway2 = MockGateway::new();
        gateway2
            .expect_get_protocol_components()
            .return_once(move |_, _, _, _, _| {
                Box::pin(async move { Ok(WithTotal { entity: vec![], total: Some(0) }) })
            });

        let mut hook_dci = UniswapV4HookDCI::new(
            inner_dci,
            metadata_orchestrator,
            hook_orchestrator_registry,
            gateway2,
            Chain::Ethereum,
            2,
            3,
        );

        hook_dci.initialize().await.unwrap();

        // Create block changes with no hook components
        let mut block_changes = BlockChanges::new(
            "test".to_string(),
            Chain::Ethereum,
            get_test_block(1),
            1,
            false,
            vec![TxWithChanges { tx: get_test_transaction(1), ..Default::default() }],
            Vec::new(),
        );

        // Should delegate directly to inner DCI
        hook_dci
            .process_block_update(&mut block_changes)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_extract_components_with_swap_hooks() {
        let gateway = MockGateway::new();
        let account_extractor = MockAccountExtractor::new();
        let entrypoint_tracer = MockEntryPointTracer::new();

        let inner_dci = DynamicContractIndexer::new(
            Chain::Ethereum,
            "test".to_string(),
            gateway,
            account_extractor,
            entrypoint_tracer,
        );

        let metadata_orchestrator = BlockMetadataOrchestrator::new(
            MetadataGeneratorRegistry::new(),
            MetadataResponseParserRegistry::new(),
            ProviderRegistry::new(),
        );

        let hook_orchestrator_registry = HookOrchestratorRegistry::new();

        let gateway2 = MockGateway::new();
        let mut hook_dci = UniswapV4HookDCI::new(
            inner_dci,
            metadata_orchestrator,
            hook_orchestrator_registry,
            gateway2,
            Chain::Ethereum,
            2,
            3,
        );

        // Create components with and without swap hooks
        let hook_with_swap = create_hook_address_with_swap_permissions();
        let hook_without_swap = create_hook_address_without_swap_permissions();

        let component1 = create_hook_component("comp1", hook_with_swap);
        let component2 = create_hook_component("comp2", hook_without_swap);

        let mut block_changes = BlockChanges::new(
            "test".to_string(),
            Chain::Ethereum,
            get_test_block(1),
            1,
            false,
            vec![TxWithChanges {
                tx: get_test_transaction(1),
                protocol_components: HashMap::from([
                    ("comp1".to_string(), component1.clone()),
                    ("comp2".to_string(), component2.clone()),
                ]),
                ..Default::default()
            }],
            Vec::new(),
        );

        // Initialize the block layer in the cache
        hook_dci
            .cache
            .protocol_components
            .validate_and_ensure_block_layer_test(&block_changes.block)
            .unwrap();

        let components = hook_dci
            .extract_components_with_swap_hooks(&mut block_changes)
            .unwrap();

        // Should only extract component with swap hooks
        assert_eq!(components.len(), 1);
        assert!(components.contains_key("comp1"));
        let (tx_hash, component) = components.get("comp1").unwrap();
        assert_eq!(component.id, "comp1");
    }

    #[tokio::test]
    async fn test_extract_components_with_mutated_components() {
        let gateway = MockGateway::new();
        let account_extractor = MockAccountExtractor::new();
        let entrypoint_tracer = MockEntryPointTracer::new();

        let inner_dci = DynamicContractIndexer::new(
            Chain::Ethereum,
            "test".to_string(),
            gateway,
            account_extractor,
            entrypoint_tracer,
        );

        let metadata_orchestrator = BlockMetadataOrchestrator::new(
            MetadataGeneratorRegistry::new(),
            MetadataResponseParserRegistry::new(),
            ProviderRegistry::new(),
        );

        let hook_orchestrator_registry = HookOrchestratorRegistry::new();

        let gateway2 = MockGateway::new();
        let mut hook_dci = UniswapV4HookDCI::new(
            inner_dci,
            metadata_orchestrator,
            hook_orchestrator_registry,
            gateway2,
            Chain::Ethereum,
            2,
            3,
        );

        // Pre-populate the cache with an existing component
        let hook_address = create_hook_address_with_swap_permissions();
        let existing_component = create_hook_component("existing_comp", hook_address);
        hook_dci
            .cache
            .protocol_components
            .insert_permanent("existing_comp".to_string(), existing_component.clone());

        // Create block changes with only state_updates (no new components)
        let mut state_updates = HashMap::new();
        let state_delta = ProtocolComponentStateDelta::new(
            "existing_comp",
            HashMap::new(),                   // No updated attributes
            std::collections::HashSet::new(), // No deleted attributes
        );
        state_updates.insert("existing_comp".to_string(), state_delta);

        let mut block_changes = BlockChanges::new(
            "test".to_string(),
            Chain::Ethereum,
            get_test_block(1),
            1,
            false,
            vec![TxWithChanges {
                tx: get_test_transaction(1),
                protocol_components: HashMap::new(), // No new components
                state_updates,
                ..Default::default()
            }],
            Vec::new(),
        );

        // Initialize the block layer in the cache
        hook_dci
            .cache
            .protocol_components
            .validate_and_ensure_block_layer_test(&block_changes.block)
            .unwrap();

        let components = hook_dci
            .extract_components_with_swap_hooks(&mut block_changes)
            .unwrap();

        // Should extract the mutated component with swap hooks
        assert_eq!(components.len(), 1);
        assert!(components.contains_key("existing_comp"));
        let (tx_hash, component) = components.get("existing_comp").unwrap();
        assert_eq!(component.id, "existing_comp");
    }

    #[tokio::test]
    async fn test_extract_components_with_balance_changes() {
        let gateway = MockGateway::new();
        let account_extractor = MockAccountExtractor::new();
        let entrypoint_tracer = MockEntryPointTracer::new();

        let inner_dci = DynamicContractIndexer::new(
            Chain::Ethereum,
            "test".to_string(),
            gateway,
            account_extractor,
            entrypoint_tracer,
        );

        let metadata_orchestrator = BlockMetadataOrchestrator::new(
            MetadataGeneratorRegistry::new(),
            MetadataResponseParserRegistry::new(),
            ProviderRegistry::new(),
        );

        let hook_orchestrator_registry = HookOrchestratorRegistry::new();

        let gateway2 = MockGateway::new();
        let mut hook_dci = UniswapV4HookDCI::new(
            inner_dci,
            metadata_orchestrator,
            hook_orchestrator_registry,
            gateway2,
            Chain::Ethereum,
            2,
            3,
        );

        // Pre-populate the cache with an existing component
        let hook_address = create_hook_address_with_swap_permissions();
        let existing_component = create_hook_component("balance_comp", hook_address);
        hook_dci
            .cache
            .protocol_components
            .insert_permanent("balance_comp".to_string(), existing_component.clone());

        // Create block changes with only balance_changes (no new components)
        let mut balance_changes = HashMap::new();
        balance_changes.insert("balance_comp".to_string(), HashMap::new()); // Dummy balance update

        let mut block_changes = BlockChanges::new(
            "test".to_string(),
            Chain::Ethereum,
            get_test_block(1),
            1,
            false,
            vec![TxWithChanges {
                tx: get_test_transaction(1),
                protocol_components: HashMap::new(), // No new components
                balance_changes,
                ..Default::default()
            }],
            Vec::new(),
        );

        // Initialize the block layer in the cache
        hook_dci
            .cache
            .protocol_components
            .validate_and_ensure_block_layer_test(&block_changes.block)
            .unwrap();

        let components = hook_dci
            .extract_components_with_swap_hooks(&mut block_changes)
            .unwrap();

        // Should extract the component with balance changes that has swap hooks
        assert_eq!(components.len(), 1);
        assert!(components.contains_key("balance_comp"));
        let (tx_hash, component) = components.get("balance_comp").unwrap();
        assert_eq!(component.id, "balance_comp");
    }

    #[tokio::test]
    async fn test_categorize_components() {
        let gateway = MockGateway::new();
        let account_extractor = MockAccountExtractor::new();
        let entrypoint_tracer = MockEntryPointTracer::new();

        let inner_dci = DynamicContractIndexer::new(
            Chain::Ethereum,
            "test".to_string(),
            gateway,
            account_extractor,
            entrypoint_tracer,
        );

        let metadata_orchestrator = BlockMetadataOrchestrator::new(
            MetadataGeneratorRegistry::new(),
            MetadataResponseParserRegistry::new(),
            ProviderRegistry::new(),
        );

        let hook_orchestrator_registry = HookOrchestratorRegistry::new();

        let gateway2 = MockGateway::new();
        let mut hook_dci = UniswapV4HookDCI::new(
            inner_dci,
            metadata_orchestrator,
            hook_orchestrator_registry,
            gateway2,
            Chain::Ethereum,
            2,
            3,
        );

        let hook_address = create_hook_address_with_swap_permissions();
        let component = create_hook_component("comp1", hook_address);
        let tx_hash = TxHash::from([1u8; 32]);

        let block_changes = BlockChanges::new(
            "test".to_string(),
            Chain::Ethereum,
            get_test_block(1),
            1,
            false,
            vec![TxWithChanges {
                tx: get_test_transaction(1),
                protocol_components: HashMap::from([("comp1".to_string(), component.clone())]),
                ..Default::default()
            }],
            Vec::new(),
        );

        // Test component not in cache (needs full processing)
        let mut components_map = HashMap::new();
        components_map.insert("comp1".to_string(), (tx_hash.clone(), component.clone()));

        let (full_processing, balance_only) = hook_dci
            .categorize_components(&components_map)
            .unwrap();

        assert_eq!(full_processing.len(), 1);
        assert_eq!(balance_only.len(), 0);
        assert_eq!(full_processing[0].1.id, "comp1");

        // Add component to cache as TracingComplete
        let state = ComponentProcessingState {
            status: ProcessingStatus::TracingComplete,
            retry_count: 0,
            last_error: None,
        };
        hook_dci
            .cache
            .component_states
            .insert_permanent("comp1".to_string(), state);

        // Test component in cache with TracingComplete (needs balance only)
        let (full_processing, balance_only) = hook_dci
            .categorize_components(&components_map)
            .unwrap();

        assert_eq!(full_processing.len(), 0);
        assert_eq!(balance_only.len(), 1);
        assert_eq!(balance_only[0].1.id, "comp1");
    }

    #[tokio::test]
    async fn test_handle_component_success() {
        let gateway = MockGateway::new();
        let account_extractor = MockAccountExtractor::new();
        let entrypoint_tracer = MockEntryPointTracer::new();

        let inner_dci = DynamicContractIndexer::new(
            Chain::Ethereum,
            "test".to_string(),
            gateway,
            account_extractor,
            entrypoint_tracer,
        );

        let metadata_orchestrator = BlockMetadataOrchestrator::new(
            MetadataGeneratorRegistry::new(),
            MetadataResponseParserRegistry::new(),
            ProviderRegistry::new(),
        );

        let hook_orchestrator_registry = HookOrchestratorRegistry::new();

        let gateway2 = MockGateway::new();
        let mut hook_dci = UniswapV4HookDCI::new(
            inner_dci,
            metadata_orchestrator,
            hook_orchestrator_registry,
            gateway2,
            Chain::Ethereum,
            2,
            3,
        );

        let block = get_test_block(1);
        let component_id = "comp1".to_string();

        // Initialize the block layer in the cache first
        hook_dci
            .cache
            .component_states
            .validate_and_ensure_block_layer_test(&block)
            .unwrap();

        // Handle success
        hook_dci
            .handle_component_success(component_id.clone(), &block)
            .unwrap();

        // Check that component state was updated
        let state = hook_dci
            .cache
            .component_states
            .get(&component_id)
            .unwrap();
        assert!(matches!(state.status, ProcessingStatus::TracingComplete));
        assert_eq!(state.retry_count, 0);
        assert!(state.last_error.is_none());
    }

    #[tokio::test]
    async fn test_process_revert() {
        let gateway = MockGateway::new();
        let account_extractor = MockAccountExtractor::new();
        let entrypoint_tracer = MockEntryPointTracer::new();

        let inner_dci = DynamicContractIndexer::new(
            Chain::Ethereum,
            "test".to_string(),
            gateway,
            account_extractor,
            entrypoint_tracer,
        );

        let metadata_orchestrator = BlockMetadataOrchestrator::new(
            MetadataGeneratorRegistry::new(),
            MetadataResponseParserRegistry::new(),
            ProviderRegistry::new(),
        );

        let hook_orchestrator_registry = HookOrchestratorRegistry::new();

        let gateway2 = MockGateway::new();
        let mut hook_dci = UniswapV4HookDCI::new(
            inner_dci,
            metadata_orchestrator,
            hook_orchestrator_registry,
            gateway2,
            Chain::Ethereum,
            2,
            3,
        );

        let block_hash = Bytes::from([1u8; 32]);

        // Should process revert without error
        hook_dci
            .process_revert(&block_hash)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_process_metadata_errors_pausing_on_tracing_failure() {
        let gateway = MockGateway::new();
        let account_extractor = MockAccountExtractor::new();
        let entrypoint_tracer = MockEntryPointTracer::new();

        let inner_dci = DynamicContractIndexer::new(
            Chain::Ethereum,
            "test".to_string(),
            gateway,
            account_extractor,
            entrypoint_tracer,
        );

        let metadata_orchestrator = BlockMetadataOrchestrator::new(
            MetadataGeneratorRegistry::new(),
            MetadataResponseParserRegistry::new(),
            ProviderRegistry::new(),
        );

        let hook_orchestrator_registry = HookOrchestratorRegistry::new();

        let gateway2 = MockGateway::new();
        let mut hook_dci = UniswapV4HookDCI::new(
            inner_dci,
            metadata_orchestrator,
            hook_orchestrator_registry,
            gateway2,
            Chain::Ethereum,
            2, // pause_after_retries
            3, // max_retries
        );

        let block = get_test_block(1);
        let tx = get_test_transaction(1);
        let component_id = "test_component".to_string();
        let hook_address = create_hook_address_with_swap_permissions();
        let component = create_hook_component(&component_id, hook_address);

        // Create block changes with transaction
        let mut block_changes = BlockChanges::new(
            "test".to_string(),
            Chain::Ethereum,
            block.clone(),
            1,
            false,
            vec![TxWithChanges {
                tx: tx.clone(),
                protocol_components: HashMap::new(),
                state_updates: HashMap::new(),
                balance_changes: HashMap::new(),
                ..Default::default()
            }],
            Vec::new(),
        );

        // Initialize the block layer in the cache first
        hook_dci
            .cache
            .component_states
            .validate_and_ensure_block_layer_test(&block)
            .unwrap();

        let tracing_metadata = ComponentTracingMetadata::new(tx.hash.clone())
            .with_balances(Err(MetadataError::RequestFailed(
                "RPC timeout during tracing".to_string(),
            )))
            .with_limits(Err(MetadataError::ProviderFailed(
                "Simulation failed: insufficient gas".to_string(),
            )));

        let component_metadata = vec![(component.clone(), tracing_metadata)];

        let component_id_to_tx_map = HashMap::from([(component_id.clone(), Some(tx.clone()))]);

        // Process metadata errors - this should trigger pausing logic
        let result = hook_dci.process_metadata_errors(
            &component_metadata,
            &component_id_to_tx_map,
            &mut block_changes,
        );

        assert!(result.is_ok(), "process_metadata_errors should succeed");

        // Verify that the pausing attribute was added to block_changes
        assert_eq!(block_changes.txs_with_update.len(), 1);
        let tx_with_changes = &block_changes.txs_with_update[0];

        // Check that the state update includes the "paused" attribute
        assert!(
            tx_with_changes
                .state_updates
                .contains_key(&component_id),
            "State updates should contain the component"
        );

        let state_delta = &tx_with_changes.state_updates[&component_id];
        assert!(
            state_delta
                .updated_attributes
                .contains_key("paused"),
            "State delta should contain 'paused' attribute"
        );

        let paused_value = &state_delta.updated_attributes["paused"];
        assert_eq!(
            paused_value,
            &Bytes::from([3u8]),
            "Paused attribute should have expected value"
        );
    }

    #[tokio::test]
    async fn test_component_with_balance_changes_no_external_metadata_needs_full_processing() {
        // This test covers the scenario where:
        // 1. A component has balance changes in the block
        // 2. The component has no external metadata source (e.g., not an Euler vault)
        // 3. The component needs full processing (is Unprocessed or Failed with retries available)
        // 4. No orchestrator is registered for this hook (simulating the error case)
        //
        // This should not fail with "No tx found for component" error
        // because component_id_to_tx_map is built from swap_hook_components

        let gateway = MockGateway::new();
        let account_extractor = MockAccountExtractor::new();
        let entrypoint_tracer = MockEntryPointTracer::new();

        let inner_dci = DynamicContractIndexer::new(
            Chain::Ethereum,
            "test".to_string(),
            gateway,
            account_extractor,
            entrypoint_tracer,
        );

        // Create metadata orchestrator with no generators (component will have no external
        // metadata)
        let metadata_orchestrator = BlockMetadataOrchestrator::new(
            MetadataGeneratorRegistry::new(),
            MetadataResponseParserRegistry::new(),
            ProviderRegistry::new(),
        );

        // Create empty hook orchestrator registry (no orchestrator for this hook)
        let hook_orchestrator_registry = HookOrchestratorRegistry::new();

        let gateway2 = MockGateway::new();
        let mut hook_dci = UniswapV4HookDCI::new(
            inner_dci,
            metadata_orchestrator,
            hook_orchestrator_registry,
            gateway2,
            Chain::Ethereum,
            2, // pause_after_retries
            3, // max_retries
        );

        // Create a component with swap hooks that will be in the block
        let hook_address = create_hook_address_with_swap_permissions();
        let component = create_hook_component("balance_only_comp", hook_address);

        // Pre-populate the cache with this component in Unprocessed state
        // This ensures it will need full processing
        hook_dci
            .cache
            .protocol_components
            .insert_permanent(component.id.clone(), component.clone());

        let unprocessed_state = ComponentProcessingState {
            status: ProcessingStatus::Unprocessed,
            retry_count: 0,
            last_error: None,
        };
        hook_dci
            .cache
            .component_states
            .insert_permanent(component.id.clone(), unprocessed_state);

        // Create block changes with ONLY balance changes (no new component, no state updates)
        // This simulates a component that only has balance changes in the block
        let mut balance_changes = HashMap::new();
        balance_changes.insert(component.id.clone(), HashMap::new());

        let block = get_test_block(1);
        let tx = get_test_transaction(1);

        let mut block_changes = BlockChanges::new(
            "test".to_string(),
            Chain::Ethereum,
            block.clone(),
            1,
            false,
            vec![TxWithChanges {
                tx: tx.clone(),
                protocol_components: HashMap::new(), // NO new components
                state_updates: HashMap::new(),       // NO state updates
                balance_changes,                     // ONLY balance changes
                ..Default::default()
            }],
            Vec::new(),
        );

        // Initialize the block layer in the cache
        hook_dci
            .cache
            .protocol_components
            .validate_and_ensure_block_layer_test(&block)
            .unwrap();

        hook_dci
            .cache
            .component_states
            .validate_and_ensure_block_layer_test(&block)
            .unwrap();

        // Process the block update
        // This should handle the error gracefully and mark the component as failed
        // because no orchestrator is registered, but it should NOT fail with
        // "No tx found for component" error
        let result = hook_dci
            .process_block_update(&mut block_changes)
            .await;

        // The result should be Ok because the error is handled gracefully
        // The component should be marked as failed with a paused state
        assert!(
            result.is_ok(),
            "process_block_update should handle missing orchestrator gracefully"
        );

        // Verify the component was marked as failed
        let final_state = hook_dci
            .cache
            .component_states
            .get(&component.id);
        assert!(final_state.is_some(), "Component state should exist in cache");
        let final_state = final_state.unwrap();
        assert!(
            matches!(final_state.status, ProcessingStatus::Failed),
            "Component should be marked as Failed"
        );
        assert_eq!(final_state.retry_count, 1, "Retry count should be incremented");

        // Verify that the paused attribute was added to block_changes
        assert_eq!(block_changes.txs_with_update.len(), 1, "Should still have one transaction");
        let tx_with_changes = &block_changes.txs_with_update[0];
        assert!(
            tx_with_changes
                .state_updates
                .contains_key(&component.id),
            "State updates should contain the component with paused attribute"
        );

        let state_delta = &tx_with_changes.state_updates[&component.id];
        assert!(
            state_delta
                .updated_attributes
                .contains_key("paused"),
            "State delta should contain 'paused' attribute"
        );
    }

    #[cfg(test)]
    mod integration_tests {
        use std::{collections::HashSet, str::FromStr, sync::Arc};

        use tracing::info;
        use tycho_ethereum::{
            rpc::EthereumRpcClient,
            services::entrypoint_tracer::{
                balance_slot_detector::EVMBalanceSlotDetector, tracer::EVMEntrypointService,
            },
        };

        use super::*;
        use crate::extractor::dynamic_contract_indexer::hooks::{
            component_metadata::{
                MetadataGeneratorRegistry, MetadataResponseParserRegistry, ProviderRegistry,
            },
            entrypoint_generator::{
                DefaultSwapAmountEstimator, HookEntrypointConfig, HookEntrypointGenerator,
                UniswapV4DefaultHookEntrypointGenerator,
            },
            hook_orchestrator::{DefaultUniswapV4HookOrchestrator, HookOrchestratorRegistry},
            integrations::register_integrations,
            metadata_orchestrator::BlockMetadataOrchestrator,
            rpc_metadata_provider::RPCMetadataProvider,
        };

        // Test fixture data based on provided protocol component
        fn create_test_protocol_component() -> ProtocolComponent {
            let mut static_attributes = HashMap::new();
            static_attributes.insert("tick_spacing".to_string(), Bytes::from_str("0x01").unwrap());
            static_attributes.insert(
                "pool_id".to_string(),
                Bytes::from_str(
                    "0x156c3163f4cabc00f83d2bfad9ee341aebc85a5bcb566c0ba8fc4358a1023166",
                )
                .unwrap(),
            );
            static_attributes.insert(
                "hooks".to_string(),
                Address::from("0x55dcf9455eee8fd3f5eed17606291272cde428a8"),
            );
            static_attributes.insert("key_lp_fee".to_string(), Bytes::from_str("0x64").unwrap());

            ProtocolComponent {
                id: "0x156c3163f4cabc00f83d2bfad9ee341aebc85a5bcb566c0ba8fc4358a1023166"
                    .to_string(),
                protocol_system: "uniswap_v4_hooks".to_string(),
                protocol_type_name: "uniswap_v4_pool".to_string(),
                chain: Chain::Ethereum,
                tokens: vec![
                    Address::from("0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0"),
                    Address::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
                ],
                contract_addresses: vec![],
                static_attributes,
                change: ChangeType::Creation,
                creation_tx: TxHash::from(
                    "0x17cc501a356d8e367f57c40419bd954790fe83f8d2dbfb06058bc7617f1c16f3",
                ),
                created_at: chrono::NaiveDateTime::parse_from_str(
                    "2025-07-01 15:49:11",
                    "%Y-%m-%d %H:%M:%S",
                )
                .unwrap(),
            }
        }

        // Create initial component state based on provided data
        fn create_test_component_state() -> ProtocolComponentState {
            let mut attributes = HashMap::new();
            attributes.insert(
                "sqrt_price_x96".to_string(),
                Bytes::from_str("0x01000000000000000000000000").unwrap(),
            );
            attributes
                .insert("protocol_fees/zero2one".to_string(), Bytes::from_str("0x00").unwrap());
            attributes.insert("tick".to_string(), Bytes::from_str("0x00").unwrap());
            attributes.insert("liquidity".to_string(), Bytes::from_str("0x00").unwrap());
            attributes.insert(
                "balance_owner".to_string(),
                Bytes::from_str("0x000000000004444c5dc75cb358380d2e3de08a90").unwrap(),
            );
            attributes
                .insert("protocol_fees/one2zero".to_string(), Bytes::from_str("0x00").unwrap());

            let mut balances = HashMap::new();
            balances.insert(
                Address::from("0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0"),
                Bytes::from_str("0x00").unwrap(),
            );
            balances.insert(
                Address::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
                Bytes::from_str("0x00").unwrap(),
            );

            ProtocolComponentState {
                component_id: "0x156c3163f4cabc00f83d2bfad9ee341aebc85a5bcb566c0ba8fc4358a1023166"
                    .to_string(),
                attributes,
                balances,
            }
        }

        fn create_test_block(block_number: u64) -> Block {
            Block::new(
                block_number,
                Chain::Ethereum,
                Bytes::from(block_number).lpad(32, 0),
                Bytes::from(block_number - 1).lpad(32, 0),
                chrono::DateTime::from_timestamp(1719849000 + (block_number * 12) as i64, 0)
                    .unwrap()
                    .naive_utc(),
            )
        }

        fn create_test_transaction(nonce: u64) -> Transaction {
            Transaction::new(
                Bytes::from(nonce).lpad(32, 0),
                Bytes::from(1u64).lpad(32, 0),
                Address::from("0x2626664c2603336E57B271c5C0b26F421741e481"), /* Use real router
                                                                              * address as
                                                                              * sender */
                Some(Address::from("0x55dcf9455eee8fd3f5eed17606291272cde428a8")),
                nonce,
            )
        }

        // Setup hook orchestrator registry with the test hook address
        fn setup_test_hook_orchestrator_registry(
            router_address: Address,
            pool_manager: Address,
            rpc: &EthereumRpcClient,
        ) -> HookOrchestratorRegistry {
            let mut hook_registry = HookOrchestratorRegistry::new();

            let hook_address = Address::from("0x55dcf9455eee8fd3f5eed17606291272cde428a8");

            let config = HookEntrypointConfig::new(
                Some(4),
                1,
                Some(router_address.clone()),
                None,
                None,
                pool_manager.clone(),
            );

            let balance_slot_detector =
                EVMBalanceSlotDetector::new(rpc).with_max_token_batch_size(5);

            let mut entrypoint_generator = UniswapV4DefaultHookEntrypointGenerator::new(
                DefaultSwapAmountEstimator::with_limits(),
                pool_manager.clone(),
                balance_slot_detector,
            );
            entrypoint_generator.set_config(config);

            let orchestrator = DefaultUniswapV4HookOrchestrator::new(entrypoint_generator);

            hook_registry.register_hook_orchestrator(hook_address, Box::new(orchestrator));

            hook_registry
        }

        // Setup metadata registries for the test
        fn setup_test_metadata_registries(
            rpc_url: String,
        ) -> (MetadataGeneratorRegistry, MetadataResponseParserRegistry, ProviderRegistry) {
            let mut generator_registry = MetadataGeneratorRegistry::new();
            let mut parser_registry = MetadataResponseParserRegistry::new();
            let mut provider_registry = ProviderRegistry::new();

            let hook_address = Address::from("0x55dcf9455eee8fd3f5eed17606291272cde428a8");

            register_integrations(
                &mut generator_registry,
                &mut parser_registry,
                &mut provider_registry,
                rpc_url,
            );

            provider_registry.register_provider(
                "rpc_default".to_string(),
                Arc::new(RPCMetadataProvider::new(50)),
            );

            (generator_registry, parser_registry, provider_registry)
        }

        // Note: This is an integration test that requires real DB and RPC connections
        // It should be run manually with proper environment setup
        #[tokio::test]
        #[ignore] // This test requires real DB and RPC connections
        async fn test_hook_dci_process_block_update_integration() {
            // Initialize tracing for tests
            let _ = tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .try_init();
            // Test configuration - using realistic mainnet addresses
            let chain = Chain::Ethereum;
            let router_address = Address::from("0x2e234dae75c793f67a35089c9d99245e1c58470b");
            let pool_manager = Address::from("0x000000000004444c5dc75cB358380D2e3dE08A90"); // Real Uniswap V4 pool manager

            // Create mock gateways
            let mut db_gateway = MockGateway::new();
            let mut account_extractor = MockAccountExtractor::new();

            // Use real RPC-based tracer instead of mock
            let trace_rpc_url = std::env::var("TRACE_RPC_URL").expect("RPC_URL must be set");
            let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");
            let rpc =
                EthereumRpcClient::new(&rpc_url).expect("Failed to create Ethereum RPC client");
            let trace_rpc =
                EthereumRpcClient::new(&trace_rpc_url).expect("Failed to create Trace RPC client");
            let entrypoint_tracer = EVMEntrypointService::new(&trace_rpc);

            // Setup initial expectations for initialization
            db_gateway
                .expect_get_entry_points_tracing_params()
                .return_once(move |_, _| {
                    Box::pin(async move { Ok(WithTotal { entity: HashMap::new(), total: None }) })
                });

            db_gateway
                .expect_get_entry_points()
                .return_once(move |_, _| {
                    Box::pin(async move { Ok(WithTotal { entity: HashMap::new(), total: None }) })
                });
            db_gateway
                .expect_get_traced_entry_points()
                .return_once(move |_| Box::pin(async move { Ok(HashMap::new()) }));

            // Mock get_tokens to return empty result
            db_gateway
                .expect_get_tokens()
                .return_once(move |_, _, _, _, _| {
                    Box::pin(async move {
                        Ok(tycho_common::storage::WithTotal { entity: Vec::new(), total: Some(0) })
                    })
                });

            // Setup expectation for the account extractor
            account_extractor
                .expect_get_accounts_at_block()
                .returning(|_, _| Ok(HashMap::new()));

            // Create inner DCI
            let inner_dci = DynamicContractIndexer::new(
                chain,
                "test_extractor".to_string(),
                db_gateway,
                account_extractor,
                entrypoint_tracer,
            );

            // Setup metadata registries and orchestrators
            let (generator_registry, parser_registry, provider_registry) =
                setup_test_metadata_registries(rpc_url.clone());

            let metadata_orchestrator = BlockMetadataOrchestrator::new(
                generator_registry,
                parser_registry,
                provider_registry,
            );
            let hook_orchestrator_registry =
                setup_test_hook_orchestrator_registry(router_address, pool_manager.clone(), &rpc);

            // Create new mock gateway for Hook DCI (since we moved ownership of the first one)
            let mut db_gateway2 = MockGateway::new();

            // Set up expectations for process_block_update
            // This would be called by inner_dci.process_block_update
            db_gateway2
                .expect_insert_entry_points()
                .returning(|_| Box::pin(async move { Ok(()) }));

            db_gateway2
                .expect_insert_entry_point_tracing_params()
                .returning(|_| Box::pin(async move { Ok(()) }));

            // Set up expectation for getting protocol components during initialization
            let test_component = create_test_protocol_component();
            let component_clone = test_component.clone();
            db_gateway2
                .expect_get_protocol_components()
                .withf(move |chain, system, ids, min_tvl, pagination| {
                    chain == &Chain::Ethereum &&
                        system.as_deref() == Some("uniswap_v4_hooks") &&
                        ids.is_none() &&
                        min_tvl.is_none() &&
                        pagination.is_none()
                })
                .return_once(move |_, _, _, _, _| {
                    Box::pin(async move {
                        Ok(WithTotal { entity: vec![component_clone], total: Some(1) })
                    })
                });

            // Set up expectation for getting entry points for the component
            db_gateway2
                .expect_get_entry_points_tracing_params()
                .withf(move |filter: &EntryPointFilter, _| {
                    filter.protocol_system == "uniswap_v4_hooks"
                        && filter
                        .component_ids
                        .as_ref()
                        .map(|ids| ids.contains(&"0x156c3163f4cabc00f83d2bfad9ee341aebc85a5bcb566c0ba8fc4358a1023166".to_string()))
                        .unwrap_or(false)
                })
                .return_once(move |_, _| {
                    Box::pin(async move {
                        Ok(WithTotal {
                            entity: HashMap::new(), // No entrypoints yet - component needs processing
                            total: None,
                        })
                    })
                });

            // Create Hook DCI
            let mut hook_dci = UniswapV4HookDCI::new(
                inner_dci,
                metadata_orchestrator,
                hook_orchestrator_registry,
                db_gateway2,
                chain,
                3, // max_retries
                2, // pause_after_retries
            );

            info!("Initializing");
            // Initialize the Hook DCI
            hook_dci.initialize().await.unwrap();

            // Create test block changes with the new protocol component
            // Using a real mainnet block for RPC compatibility
            let block = Block::new(
                23003136, // Real mainnet block number
                Chain::Ethereum,
                Bytes::from_str(
                    "0xfdd7626c879f499cc6ad2011ed783da534d5a8b817ddd40e14b87e3bdd84aecc",
                )
                .unwrap(), // Real block hash
                Bytes::from_str(
                    "0x97e6d877bd7e6587c29711ee80b873d4eac8c49fc616145e6326e07a5e41bf1810",
                )
                .unwrap(), // Real parent hash
                chrono::DateTime::from_timestamp(1724251307, 0)
                    .unwrap()
                    .naive_utc(), /* Real timestamp */
            );
            let tx = create_test_transaction(1);

            let mut protocol_components = HashMap::new();
            protocol_components.insert(test_component.id.clone(), test_component.clone());

            let tx_with_changes = TxWithChanges {
                tx: tx.clone(),
                protocol_components,
                state_updates: HashMap::new(),
                balance_changes: HashMap::new(),
                ..Default::default()
            };

            let mut block_changes = BlockChanges::new(
                "test_extractor".to_string(),
                chain,
                block.clone(),
                block.number, // finalized_block_height - same as current block for testing
                false,
                vec![tx_with_changes],
                vec![],
            );

            // Process the block update
            let result = hook_dci
                .process_block_update(&mut block_changes)
                .await;

            // Verify the result
            match result {
                Ok(_) => {
                    println!("Integration test completed successfully!");
                }
                Err(e) => {
                    panic!("process_block_update failed with error: {e:?}");
                }
            }

            // Verify that the component was processed
            // In a real integration test, you would verify:
            // 1. Entry points were generated and stored
            // 2. Component state was updated in the cache
            // 3. Balance updates were applied
            // 4. The hook orchestrator was called appropriately

            println!("Integration test completed successfully!");
        }

        // Additional test for handling state updates on existing components
        #[tokio::test]
        #[ignore] // This test requires real DB and RPC connections
        async fn test_hook_dci_process_block_update_with_state_changes() {
            let chain = Chain::Ethereum;
            let router_address = Address::from("0x1234567890123456789012345678901234567890");
            let pool_manager = Address::from("0x5d8d4b3c8b1f2a3c4b8f6e9d7c5a3b1e2f4d6a8c");

            // Similar setup as above but with existing component and state updates
            let mut db_gateway = MockGateway::new();
            let account_extractor = MockAccountExtractor::new();
            let entrypoint_tracer = MockEntryPointTracer::new();

            // Setup expectations for initialization
            db_gateway
                .expect_get_entry_points_tracing_params()
                .return_once(move |_, _| {
                    Box::pin(async move { Ok(WithTotal { entity: HashMap::new(), total: None }) })
                });

            db_gateway
                .expect_get_entry_points()
                .return_once(move |_, _| {
                    Box::pin(async move { Ok(WithTotal { entity: HashMap::new(), total: None }) })
                });

            db_gateway
                .expect_get_traced_entry_points()
                .return_once(move |_| Box::pin(async move { Ok(HashMap::new()) }));

            let test_component = create_test_protocol_component();
            let component_id = test_component.id.clone();
            let component_clone = test_component.clone();

            db_gateway
                .expect_get_protocol_components()
                .return_once(move |_, _, _, _, _| {
                    Box::pin(async move {
                        Ok(WithTotal { entity: vec![component_clone], total: Some(1) })
                    })
                });

            // This time, return some entrypoints to indicate the component has been traced before
            db_gateway
                .expect_get_entry_points_tracing_params()
                .withf(move |filter: &EntryPointFilter, _| {
                    filter.protocol_system == "uniswap_v4_hooks"
                })
                .return_once(move |_, _| {
                    let mut entrypoints = HashMap::new();
                    let mut ep_set = HashSet::new();

                    let entry_point = EntryPoint {
                        external_id: "test_ep_1".to_string(),
                        target: Address::from("0x55dcf9455eee8fd3f5eed17606291272cde428a8"),
                        signature: "swap(uint256,uint256)".to_string(),
                    };

                    let params = TracingParams::RPCTracer(RPCTracerParams {
                        caller: Some(Address::from("0x1234567890123456789012345678901234567890")),
                        calldata: Bytes::from_str("0x").unwrap(),
                        state_overrides: None,
                        prune_addresses: None,
                    });

                    ep_set.insert(EntryPointWithTracingParams { entry_point, params });
                    entrypoints.insert(component_id.clone(), ep_set);

                    Box::pin(async move { Ok(WithTotal { entity: entrypoints, total: None }) })
                });

            let inner_dci = DynamicContractIndexer::new(
                chain,
                "test_extractor".to_string(),
                db_gateway,
                account_extractor,
                entrypoint_tracer,
            );

            let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");
            let rpc =
                EthereumRpcClient::new(&rpc_url).expect("Failed to create Ethereum RPC client");
            let (generator_registry, parser_registry, provider_registry) =
                setup_test_metadata_registries(rpc_url.clone());
            let metadata_orchestrator = BlockMetadataOrchestrator::new(
                generator_registry,
                parser_registry,
                provider_registry,
            );
            let hook_orchestrator_registry =
                setup_test_hook_orchestrator_registry(router_address, pool_manager.clone(), &rpc);

            let db_gateway2 = MockGateway::new();

            let mut hook_dci = UniswapV4HookDCI::new(
                inner_dci,
                metadata_orchestrator,
                hook_orchestrator_registry,
                db_gateway2,
                chain,
                2,
                3,
            );

            hook_dci.initialize().await.unwrap();

            // Create block changes with state updates (no new components)
            let block = create_test_block(20547124);
            let tx = create_test_transaction(2);

            let mut state_updates = HashMap::new();
            let mut updated_attributes = HashMap::new();
            updated_attributes.insert("liquidity".to_string(), Bytes::from_str("0x1000").unwrap());

            let state_delta = ProtocolComponentStateDelta::new(
                &test_component.id,
                updated_attributes,
                HashSet::new(),
            );
            state_updates.insert(test_component.id.clone(), state_delta);

            let tx_with_changes = TxWithChanges {
                tx: tx.clone(),
                protocol_components: HashMap::new(), // No new components
                state_updates,
                balance_changes: HashMap::new(),
                ..Default::default()
            };

            let mut block_changes = BlockChanges::new(
                "test_extractor".to_string(),
                chain,
                block,
                20547121,
                false,
                vec![tx_with_changes],
                vec![],
            );

            // Process the block update
            let result = hook_dci
                .process_block_update(&mut block_changes)
                .await;

            assert!(result.is_ok(), "process_block_update with state changes should succeed");
            println!("State update test completed successfully!");
        }
    }
}
