use core::fmt::Debug;
use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;

use crate::{
    models::{
        blockchain::{
            Block, BlockAggregatedChanges, BlockTag, EntryPointWithTracingParams, TracedEntryPoint,
            TxInput,
        },
        contract::AccountDelta,
        token::{Token, TokenQuality, TransferCost, TransferTax},
        Address, Balance, BlockHash, StoreKey,
    },
    Bytes,
};

/// Indexes protocol state deltas from raw EVM transactions.
///
/// A `TxDeltaIndexer` is a native-code substitute for a Substreams package.
/// It consumes a stream of finalised [`BlockAggregatedChanges`] to keep its internal
/// protocol state current, then — on demand — applies a batch of in-flight
/// [`TxInput`]s and returns the resulting [`BlockAggregatedChanges`]. The primary
/// consumer is an Ethereum block builder that needs to know how a candidate
/// transaction bundle alters DEX state before deciding whether to include it.
///
/// # Lifecycle
///
/// 1. **Hydrate** — call [`apply_block`][TxDeltaIndexer::apply_block] for each finalised block
///    received from the Tycho client. The first call serves as initialisation: `state_deltas` and
///    `component_balances` will contain the full snapshot at that height rather than a sparse
///    delta. Subsequent calls apply incremental deltas.
///
/// 2. **Query** — call [`generate_deltas`][TxDeltaIndexer::generate_deltas] with a batch of
///    in-flight transactions at any point. The indexer applies them against its current internal
///    state and returns a [`BlockAggregatedChanges`] describing what would change. Internal state
///    is **not** mutated by this call; it always operates on the state left by the most recent
///    `apply_block`.
pub trait TxDeltaIndexer: Send {
    /// Advances internal protocol state by applying a finalised block.
    ///
    /// Must be called in block-height order. The first call initialises the
    /// indexer from a full snapshot; subsequent calls apply incremental state
    /// deltas. After this call returns, [`generate_deltas`][TxDeltaIndexer::generate_deltas]
    /// will produce deltas relative to the new state.
    ///
    /// # Parameters
    ///
    /// * `block` — a finalised [`BlockAggregatedChanges`] as received from the Tycho client. On the
    ///   first call this carries the full component and state snapshot; on later calls it carries
    ///   only the changed attributes and balances.
    fn apply_block(&mut self, block: &BlockAggregatedChanges) -> anyhow::Result<()>;

    /// Applies a batch of in-flight transactions against the current state and
    /// returns the protocol state deltas they would produce.
    ///
    /// The returned [`BlockAggregatedChanges`] contains the aggregated deltas across
    /// all transactions in the batch: `state_deltas`, `component_balances`,
    /// `new_protocol_components`, and `deleted_protocol_components`. Block
    /// metadata (`block`, `chain`, `extractor`, `finalized_block_height`) is
    /// populated from the state stored by the most recent
    /// [`apply_block`][TxDeltaIndexer::apply_block] call.
    ///
    /// Internal state is **not** modified. Calling `generate_deltas` twice with
    /// the same transactions returns identical results.
    ///
    /// Transactions where `succeeded == false` are silently skipped.
    ///
    /// # Parameters
    ///
    /// * `txs` — ordered slice of in-flight transactions, typically a builder's candidate bundle or
    ///   the full mempool selection for one block.
    fn generate_deltas(&mut self, txs: &[TxInput]) -> BlockAggregatedChanges;
}

/// A struct representing a request to get an account state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StorageSnapshotRequest {
    // The address of the account to get the state of.
    pub address: Address,
    // The specific slots to get the state of. If `None`, the entire account state will be
    // returned.
    pub slots: Option<Vec<StoreKey>>,
}

impl std::fmt::Display for StorageSnapshotRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let address_str = self.address.to_string();
        let truncated_address = if address_str.len() >= 10 {
            format!("{}...{}", &address_str[0..8], &address_str[address_str.len() - 4..])
        } else {
            address_str
        };

        match &self.slots {
            Some(slots) => write!(f, "{truncated_address}[{} slots]", slots.len()),
            None => write!(f, "{truncated_address}[all slots]"),
        }
    }
}

/// Trait for getting multiple account states from chain data.
#[cfg_attr(feature = "test-utils", mockall::automock(type Error = String;))]
#[async_trait]
pub trait AccountExtractor {
    type Error: Debug + Send + Sync;

