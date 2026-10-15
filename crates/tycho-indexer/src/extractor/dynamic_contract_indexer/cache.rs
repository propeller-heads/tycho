use std::{
    collections::{hash_map::Entry, HashMap, HashSet, VecDeque},
    hash::Hash,
};

use deepsize::DeepSizeOf;
use metrics::gauge;
use thiserror::Error;
use tracing::{debug, info, trace, warn};
use tycho_common::models::{
    blockchain::{Block, EntryPoint, EntryPointWithTracingParams, TracingParams, TracingResult},
    Address, BlockHash, ComponentId, EntryPointId, StoreKey,
};

/// A unique identifier for a storage location, consisting of an address and a storage key.
type StorageLocation = (Address, StoreKey);

/// A function used for merging values when the same key exists in multiple layers during finality
/// handling.
// Perf: all the merge functions are defined at compile time, so we can avoid the overhead of
// dynamic dispatch.
type MergeFunction<V> = Box<dyn Fn(&V, V) -> V>;

/// Strategy for merging values when the same key exists in multiple layers during finality
/// handling.
pub(super) enum MergeStrategy<V> {
    /// Replace existing values with new ones (default behavior).
    Replace,
    /// Use a custom function to merge values.
    Custom(MergeFunction<V>),
}

/// Central data cache used by the Dynamic Contract Indexer (DCI).
#[derive(Debug, DeepSizeOf)]
pub(super) struct DCICache {
    /// Maps entry point IDs to entry point definitions.
    pub(super) ep_id_to_entrypoint: VersionedCache<EntryPointId, EntryPoint>,
    /// Stores tracing results for entry points paired with specific tracing parameters.
    /// None indicates that the TracingParams exist but have no results (e.g., failed traces).
    pub(super) entrypoint_results:
        VersionedCache<(EntryPointId, TracingParams), Option<TracingResult>>,
    /// Maps a storage location to entry points that should be retriggered when that location
    /// changes and the address storage offset for packed slots.
    pub(super) retriggers:
        VersionedCache<StorageLocation, (HashSet<EntryPointWithTracingParams>, u8)>,
    /// Stores tracked contract addresses and their associated storage keys.
    pub(super) tracked_contracts: VersionedCache<Address, HashSet<StoreKey>>,
    /// Stores addresses identified as ERC-20 tokens to skip full indexing.
    /// perf: implement a versioned wrapper around HashSet, similar to VersionedCache for HashMap
    pub(super) erc20_addresses: VersionedCache<Address, bool>,
    /// Stores manually blacklisted addresses that should skip full indexing
    /// but are not tokens (e.g., UniswapV4 pool manager).
    pub(super) blacklisted_addresses: VersionedCache<Address, bool>,
    /// Maps an entry point id to the component ids that use it.
    pub(super) ep_id_to_component_id: VersionedCache<EntryPointId, HashSet<ComponentId>>,
    /// Maps component IDs to their associated entrypoint params for retry logic.
    pub(super) component_id_to_entrypoint_params:
        VersionedCache<ComponentId, HashSet<EntryPointWithTracingParams>>,
    /// Tracks the number of retry attempts for each failed tracing params.
    /// Used to cap retries at a maximum number of attempts (e.g., 5).
    pub(super) tracing_retry_counts: VersionedCache<(EntryPointId, TracingParams), u32>,
}

