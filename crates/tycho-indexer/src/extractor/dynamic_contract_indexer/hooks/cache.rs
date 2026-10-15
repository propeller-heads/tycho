//! The hooks DCI's cache: per-component processing state and the components it tracks, layered
//! by block like the DCI's own cache so reverts and finality apply to it the same way.

use deepsize::DeepSizeOf;
use metrics::gauge;
use tycho_common::models::{
    blockchain::Block, protocol::ProtocolComponent, BlockHash, ComponentId,
};

use crate::extractor::dynamic_contract_indexer::{
    cache::{DCICacheError, MergeStrategy, VersionedCache},
    hooks::hook_dci::ComponentProcessingState,
};

/// Central data cache used by the Hooks Dynamic Contract Indexer (HooksDCI).
#[derive(Debug, DeepSizeOf)]
pub(super) struct HooksDCICache {
    /// Maps component IDs to their processing state.
    pub(super) component_states: VersionedCache<ComponentId, ComponentProcessingState>,
    /// Stores ProtocolComponent data for both newly created and mutated components.
    pub(super) protocol_components: VersionedCache<ComponentId, ProtocolComponent>,
}

impl HooksDCICache {
    pub(super) fn new() -> Self {
        Self { component_states: VersionedCache::new(), protocol_components: VersionedCache::new() }
    }

    /// Reverts the cache to the state at a specific block hash.
    ///
    /// This operation will discard all changes made in blocks after the specified block.
    /// Errors if the block is not found and is not the parent of the latest pending block.
    ///
    /// # Arguments
    /// * `block` - The block to revert to.
    ///
    /// # Returns
    /// * `Ok(())` - On successful reversion
    /// * `Err(DCICacheError::RevertToBlockNotFound)` - If the block is not found in one of the
    ///   pending layers
    pub(super) fn revert_to(&mut self, block: &BlockHash) -> Result<(), DCICacheError> {
        self.component_states.revert_to(block)?;
        self.protocol_components
            .revert_to(block)?;
        Ok(())
    }

    /// Move new finalized blocks state to the permanent layer.
    ///
    /// # Arguments
    /// * `finalized_block_height` - The height of the finalized block.
    pub(super) fn handle_finality(
        &mut self,
        finalized_block_height: u64,
    ) -> Result<(), DCICacheError> {
        self.component_states
            .handle_finality(finalized_block_height, MergeStrategy::Replace)?;
        self.protocol_components
            .handle_finality(finalized_block_height, MergeStrategy::Replace)?;
        Ok(())
    }

    /// Tries to insert a block layer for the given block.
    /// If the block already exists, no-op.
    /// If the block does not exist, we check if it's the next block in the chain. If it's not it
    /// returns an error.
    ///
    /// # Arguments
    /// * `block` - The block to validate and ensure the layer for.
    ///
    /// # Returns
    /// * `Ok(())` - On successful layer creation
    /// * `Err(DCICacheError::UnexpectedBlockOrder)` - If the inserted block is not the correct
    ///   order
    pub(super) fn try_insert_block_layer(&mut self, block: &Block) -> Result<(), DCICacheError> {
        self.component_states
            .validate_and_ensure_block_layer_internal(block)?;
        self.protocol_components
            .validate_and_ensure_block_layer_internal(block)?;
        Ok(())
    }

    /// Emits per-sub-cache metrics for the hooks DCI cache.
    pub(super) fn emit_metrics(&self, chain: &str, extractor: &str) {
        let subcaches: &[(&str, usize, usize)] = &[
            (
                "hooks_component_states",
                self.component_states.deep_size_of(),
                self.component_states.unique_key_count(),
            ),
            (
                "hooks_protocol_components",
                self.protocol_components.deep_size_of(),
                self.protocol_components
                    .unique_key_count(),
            ),
        ];

        for &(name, size, count) in subcaches {
            gauge!(
                "dci_cache_subcache_size",
                "chain" => chain.to_owned(),
                "extractor" => extractor.to_owned(),
                "subcache" => name.to_string(),
            )
            .set(size as f64);
            gauge!(
                "dci_cache_subcache_key_count",
                "chain" => chain.to_owned(),
                "extractor" => extractor.to_owned(),
                "subcache" => name.to_string(),
            )
            .set(count as f64);
        }
    }
}