    /// Get the account states at the end of the given block (after all transactions in the block
    /// have been applied).
    ///
    /// # Arguments
    ///
    /// * `block`: The block at which to retrieve the account states.
    /// * `requests`: A slice of `StorageSnapshotRequest` objects, each containing an address and
    ///   optional slots.
    /// Note: If the `slots` field is `None`, the function will return the entire account state.
    /// That could be a lot of data, so use with caution.
    ///
    /// returns: Result<HashMap<Bytes, AccountDelta, RandomState>, Self::Error>
    /// A result containing a HashMap where the keys are `Bytes` (addresses) and the values are
    /// `AccountDelta` objects.
    async fn get_accounts_at_block(
        &self,
        block: &Block,
        requests: &[StorageSnapshotRequest],
    ) -> Result<HashMap<Bytes, AccountDelta>, Self::Error>; //TODO: do not return `AccountUpdate` but `Account`
}

/// Trait for analyzing a token, including its quality, transfer cost, and transfer tax.
#[async_trait]
pub trait TokenAnalyzer: Send + Sync {
    type Error;

    /// Analyzes the quality of a token given its address and a block tag.
    ///
    /// # Parameters
    /// * `token` - The address of the token to analyze.
    /// * `block` - The block tag at which the analysis should be performed.
    ///
    /// # Returns
    /// A result containing:
    /// * `TokenQuality` - The quality assessment of the token (either `Good` or `Bad`).
    /// * `Option<TransferCost>` - The average cost per transfer, if available.
    /// * `Option<TransferTax>` - The transfer tax, if applicable.
    ///
    /// On failure, returns `Self::Error`.
    async fn analyze(
        &self,
        token: Bytes,
        block: BlockTag,
    ) -> Result<(TokenQuality, Option<TransferCost>, Option<TransferTax>), Self::Error>;
}

/// Trait for finding an address that owns a specific token. This is useful for detecting
/// bad tokens by identifying addresses with enough balance to simulate transactions.
#[async_trait]
pub trait TokenOwnerFinding: Send + Sync + Debug {
    /// Finds an address that holds at least `min_balance` of the specified token.
    ///
    /// # Parameters
    /// * `token` - The address of the token to search for.
    /// * `min_balance` - The minimum balance required for the address to be considered.
    ///
    /// # Returns
    /// A result containing:
    /// * `Option<(Address, Balance)>` - The address and its actual balance if an owner is found.
    /// If no address meets the criteria, returns `None`.
    /// On failure, returns a string representing an error message.
    async fn find_owner(
        &self,
        token: Address,
        min_balance: Balance,
    ) -> Result<Option<(Address, Balance)>, String>; // TODO: introduce custom error type
}

/// Trait for retrieving additional information about tokens, such as the number of decimals
/// and the token symbol, to help construct `CurrencyToken` objects.
#[async_trait]
pub trait TokenPreProcessor: Send + Sync {
    /// Given a list of token addresses, this function retrieves additional metadata for each token.
    ///
    /// # Parameters
    /// * `addresses` - A vector of token addresses to process.
    /// * `token_finder` - A reference to a `TokenOwnerFinding` implementation to help find token
    ///   owners.
    /// * `block` - The block tag at which the information should be retrieved.
    ///
    /// # Returns
    /// A vector of `CurrencyToken` objects, each containing the processed information for the
    /// token.
    async fn get_tokens(
        &self,
        addresses: Vec<Bytes>,
        token_finder: Arc<dyn TokenOwnerFinding>,
        block: BlockTag,
    ) -> Vec<Token>;
}

/// Trait for tracing blockchain transaction execution.
#[cfg_attr(feature = "test-utils", mockall::automock(type Error = String;))]
#[async_trait]
pub trait EntryPointTracer: Sync {
    type Error: Debug;

    /// Traces the execution of a list of entry points at a specific block.
    ///
    /// # Parameters
    /// * `block_hash` - The hash of the block at which to perform the trace. The trace will use the
    ///   state of the blockchain at this block.
    /// * `entry_points` - A list of entry points to trace with their data.
    ///
    /// # Returns
    /// Returns a vector of `TracedEntryPoint`, where each element contains:
    /// * `retriggers` - A set of (address, storage slot) pairs representing storage locations that
    ///   could alter tracing results. If any of these storage slots change, the set of called
    ///   contract might be outdated.
    /// * `accessed_slots` - A map of all contract addresses that were called during the trace with
    ///   a list of storage slots that were accessed (read or written).
    async fn trace(
        &self,
        block_hash: BlockHash,
        entry_points: Vec<EntryPointWithTracingParams>,
    ) -> Vec<Result<TracedEntryPoint, Self::Error>>;
}