impl DCICache {
    pub(super) fn new() -> Self {
        Self {
            ep_id_to_entrypoint: VersionedCache::new(),
            entrypoint_results: VersionedCache::new(),
            retriggers: VersionedCache::new(),
            tracked_contracts: VersionedCache::new(),
            erc20_addresses: VersionedCache::new(),
            blacklisted_addresses: VersionedCache::new(),
            ep_id_to_component_id: VersionedCache::new(),
            component_id_to_entrypoint_params: VersionedCache::new(),
            tracing_retry_counts: VersionedCache::new(),
        }
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
        self.ep_id_to_entrypoint
            .revert_to(block)?;
        self.entrypoint_results
            .revert_to(block)?;
        self.retriggers.revert_to(block)?;
        self.tracked_contracts
            .revert_to(block)?;
        self.erc20_addresses.revert_to(block)?;
        self.blacklisted_addresses
            .revert_to(block)?;
        self.ep_id_to_component_id
            .revert_to(block)?;
        self.component_id_to_entrypoint_params
            .revert_to(block)?;
        self.tracing_retry_counts
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
        self.ep_id_to_entrypoint
            .handle_finality(finalized_block_height, MergeStrategy::Replace)?;
        self.entrypoint_results
            .handle_finality(finalized_block_height, MergeStrategy::Replace)?;
        // Use custom merge function for retriggers to union HashSets
        self.retriggers.handle_finality(
            finalized_block_height,
            MergeStrategy::Custom(Box::new(
                |existing: &(HashSet<EntryPointWithTracingParams>, u8),
                 new: (HashSet<EntryPointWithTracingParams>, u8)| {
                    let (mut merged, previous_offset) = existing.clone();
                    merged.extend(new.0);
                    if previous_offset != new.1 {
                        warn!("Offset discrepancy detected: {previous_offset} != {}", new.1)
                    }
                    (merged, previous_offset)
                },
            )),
        )?;

        // Use custom merge function for tracked_contracts to merge HashSets
        self.tracked_contracts.handle_finality(
            finalized_block_height,
            MergeStrategy::Custom(Box::new(
                |existing: &HashSet<StoreKey>, new: HashSet<StoreKey>| {
                    let mut merged = existing.clone();
                    merged.extend(new);
                    merged
                },
            )),
        )?;

        self.erc20_addresses
            .handle_finality(finalized_block_height, MergeStrategy::Replace)?;
        self.blacklisted_addresses
            .handle_finality(finalized_block_height, MergeStrategy::Replace)?;
        self.ep_id_to_component_id
            .handle_finality(
                finalized_block_height,
                MergeStrategy::Custom(Box::new(
                    |existing: &HashSet<ComponentId>, new: HashSet<ComponentId>| {
                        let mut merged = existing.clone();
                        merged.extend(new);
                        merged
                    },
                )),
            )?;
        self.component_id_to_entrypoint_params
            .handle_finality(
                finalized_block_height,
                MergeStrategy::Custom(Box::new(
                    |existing: &HashSet<EntryPointWithTracingParams>,
                     new: HashSet<EntryPointWithTracingParams>| {
                        let mut merged = existing.clone();
                        merged.extend(new);
                        merged
                    },
                )),
            )?;
        self.tracing_retry_counts
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
        self.ep_id_to_entrypoint
            .validate_and_ensure_block_layer_internal(block)?;
        self.entrypoint_results
            .validate_and_ensure_block_layer_internal(block)?;
        self.retriggers
            .validate_and_ensure_block_layer_internal(block)?;
        self.tracked_contracts
            .validate_and_ensure_block_layer_internal(block)?;
        self.ep_id_to_component_id
            .validate_and_ensure_block_layer_internal(block)?;
        self.erc20_addresses
            .validate_and_ensure_block_layer_internal(block)?;
        self.blacklisted_addresses
            .validate_and_ensure_block_layer_internal(block)?;
        self.component_id_to_entrypoint_params
            .validate_and_ensure_block_layer_internal(block)?;
        self.tracing_retry_counts
            .validate_and_ensure_block_layer_internal(block)?;

        Ok(())
    }

    /// Emits per-sub-cache metrics and logs the top 10 tracked contracts by slot count.
    pub(super) fn emit_metrics(&self, chain: &str, extractor: &str) {
        let subcaches: &[(&str, usize, usize)] = &[
            (
                "tracked_contracts",
                self.tracked_contracts.deep_size_of(),
                self.tracked_contracts
                    .unique_key_count(),
            ),
            ("retriggers", self.retriggers.deep_size_of(), self.retriggers.unique_key_count()),
            (
                "entrypoint_results",
                self.entrypoint_results.deep_size_of(),
                self.entrypoint_results
                    .unique_key_count(),
            ),
            (
                "ep_id_to_entrypoint",
                self.ep_id_to_entrypoint.deep_size_of(),
                self.ep_id_to_entrypoint
                    .unique_key_count(),
            ),
            (
                "erc20_addresses",
                self.erc20_addresses.deep_size_of(),
                self.erc20_addresses.unique_key_count(),
            ),
            (
                "blacklisted_addresses",
                self.blacklisted_addresses
                    .deep_size_of(),
                self.blacklisted_addresses
                    .unique_key_count(),
            ),
            (
                "ep_id_to_component_id",
                self.ep_id_to_component_id
                    .deep_size_of(),
                self.ep_id_to_component_id
                    .unique_key_count(),
            ),
            (
                "component_id_to_entrypoint_params",
                self.component_id_to_entrypoint_params
                    .deep_size_of(),
                self.component_id_to_entrypoint_params
                    .unique_key_count(),
            ),
            (
                "tracing_retry_counts",
                self.tracing_retry_counts.deep_size_of(),
                self.tracing_retry_counts
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

        gauge!(
            "dci_cache_pending_layers",
            "chain" => chain.to_owned(),
            "extractor" => extractor.to_owned(),
        )
        .set(
            self.tracked_contracts
                .pending_layer_count() as f64,
        );

        let mut by_slots: Vec<_> = self
            .tracked_contracts
            .iter_latest()
            .map(|(addr, slots)| (addr, slots.len()))
            .collect();
        by_slots.sort_unstable_by_key(|b| std::cmp::Reverse(b.1));
        by_slots.truncate(10);

        if let Some((_, max_slots)) = by_slots.first() {
            gauge!(
                "dci_cache_tracked_contracts_max_slots",
                "chain" => chain.to_owned(),
                "extractor" => extractor.to_owned(),
            )
            .set(*max_slots as f64);
        }

        if !by_slots.is_empty() {
            let top_contracts: Vec<String> = by_slots
                .iter()
                .map(|(addr, slots)| format!("{addr}:{slots}"))
                .collect();
            info!(
                top_tracked_contracts = %top_contracts.join(", "),
                "DCI cache top tracked contracts by slot count"
            );
        }
    }
}

#[derive(Error, Debug, PartialEq)]
pub enum DCICacheError {
    #[error("Unexpected block order: new block number {0} is not a child of {1}")]
    UnexpectedBlockOrder(u64, u64),
    #[error("Block {0} not found in pending cache in context: {1}")]
    BlockNotFound(String, String),
}

/// A versioned data container scoped to a specific block.
///
/// Stores key-value pairs for a block, used in the pending portion of a cache.
#[derive(Clone, Debug, DeepSizeOf)]
struct BlockScopedMap<K: DeepSizeOf + Eq + Hash, V: DeepSizeOf> {
    /// Block metadata for this layer.
    block: Block,
    /// Key-value store scoped to the block.
    data: HashMap<K, V>,
}

/// A cache structure that supports versioned storage by block.
///
/// It contains:
/// - Permanent data that is not revertable.
/// - Pending data organized in layers per block, supporting reverts.
#[derive(Debug, DeepSizeOf)]
pub(super) struct VersionedCache<K: DeepSizeOf + Hash + Eq, V: DeepSizeOf> {
    /// Entries that are permanent and not affected by block reverts.
    permanent: HashMap<K, V>,
    /// Stack of pending block-scoped layers. These can be reverted.
    pending: VecDeque<BlockScopedMap<K, V>>,
}
impl<K, V> VersionedCache<K, V>
where
    K: Eq + Hash + Clone + std::fmt::Debug + DeepSizeOf,
    V: Clone + std::fmt::Debug + DeepSizeOf,
{
    pub(super) fn new() -> Self {
        Self { permanent: HashMap::new(), pending: VecDeque::new() }
    }

    /// Inserts a key-value pair into the permanent layer.
    pub(super) fn insert_permanent(&mut self, k: K, v: V) {
        self.permanent.insert(k, v);
    }

    /// Inserts a key-value pair into the pending layer for the specified block.
    ///
    /// An error is returned if the layer for the block is not found.
    pub(super) fn insert_pending(&mut self, block: Block, k: K, v: V) -> Result<(), DCICacheError> {
        let layer = self.get_layer_mut(&block)?;
        layer.data.insert(k, v);
        Ok(())
    }

    /// Adds multiple key-value pairs to the permanent layer.
    pub(super) fn extend_permanent(&mut self, entries: impl IntoIterator<Item = (K, V)>) {
        self.permanent.extend(entries);
    }

    /// Adds multiple key-value pairs to the pending layer for the given block.
    ///
    /// An error is returned if the layer for the block is not found.
    pub(super) fn extend_pending(
        &mut self,
        block: Block,
        entries: impl IntoIterator<Item = (K, V)>,
    ) -> Result<(), DCICacheError> {
        let layer = self.get_layer_mut(&block)?;
        layer.data.extend(entries);
        Ok(())
    }

    /// Gets a mutable entry to a value in the permanent layer.
    pub(super) fn permanent_entry<'a>(&'a mut self, k: &K) -> Entry<'a, K, V> {
        self.permanent.entry(k.clone())
    }

    /// Gets a mutable entry to a value in the pending layer for the specified block.
    ///
    /// An error is returned if the layer for the block is not found.
    pub(super) fn pending_entry<'a>(
        &'a mut self,
        block: &Block,
        k: &K,
    ) -> Result<Entry<'a, K, V>, DCICacheError> {
        let layer = self.get_layer_mut(block)?;
        Ok(layer.data.entry(k.clone()))
    }

    /// Retrieves a single value for a key, checking latest pending block first, then permanent.
    pub(super) fn get(&self, k: &K) -> Option<&V> {
        for layer in self.pending.iter().rev() {
            if let Some(v) = layer.data.get(k) {
                return Some(v);
            }
        }
        self.permanent.get(k)
    }

    /// Retrieves all values for a key from all layers, starting from the latest pending layer.
    ///
    /// # Arguments
    /// * `k` - The key to get the values for.
    ///
    /// # Returns
    /// * `Some(Iterator<Item = &V>)` - An iterator of all values for the key, starting from the
    ///   latest pending layer.
    /// * `None` - If the key is not found in any layer.
    pub(super) fn get_all<'a>(&'a self, k: K) -> Option<impl Iterator<Item = &'a V> + 'a> {
        let key_for_pending = k.clone();
        let key_for_permanent = k;

        let pending_iter = self
            .pending
            .iter()
            .rev()
            .filter_map(move |layer| layer.data.get(&key_for_pending));

        let permanent_iter = self
            .permanent
            .get(&key_for_permanent)
            .into_iter();

        let iter = pending_iter.chain(permanent_iter);

        // Check if iterator would yield any items by peeking at the first element
        // This avoids the redundant contains_key() check that iterates through all layers again
        // while still returning None for empty iterators
        let mut iter = iter.peekable();
        if iter.peek().is_some() {
            Some(iter)
        } else {
            None
        }
    }

    /// Checks if the given key exists in either the pending or permanent layer.
    pub(super) fn contains_key(&self, key: &K) -> bool {
        for layer in self.pending.iter().rev() {
            if layer.data.contains_key(key) {
                return true;
            }
        }
        self.permanent.contains_key(key)
    }

    /// Returns the number of pending block layers.
    pub(super) fn pending_layer_count(&self) -> usize {
        self.pending.len()
    }

    /// Returns an iterator of all unique keys with their latest value.
    /// Pending layers take precedence over the permanent layer.
    pub(super) fn iter_latest(&self) -> impl Iterator<Item = (&K, &V)> {
        let mut result: HashMap<&K, &V> = self.permanent.iter().collect();
        for layer in &self.pending {
            for (k, v) in &layer.data {
                result.insert(k, v);
            }
        }
        result.into_iter()
    }

    /// Returns the count of all unique keys across permanent and pending layers.
    pub(super) fn unique_key_count(&self) -> usize {
        let mut unique_keys = HashSet::new();
        // Collect keys from permanent layer
        unique_keys.extend(self.permanent.keys());
        // Collect keys from all pending layers
        for layer in self.pending.iter() {
            unique_keys.extend(layer.data.keys());
        }
        unique_keys.len()
    }

    /// Process block finality by moving all finalized layers from pending to permanent storage.
    ///
    /// # Arguments
    /// * `finalized_block_height` - The block number of the finalized block
    /// * `strategy` - Strategy for resolving conflicts when the same key exists in multiple layers.
    ///   `Replace` will use later values to replace earlier ones. `Custom(fn)` will use the
    ///   provided function to merge values.
    ///
    /// # Returns
    /// * `Ok(())` - On successful processing
    /// * `Err(DCICacheError::FinalityNotFound)` - If the finalized block is not found in pending
    ///   layers
    ///
    /// Note: to make sure the finalized height is always found, we keep the finalized block in the
    /// pending layers. For example:
    /// If pending layers contain blocks [100, 101, 102, 103] and block 102 is finalized:
    /// - Blocks 100 and 101 are moved to permanent storage
    /// - Block 102 and 103 remain in pending storage
    pub(super) fn handle_finality(
        &mut self,
        finalized_block_height: u64,
        strategy: MergeStrategy<V>,
    ) -> Result<(), DCICacheError> {
        if self.pending.is_empty() {
            return Ok(());
        }

        trace!(
            "Handling finality for block {}, pending layers: {:?}",
            finalized_block_height,
            self.pending
                .iter()
                .map(|layer| layer.block.number)
                .collect::<Vec<_>>()
        );

        let finalized_index = self
            .pending
            .iter()
            .position(|layer| layer.block.number == finalized_block_height)
            .ok_or_else(|| {
                DCICacheError::BlockNotFound(
                    finalized_block_height.to_string(),
                    "finality".to_string(),
                )
            })?;

        // Move all finalized layers but the last one to permanent storage
        let finalized_layers: Vec<_> = self
            .pending
            .drain(..finalized_index)
            .collect();

        for layer in finalized_layers {
            match &strategy {
                MergeStrategy::Replace => {
                    self.permanent.extend(layer.data);
                }
                MergeStrategy::Custom(merge_func) => {
                    for (key, value) in layer.data {
                        match self.permanent.entry(key) {
                            Entry::Occupied(mut entry) => {
                                let merged_value = merge_func(entry.get(), value);
                                entry.insert(merged_value);
                            }
                            Entry::Vacant(entry) => {
                                entry.insert(value);
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Reverts the pending layers to the state of a specific block.
    ///
    /// This will remove all block layers added after the specified block.
    ///
    /// Errors if the block is not found and is not the parent of the latest pending block.
    ///
    /// # Arguments
    /// * `block` - The block to revert to.
    ///
    /// # Returns
    /// * `Ok(())` - On successful reversion
    /// * `Err(DCICacheError::RevertToBlockNotFound)` - If the block is not found in the pending
    ///   layers
    pub(super) fn revert_to(&mut self, block: &BlockHash) -> Result<(), DCICacheError> {
        // If there are no pending layers, do nothing
        if self.pending.is_empty() {
            return Ok(());
        }

        debug!("Purging DCI cache... Target hash {}", block.to_string());

        let mut found = false;
        for (index, layer) in self.pending.iter().enumerate().rev() {
            if layer.block.hash == *block {
                let _ = self.pending.split_off(index + 1); // +1 to keep the target block
                found = true;
                break;
            }
        }

        if !found {
            // On startup, the pending cache could not contain the latest finalized block but only
            // its child. In this case, we clear the pending cache and return.
            if self
                .pending
                .front()
                .unwrap()
                .block
                .parent_hash ==
                *block
            {
                self.pending.clear();
                return Ok(());
            }

            // Otherwise, we're in an erroneous state.
            return Err(DCICacheError::BlockNotFound(block.to_string(), "revert to".to_string()));
        }

        Ok(())
    }

    /// Validates block order and ensures the corresponding block layer exists.
    ///
    /// A valid block must:
    /// - Be the same block as the most recent pending layer;
    /// - Or be the child of the current pending block.
    ///
    /// Fails if blocks arrive out of order.
    ///
    /// # Arguments
    /// * `block` - The block used to determine or create the layer.
    ///
    /// # Returns
    /// * `Ok(())` - On successful validation and layer creation
    /// * `Err(DCICacheError::UnexpectedBlockOrder)` - On invalid chain progression
    pub(super) fn validate_and_ensure_block_layer_internal(
        &mut self,
        block: &Block,
    ) -> Result<(), DCICacheError> {
        match self.pending.back() {
            None => {
                self.pending
                    .push_back(BlockScopedMap { block: block.clone(), data: HashMap::new() });
            }
            Some(last_layer) if last_layer.block == *block => {
                // no-op, already exists
            }
            Some(last_layer) if last_layer.block.hash == block.parent_hash => {
                self.pending
                    .push_back(BlockScopedMap { block: block.clone(), data: HashMap::new() });
            }
            Some(last_layer) => {
                return Err(DCICacheError::UnexpectedBlockOrder(
                    block.number,
                    last_layer.block.number,
                ));
            }
        }

        Ok(())
    }

    /// Gets a mutable reference to the layer for the given block.
    ///
    /// # Arguments
    /// * `block` - The block to get the layer for.
    ///
    /// # Returns
    /// * `Ok(&mut BlockScopedMap<K, V>)` - Layer for the block
    /// * `Err(DCICacheError::BlockNotFound)` - If the block is not found in the pending layers
    fn get_layer_mut(&mut self, block: &Block) -> Result<&mut BlockScopedMap<K, V>, DCICacheError> {
        self.pending
            .iter_mut()
            .find(|layer| layer.block == *block)
            .ok_or_else(|| {
                DCICacheError::BlockNotFound(block.number.to_string(), "get layer".to_string())
            })
    }

    /// Returns full permanent state (only available in tests).
    #[cfg(test)]
    pub(super) fn get_full_permanent_state(&self) -> &HashMap<K, V> {
        &self.permanent
    }

    /// Validates block order and ensures the corresponding block layer exists (only available in
    /// tests).
    #[cfg(test)]
    pub(super) fn validate_and_ensure_block_layer_test(
        &mut self,
        block: &Block,
    ) -> Result<(), DCICacheError> {
        self.validate_and_ensure_block_layer_internal(block)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use chrono::DateTime;
    use tycho_common::{
        models::{
            blockchain::{
                AddressStorageLocation, Block, EntryPoint, EntryPointWithTracingParams,
                RPCTracerParams, TracingParams, TracingResult,
            },
            Address, BlockHash, Chain, StoreKey,
        },
        Bytes,
    };

    use super::*;

    fn create_test_block(number: u64, hash: &str, parent_hash: &str) -> Block {
        Block {
            number,
            hash: BlockHash::from(hash),
            parent_hash: BlockHash::from(parent_hash),
            ts: DateTime::from_timestamp(0, 0)
                .unwrap()
                .naive_utc(),
            chain: Chain::Ethereum,
        }
    }

    fn get_entrypoint(version: u8) -> EntryPoint {
        EntryPoint::new(
            format!("entrypoint_{version}"),
            Bytes::from(version),
            format!("test_entrypoint_{version}"),
        )
    }

    fn get_tracing_params(version: u8) -> TracingParams {
        TracingParams::RPCTracer(RPCTracerParams::new(None, Bytes::from(version)))
    }

    fn get_tracing_result(version: u8) -> TracingResult {
        TracingResult::new(
            HashSet::from([(
                Bytes::from(version),
                AddressStorageLocation::new(Bytes::from(version), 12),
            )]),
            HashMap::from([
                (Bytes::from(version), HashSet::from([Bytes::from(version + version * 16)])),
                (
                    Bytes::from(version + version * 16),
                    HashSet::from([Bytes::from(version + version * 16)]),
                ),
            ]),
        )
    }

    fn get_entrypoint_with_tracing_params(version: u8) -> EntryPointWithTracingParams {
        EntryPointWithTracingParams::new(get_entrypoint(version), get_tracing_params(version))
    }

    #[test]
    fn test_versioned_cache_permanent_operations() {
        let mut cache: VersionedCache<String, u32> = VersionedCache::new();

        // Test permanent operations
        cache.insert_permanent("key1".to_string(), 1);
        assert_eq!(cache.get(&"key1".to_string()), Some(&1));
        assert!(cache.contains_key(&"key1".to_string()));

        // Test permanent entry
        cache
            .permanent_entry(&"key2".to_string())
            .or_insert(2);
        assert_eq!(cache.get(&"key2".to_string()), Some(&2));

        // Test permanent update
        cache.extend_permanent(vec![("key3".to_string(), 3), ("key4".to_string(), 4)]);
        assert_eq!(cache.get(&"key3".to_string()), Some(&3));
        assert_eq!(cache.get(&"key4".to_string()), Some(&4));
    }

    #[test]
    fn test_versioned_cache_pending_operations() {
        let mut cache: VersionedCache<String, u32> = VersionedCache::new();
        let block1 = create_test_block(1, "0x01", "0x00");
        let block2 = create_test_block(2, "0x02", "0x01");
        cache
            .validate_and_ensure_block_layer_test(&block1)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_test(&block2)
            .unwrap();

        // Test pending insert
        cache
            .insert_pending(block1.clone(), "key1".to_string(), 1)
            .unwrap();
        assert_eq!(cache.get(&"key1".to_string()), Some(&1));

        // Test pending entry
        cache
            .pending_entry(&block1, &"key2".to_string())
            .unwrap()
            .or_insert(2);
        assert_eq!(cache.get(&"key2".to_string()), Some(&2));

        // Test pending update
        cache
            .extend_pending(block1.clone(), vec![("key3".to_string(), 3), ("key4".to_string(), 4)])
            .unwrap();
        assert_eq!(cache.get(&"key3".to_string()), Some(&3));
        assert_eq!(cache.get(&"key4".to_string()), Some(&4));

        // Test block ordering
        cache
            .insert_pending(block2.clone(), "key5".to_string(), 5)
            .unwrap();
        assert_eq!(cache.get(&"key5".to_string()), Some(&5));

        // Test invalid block order
        let invalid_block = create_test_block(3, "0x03", "0x00"); // Wrong parent hash
        assert!(cache
            .insert_pending(invalid_block, "key6".to_string(), 6)
            .is_err());
    }

    #[test]
    fn test_versioned_cache_revert() {
        let mut cache: VersionedCache<String, u32> = VersionedCache::new();
        let block1 = create_test_block(1, "0x01", "0x00");
        let block2 = create_test_block(2, "0x02", "0x01");
        let block3 = create_test_block(3, "0x03", "0x02");
        let block4 = create_test_block(4, "0x04", "0x03");
        cache
            .validate_and_ensure_block_layer_test(&block1)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_test(&block2)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_test(&block3)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_test(&block4)
            .unwrap();

        // Insert data in both blocks
        cache.insert_permanent("perm".to_string(), 0);
        cache
            .insert_pending(block1.clone(), "key1".to_string(), 1)
            .unwrap();
        cache
            .insert_pending(block2.clone(), "key2".to_string(), 2)
            .unwrap();

        // Verify initial state
        assert_eq!(cache.get(&"perm".to_string()), Some(&0));
        assert_eq!(cache.get(&"key1".to_string()), Some(&1));
        assert_eq!(cache.get(&"key2".to_string()), Some(&2));

        // Revert to block1
        cache.revert_to(&block1.hash).unwrap();
        assert_eq!(cache.get(&"perm".to_string()), Some(&0));
        assert_eq!(cache.get(&"key1".to_string()), Some(&1));
        assert_eq!(cache.get(&"key2".to_string()), None);

        // Make sure we can insert correct new layers after reverting
        cache
            .validate_and_ensure_block_layer_test(&block2)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_test(&block3)
            .unwrap();
    }

    #[test]
    fn test_dci_cache_revert() {
        let mut cache = DCICache::new();
        let block1 = create_test_block(1, "0x01", "0x00");
        let block2 = create_test_block(2, "0x02", "0x01");
        cache
            .try_insert_block_layer(&block1)
            .unwrap();
        cache
            .try_insert_block_layer(&block2)
            .unwrap();

        // Setup test data
        let entrypoint = get_entrypoint(1);
        let tracing_params = get_tracing_params(1);
        let tracing_result = get_tracing_result(1);
        let address = Address::from("0x1234");
        let store_key = StoreKey::from("0x5678");
        let entrypoint_with_params = get_entrypoint_with_tracing_params(1);
        let mut retrigger_set = HashSet::new();
        retrigger_set.insert(entrypoint_with_params);

        // Insert data in block1
        cache
            .ep_id_to_entrypoint
            .insert_pending(block1.clone(), entrypoint.external_id.clone(), entrypoint.clone())
            .unwrap();
        cache
            .entrypoint_results
            .insert_pending(
                block1.clone(),
                (entrypoint.external_id.clone(), tracing_params.clone()),
                Some(tracing_result.clone()),
            )
            .unwrap();
        cache
            .retriggers
            .insert_pending(
                block1.clone(),
                (address.clone(), store_key.clone()),
                (retrigger_set.clone(), 0),
            )
            .unwrap();

        // Insert different data in block2
        let entrypoint2 = get_entrypoint(2);
        cache
            .ep_id_to_entrypoint
            .insert_pending(block2.clone(), entrypoint2.external_id.clone(), entrypoint2.clone())
            .unwrap();

        // Verify initial state
        assert_eq!(
            cache
                .ep_id_to_entrypoint
                .get(&entrypoint.external_id),
            Some(&entrypoint)
        );
        assert_eq!(
            cache
                .ep_id_to_entrypoint
                .get(&entrypoint2.external_id),
            Some(&entrypoint2)
        );

        // Revert to block1
        cache.revert_to(&block1.hash).unwrap();

        // Verify state after revert
        assert_eq!(
            cache
                .ep_id_to_entrypoint
                .get(&entrypoint.external_id),
            Some(&entrypoint)
        );
        assert_eq!(
            cache
                .ep_id_to_entrypoint
                .get(&entrypoint2.external_id),
            None
        );

        // Revert to the latest finalized block (parent of block1)
        cache
            .revert_to(&block1.parent_hash)
            .unwrap();

        assert_eq!(
            cache
                .ep_id_to_entrypoint
                .get_full_permanent_state()
                .len(),
            0
        );
    }

    #[test]
    fn test_versioned_cache_handle_finality() {
        let mut cache = VersionedCache::new();
        let block1 = create_test_block(1, "0x01", "0x00");
        let block2 = create_test_block(2, "0x02", "0x01");
        let block3 = create_test_block(3, "0x03", "0x02");
        cache
            .validate_and_ensure_block_layer_test(&block1)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_test(&block2)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_test(&block3)
            .unwrap();

        // Insert data in block1
        cache
            .insert_pending(block1.clone(), "key1".to_string(), 1)
            .unwrap();

        // Insert data in block2
        cache
            .insert_pending(block2.clone(), "key2".to_string(), 2)
            .unwrap();

        // Insert data in block3
        cache
            .insert_pending(block3.clone(), "key3".to_string(), 3)
            .unwrap();

        assert_eq!(cache.get_full_permanent_state(), &HashMap::new());

        // Handle finality of block2
        cache
            .handle_finality(block2.number, MergeStrategy::Replace)
            .unwrap();

        assert_eq!(cache.get_full_permanent_state(), &HashMap::from([("key1".to_string(), 1)]));

        // block2 should still be in pending
        assert_eq!(cache.pending.len(), 2);
    }

    #[test]
    fn test_versioned_cache_get_latest_version() {
        let mut cache: VersionedCache<String, u32> = VersionedCache::new();
        let block1 = create_test_block(1, "0x01", "0x00");
        let block2 = create_test_block(2, "0x02", "0x01");
        cache
            .validate_and_ensure_block_layer_test(&block1)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_test(&block2)
            .unwrap();

        // Insert same key with different values in different blocks
        cache.insert_permanent("key".to_string(), 1);
        cache.insert_permanent("key2".to_string(), 2);
        cache
            .insert_pending(block1.clone(), "key".to_string(), 2)
            .unwrap();
        cache
            .insert_pending(block2.clone(), "key".to_string(), 3)
            .unwrap();
        cache
            .insert_pending(block2.clone(), "key2".to_string(), 3)
            .unwrap();

        // get() should return the latest version (from block2)
        assert_eq!(cache.get(&"key".to_string()), Some(&3));

        // Revert to block1
        cache.revert_to(&block1.hash).unwrap();

        // Should return version from block1
        assert_eq!(cache.get(&"key".to_string()), Some(&2));
        // Should return permanent version
        assert_eq!(cache.get(&"key2".to_string()), Some(&2));
    }

    #[test]
    fn test_versioned_cache_pending_entry_latest_version() {
        let mut cache: VersionedCache<String, u32> = VersionedCache::new();
        let block1 = create_test_block(1, "0x01", "0x00");
        let block2 = create_test_block(2, "0x02", "0x01");
        cache
            .validate_and_ensure_block_layer_test(&block1)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_test(&block2)
            .unwrap();

        // Insert same key with different values in different blocks
        cache.insert_permanent("key".to_string(), 1);
        cache
            .insert_pending(block1.clone(), "key".to_string(), 2)
            .unwrap();
        cache
            .insert_pending(block2.clone(), "key".to_string(), 3)
            .unwrap();

        // pending_entry() should return the latest version for block2
        let entry = cache
            .pending_entry(&block2, &"key".to_string())
            .unwrap();
        assert_eq!(entry.or_insert(4), &3);

        // Test that pending_entry() creates a new entry if key doesn't exist in that block
        let entry = cache
            .pending_entry(&block2, &"new_key".to_string())
            .unwrap();
        entry.or_insert(5);
        assert_eq!(cache.get(&"new_key".to_string()), Some(&5));
    }

    #[test]
    fn test_versioned_cache_update_overwrites_latest() {
        let mut cache: VersionedCache<String, u32> = VersionedCache::new();
        let block1 = create_test_block(1, "0x01", "0x00");
        let block2 = create_test_block(2, "0x02", "0x01");
        cache
            .validate_and_ensure_block_layer_test(&block1)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_test(&block2)
            .unwrap();

        // Insert initial values
        cache.insert_permanent("key1".to_string(), 1);
        cache.extend_permanent(vec![("key1".to_string(), 10), ("key2".to_string(), 20)]);

        cache
            .insert_pending(block1.clone(), "key2".to_string(), 2)
            .unwrap();
        cache
            .extend_pending(
                block1.clone(),
                vec![("key2".to_string(), 21), ("key3".to_string(), 31)],
            )
            .unwrap();

        cache
            .insert_pending(block2.clone(), "key3".to_string(), 3)
            .unwrap();
        cache
            .extend_pending(block2.clone(), vec![("key4".to_string(), 40)])
            .unwrap();

        // Verify latest versions
        assert_eq!(cache.get(&"key1".to_string()), Some(&10)); // permanent
        assert_eq!(cache.get(&"key2".to_string()), Some(&21)); // block1
        assert_eq!(cache.get(&"key3".to_string()), Some(&3)); // block2
        assert_eq!(cache.get(&"key4".to_string()), Some(&40)); // block2
    }

    #[test]
    fn test_versioned_cache_get_all() {
        let mut cache: VersionedCache<String, u32> = VersionedCache::new();
        let block1 = create_test_block(1, "0x01", "0x00");
        let block2 = create_test_block(2, "0x02", "0x01");
        let block3 = create_test_block(3, "0x03", "0x02");

        cache
            .validate_and_ensure_block_layer_internal(&block1)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_internal(&block2)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_internal(&block3)
            .unwrap();

        // Test 1: Key not found in any layer
        assert!(cache
            .get_all("nonexistent".to_string())
            .is_none());

        // Test 2: Key only in permanent layer
        cache.insert_permanent("perm_only".to_string(), 100);
        let result = cache
            .get_all("perm_only".to_string())
            .unwrap();
        assert_eq!(result.collect::<Vec<_>>(), vec![&100]);

        // Test 3: Key only in one pending layer
        cache
            .insert_pending(block2.clone(), "pending_only".to_string(), 200)
            .unwrap();
        let result = cache
            .get_all("pending_only".to_string())
            .unwrap();
        assert_eq!(result.collect::<Vec<_>>(), vec![&200]);

        // Test 4: Key in permanent and one pending layer
        cache.insert_permanent("mixed".to_string(), 300);
        cache
            .insert_pending(block1.clone(), "mixed".to_string(), 301)
            .unwrap();
        let result = cache
            .get_all("mixed".to_string())
            .unwrap();
        assert_eq!(result.collect::<Vec<_>>(), vec![&301, &300]); // Latest pending first, then permanent

        // Test 5: Key in multiple pending layers and permanent
        cache.insert_permanent("multi".to_string(), 400);
        cache
            .insert_pending(block1.clone(), "multi".to_string(), 401)
            .unwrap();
        cache
            .insert_pending(block2.clone(), "multi".to_string(), 402)
            .unwrap();
        cache
            .insert_pending(block3.clone(), "multi".to_string(), 403)
            .unwrap();
        let result = cache
            .get_all("multi".to_string())
            .unwrap();
        assert_eq!(result.collect::<Vec<_>>(), vec![&403, &402, &401, &400]); // Latest to oldest

        // Test 6: Key in some but not all pending layers
        cache.insert_permanent("sparse".to_string(), 500);
        cache
            .insert_pending(block1.clone(), "sparse".to_string(), 501)
            .unwrap();
        cache
            .insert_pending(block3.clone(), "sparse".to_string(), 503)
            .unwrap();
        let result = cache
            .get_all("sparse".to_string())
            .unwrap();
        assert_eq!(result.collect::<Vec<_>>(), vec![&503, &501, &500]); // Skips block2, includes others

        // Test 7: Key only in multiple pending layers (no permanent)
        cache
            .insert_pending(block1.clone(), "pending_multi".to_string(), 601)
            .unwrap();
        cache
            .insert_pending(block3.clone(), "pending_multi".to_string(), 603)
            .unwrap();
        let result = cache
            .get_all("pending_multi".to_string())
            .unwrap();
        assert_eq!(result.collect::<Vec<_>>(), vec![&603, &601]); // Latest to oldest pending only

        // Test 8: After revert, get_all should reflect the new state
        cache.revert_to(&block2.hash).unwrap();
        let result = cache
            .get_all("multi".to_string())
            .unwrap();
        assert_eq!(result.collect::<Vec<_>>(), vec![&402, &401, &400]); // Block3 data removed after revert

        // Test 9: After finality, get_all should include finalized data in permanent
        cache
            .handle_finality(block2.number, MergeStrategy::Replace)
            .unwrap();
        let result = cache
            .get_all("multi".to_string())
            .unwrap();
        assert_eq!(result.collect::<Vec<_>>(), vec![&402, &401]); // Block1 data moved to permanent,
                                                                  // block2 stays
                                                                  // pending
    }

    #[test]
    fn test_versioned_cache_handle_finality_with_merge() {
        let mut cache: VersionedCache<Address, Option<HashSet<StoreKey>>> = VersionedCache::new();
        let block1 = create_test_block(1, "0x01", "0x00");
        let block2 = create_test_block(2, "0x02", "0x01");
        let block3 = create_test_block(3, "0x03", "0x02");

        cache
            .validate_and_ensure_block_layer_test(&block1)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_test(&block2)
            .unwrap();
        cache
            .validate_and_ensure_block_layer_test(&block3)
            .unwrap();

        let address1 = Address::from("0x1111");
        let address2 = Address::from("0x2222");
        let key1 = StoreKey::from("0x01");
        let key2 = StoreKey::from("0x02");
        let key3 = StoreKey::from("0x03");

        // Insert initial data in permanent storage
        cache.insert_permanent(address1.clone(), Some(HashSet::from([key1.clone()])));

        // Insert data in block1 - same address, different keys
        cache
            .insert_pending(block1.clone(), address1.clone(), Some(HashSet::from([key2.clone()])))
            .unwrap();

        // Insert data in block2 - same address, more keys
        cache
            .insert_pending(block2.clone(), address1.clone(), Some(HashSet::from([key3.clone()])))
            .unwrap();

        // Insert different address in block1
        cache
            .insert_pending(block1.clone(), address2.clone(), Some(HashSet::from([key1.clone()])))
            .unwrap();

        // Handle finality with merge function
        cache
            .handle_finality(
                block2.number,
                MergeStrategy::Custom(Box::new(
                    |existing: &Option<HashSet<StoreKey>>, new: Option<HashSet<StoreKey>>| {
                        match (existing, new) {
                            (None, _) | (_, None) => None, /* If either is None (full tracking), */
                            // result is None
                            (Some(existing_set), Some(new_set)) => {
                                let mut merged = existing_set.clone();
                                merged.extend(new_set);
                                Some(merged)
                            }
                        }
                    },
                )),
            )
            .unwrap();

        // Verify merged results
        let permanent_state = cache.get_full_permanent_state();

        // address1 should have keys merged from permanent layer and block1 only
        // (block2 remains in pending, so key3 is not included in permanent yet)
        let expected_keys = HashSet::from([key1.clone(), key2.clone()]);
        assert_eq!(permanent_state.get(&address1), Some(&Some(expected_keys)));

        // address2 should have key1 from block1
        assert_eq!(permanent_state.get(&address2), Some(&Some(HashSet::from([key1.clone()]))));
    }
}