/// Trait for detecting storage slots that contain ERC20 token balances
#[cfg_attr(feature = "test-utils", mockall::automock(type Error = String;))]
#[async_trait]
pub trait BalanceSlotDetector: Send + Sync {
    type Error: Debug;

    /// Detect balance storage slots for multiple tokens from a single holder.
    /// Useful to allow overriding balances.
    ///
    /// # Arguments
    /// * `tokens` - Slice of ERC20 token addresses.
    /// * `holder` - Address that holds the tokens (e.g., pool manager)
    /// * `block_hash` - Block at which to detect slots
    ///
    /// # Returns
    /// HashMap mapping Token -> Result containing (contract_address -> storage_slot) or error.
    /// The storage slot is the one that controls the token's holder balance.
    async fn detect_balance_slots(
        &self,
        tokens: &[Address],
        holder: Address,
        block_hash: BlockHash,
    ) -> HashMap<Address, Result<(Address, Bytes), Self::Error>>;
}

/// Trait for detecting storage slots that contain ERC20 token allowances
#[cfg_attr(feature = "test-utils", mockall::automock(type Error = String;))]
#[async_trait]
pub trait AllowanceSlotDetector: Send + Sync {
    type Error: Debug;

    /// Detect allowance storage slots for multiple tokens for owner-spender pairs.
    /// Useful to allow overriding allowances in simulations.
    ///
    /// # Arguments
    /// * `tokens` - Slice of ERC20 token addresses.
    /// * `owner` - Address that owns the tokens
    /// * `spender` - Address that is allowed to spend the tokens
    /// * `block_hash` - Block at which to detect slots
    ///
    /// # Returns
    /// HashMap mapping Token -> Result containing (contract_address -> storage_slot) or error.
    /// The storage slot is the one that controls the allowance from owner to spender.
    async fn detect_allowance_slots(
        &self,
        tokens: &[Address],
        owner: Address,
        spender: Address,
        block_hash: BlockHash,
    ) -> HashMap<Address, Result<(Address, Bytes), Self::Error>>;
}

/// Trait for getting the transaction fee price from the node.
/// The fee price is the dynamic part of the fee, which usually varies based on the network
/// congestion. For example, on Ethereum, the fee price is the gas price (base fee + priority fee).
#[cfg_attr(feature = "test-utils", mockall::automock(type Error = String; type FeePrice = u128;))]
#[async_trait]
pub trait FeePriceGetter: Send + Sync {
    type Error: Debug;
    type FeePrice;

    /// Get the latest fee price information from the chain.
    ///
    /// # Returns
    /// A chain-specific fee price type that can provide an effective fee price.
    async fn get_latest_fee_price(&self) -> Result<Self::FeePrice, Self::Error>;
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_storage_snapshot_request_display() {
        // Test with specific slots
        let request_with_slots = StorageSnapshotRequest {
            address: Address::from_str("0x1234567890123456789012345678901234567890").unwrap(),
            slots: Some(vec![
                StoreKey::from(vec![1, 2, 3, 4]),
                StoreKey::from(vec![5, 6, 7, 8]),
                StoreKey::from(vec![9, 10, 11, 12]),
            ]),
        };

        let display_output = request_with_slots.to_string();
        assert_eq!(display_output, "0x123456...7890[3 slots]");

        // Test with all slots
        let request_all_slots = StorageSnapshotRequest {
            address: Address::from_str("0x9876543210987654321098765432109876543210").unwrap(),
            slots: None,
        };

        let display_output = request_all_slots.to_string();
        assert_eq!(display_output, "0x987654...3210[all slots]");

        // Test with empty slots vector
        let request_empty_slots = StorageSnapshotRequest {
            address: Address::from_str("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap(),
            slots: Some(vec![]),
        };

        let display_output = request_empty_slots.to_string();
        assert_eq!(display_output, "0xabcdef...abcd[0 slots]");
    }
}
