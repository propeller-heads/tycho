//! Builder for configuring a multi-protocol stream.
//!
//! Provides a builder for creating a multi-protocol stream that produces
//! protocol state update messages. It runs one synchronization worker per protocol
//! and a supervisor that aggregates updates, ensuring gap‑free streaming
//! and robust state tracking.
//!
//! ## Context
//!
//! This stream wraps a `TychoStream` from `tycho-client`. It decodes `FeedMessage`s
//! into protocol state updates. Internally, each protocol runs in its own
//! synchronization worker, and a supervisor aggregates their messages per block.
//!
//! ### Protocol Synchronization Worker
//! A synchronization worker runs the snapshot + delta protocol from `tycho-indexer`.
//! - It first downloads components and their snapshots.
//! - It then streams deltas.
//! - It reacts to new or paused components by pulling snapshots or removing them from the active
//!   set.
//!
//! Each worker emits snapshots and deltas to the supervisor.
//!
//! ### Stream Supervisor
//! The supervisor aggregates worker messages by block and assigns sync status.
//! - It ensures workers produce gap-free messages.
//! - It flags late workers as `Delayed`, and marks them `Stale` if they exceed `max_missed_blocks`.
//! - It marks workers with terminal errors as `Ended`.
//!
//! Aggregating by block adds small latency, since the supervisor waits briefly for
//! all workers to emit. This latency only applies to workers in `Ready` or `Delayed`.
//!
//! The stream ends only when **all** workers are `Stale` or `Ended`.
//!
//! ## Configuration
//!
//! The builder lets you customize:
//!
//! ### Protocols
//! Select which protocols to synchronize.
//!
//! ### Tokens & Minimum Token Quality
//! Provide token metadata up front so the decoder can initialize protocol states from startup
//! snapshots. `set_tokens` does not act as an ongoing filter — components arriving after startup
//! include their own token metadata. To restrict processing to specific tokens, apply that filter
//! in your consumer when reading `new_components`. New tokens arriving via stream deltas are added
//! automatically when their quality exceeds `min_token_quality`.
//!
//! ### StreamEndPolicy
//! Control when the stream ends based on worker states. By default, it ends when all
//! workers are `Stale` or `Ended`.
//!
//! ## Stream
//! The stream emits one protocol state update every `block_time`. Each update
//! reports protocol synchronization states and any changes.
//!
//! The `new_components` field lists newly deployed components and their tokens.
//!
//! The stream aims to run indefinitely. Internal retry and reconnect logic handle
//! most errors, so users should rarely need to restart it manually.
//!
//! ## Example
//! ```no_run
//! use tycho_common::models::Chain;
//! use tycho_simulation::evm::stream::ProtocolStreamBuilder;
//! use tycho_simulation::utils::load_all_tokens;
//! use futures::StreamExt;
//! use tycho_client::feed::component_tracker::ComponentFilter;
//! use tycho_simulation::evm::protocol::uniswap_v2::state::UniswapV2State;
//! use std::collections::HashSet;
//!
//! #[tokio::main]
//! async fn main() {
//!     let all_tokens = load_all_tokens(
//!         "tycho-beta.propellerheads.xyz",
//!         false,
//!         Some("sampletoken"),
//!         true,
//!         Chain::Ethereum,
//!         None,
//!         None,
//!     )
//!     .await
//!     .expect("Failed loading tokens");
//!
//!     let protocol_stream =
//!         ProtocolStreamBuilder::new("tycho-beta.propellerheads.xyz", Chain::Ethereum)
//!             .auth_key(Some("sampletoken".to_string()))
//!             .skip_state_decode_failures(true)
//!             .exchange::<UniswapV2State>(
//!                 "uniswap_v2", ComponentFilter::with_tvl_range(5.0, 10.0), None
//!             )
//!             .blocklist_components(HashSet::new())
//!             .set_tokens(all_tokens)
//!             .await
//!             .build()
//!             .await
//!             .expect("Failed building protocol stream");
//!     tokio::pin!(protocol_stream);
//!
//!     // Loop through block updates
//!     while let Some(msg) = protocol_stream.next().await {
//!         dbg!(msg).expect("failed decoding");
//!     }
//! }
//! ```
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time,
};

use futures::{future::Either, stream, Stream, StreamExt};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, warn};
use tycho_client::{
    feed::{
        component_tracker::ComponentFilter, synchronizer::ComponentWithState, BlockHeader,
        FeedMessage, SynchronizerState,
    },
    stream::{RetryConfiguration, StreamError, TychoStreamBuilder},
};
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    traits::TxDeltaIndexer,
    Bytes,
};

use crate::{
    evm::{
        decoder::{StreamDecodeError, TychoStreamDecoder},
        pending::PendingBlockProcessor,
        protocol::{
            native_wrapper::state::{NativeWrapperState, NATIVE_WRAPPER_ID},
            uniswap_v4::hooks::hook_handler_creator::initialize_hook_handlers,
        },
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock, Update},
    },
};

const EXCHANGES_REQUIRING_FILTER: [&str; 2] = ["vm:balancer_v2", "vm:curve"];

#[derive(Default, Debug, Clone, Copy)]
pub enum StreamEndPolicy {
    /// End stream if all states are Stale or Ended (default)
    #[default]
    AllEndedOrStale,
    /// End stream if any protocol ended
    AnyEnded,
    /// End stream if any protocol ended or is stale
    AnyEndedOrStale,
    /// End stream if any protocol is stale
    AnyStale,
}

impl StreamEndPolicy {
    fn should_end<'a>(&self, states: impl IntoIterator<Item = &'a SynchronizerState>) -> bool {
        let mut it = states.into_iter();
        match self {
            StreamEndPolicy::AllEndedOrStale => false,
            StreamEndPolicy::AnyEnded => it.any(|s| matches!(s, SynchronizerState::Ended(_))),
            StreamEndPolicy::AnyStale => it.any(|s| matches!(s, SynchronizerState::Stale(_))),
            StreamEndPolicy::AnyEndedOrStale => {
                it.any(|s| matches!(s, SynchronizerState::Stale(_) | SynchronizerState::Ended(_)))
            }
        }
    }
}

/// Builds and configures the multi protocol stream described in the [module-level docs](self).
///
/// See the module documentation for details on protocols, configuration options, and
/// stream behavior.
pub struct ProtocolStreamBuilder {
    decoder: TychoStreamDecoder<BlockHeader>,
    stream_builder: TychoStreamBuilder,
    stream_end_policy: StreamEndPolicy,
    chain: Chain,
    pending_indexers: HashMap<String, Box<dyn TxDeltaIndexer>>,
}

impl ProtocolStreamBuilder {
    /// Creates a new builder for a multi-protocol stream.
    ///
    /// See the [module-level docs](self) for full details on stream behavior and configuration.
    pub fn new(tycho_url: &str, chain: Chain) -> Self {
        Self {
            decoder: TychoStreamDecoder::new(),
            stream_builder: TychoStreamBuilder::new(tycho_url, chain),
            stream_end_policy: StreamEndPolicy::default(),
            chain,
            pending_indexers: HashMap::new(),
        }
    }

    /// Adds a specific exchange to the stream.
    ///
    /// This configures the builder to include a new protocol synchronizer for `name`,
    /// filtering its components according to `filter` and optionally `filter_fn`.
    ///
    /// The type parameter `T` specifies the decoder type for this exchange. All
    /// component states for this exchange will be decoded into instances of `T`.
    ///
    /// # Parameters
    ///
    /// - `name`: The protocol or exchange name (e.g., `"uniswap_v4"`, `"vm:balancer_v2"`).
    /// - `filter`: Defines the set of components to include in the stream.
    /// - `filter_fn`: Optional custom filter function for client-side filtering of components not
    ///   expressible in `filter`.
    ///
    /// # Notes
    ///
    /// For certain protocols (e.g., `"uniswap_v4"`, `"vm:balancer_v2"`, `"vm:curve"`), omitting
    /// `filter_fn` may cause decoding errors or incorrect results. In these cases, a proper
    /// filter function is required to ensure correct decoding and quoting logic.
    pub fn exchange<T>(
        mut self,
        name: &str,
        filter: ComponentFilter,
        filter_fn: Option<fn(&ComponentWithState) -> bool>,
    ) -> Self
    where
        T: ProtocolSim
            + TryFromWithBlock<ComponentWithState, BlockHeader, Error = InvalidSnapshotError>
            + Send
            + 'static,
    {
        self.stream_builder = self
            .stream_builder
            .exchange(name, filter);
        self.decoder.register_decoder::<T>(name);
        if let Some(predicate) = filter_fn {
            self.decoder
                .register_filter(name, predicate);
        }

        if EXCHANGES_REQUIRING_FILTER.contains(&name) && filter_fn.is_none() {
            warn!(
                "Warning: For exchange type '{}', it is necessary to set a filter function because not all pools are supported. See all filters at src/evm/protocol/filters.rs",
                name
            );
        }

        self
    }

    /// Adds a specific exchange to the stream with decoder context.
    ///
    /// This configures the builder to include a new protocol synchronizer for `name`,
    /// filtering its components according to `filter` and optionally `filter_fn`. It also registers
    /// the DecoderContext (this is useful to test protocols that are not live yet)
    ///
    /// The type parameter `T` specifies the decoder type for this exchange. All
    /// component states for this exchange will be decoded into instances of `T`.
    ///
    /// # Parameters
    ///
    /// - `name`: The protocol or exchange name (e.g., `"uniswap_v4"`, `"vm:balancer_v2"`).
    /// - `filter`: Defines the set of components to include in the stream.
    /// - `filter_fn`: Optional custom filter function for client-side filtering of components not
    ///   expressible in `filter`.
    /// - `decoder_context`: The decoder context for this exchange
    ///
    /// # Notes
    ///
    /// For certain protocols (e.g., `"uniswap_v4"`, `"vm:balancer_v2"`, `"vm:curve"`), omitting
    /// `filter_fn` may cause decoding errors or incorrect results. In these cases, a proper
    /// filter function is required to ensure correct decoding and quoting logic.
    pub fn exchange_with_decoder_context<T>(
        mut self,
        name: &str,
        filter: ComponentFilter,
        filter_fn: Option<fn(&ComponentWithState) -> bool>,
        decoder_context: DecoderContext,
    ) -> Self
    where
        T: ProtocolSim
            + TryFromWithBlock<ComponentWithState, BlockHeader, Error = InvalidSnapshotError>
            + Send
            + 'static,
    {
        self.stream_builder = self
            .stream_builder
            .exchange(name, filter);
        self.decoder
            .register_decoder_with_context::<T>(name, decoder_context);
        if let Some(predicate) = filter_fn {
            self.decoder
                .register_filter(name, predicate);
        }

        if EXCHANGES_REQUIRING_FILTER.contains(&name) && filter_fn.is_none() {
            warn!(
                "Warning: For exchange type '{}', it is necessary to set a filter function because not all pools are supported. See all filters at src/evm/protocol/filters.rs",
                name
            );
        }

        self
    }

    /// Sets the block time interval for the stream.
    ///
    /// This controls how often the stream produces updates.
    pub fn block_time(mut self, block_time: u64) -> Self {
        self.stream_builder = self
            .stream_builder
            .block_time(block_time);
        self
    }

    /// Sets the network operation timeout (deprecated).
    ///
    /// Use [`latency_buffer()`](Self::latency_buffer) instead for controlling latency.
    /// This method is retained for backwards compatibility.
    #[deprecated = "Use latency_buffer instead"]
    pub fn timeout(mut self, timeout: u64) -> Self {
        self.stream_builder = self.stream_builder.timeout(timeout);
        self
    }

    /// Sets the latency buffer to aggregate same-block messages.
    ///
    /// This allows the supervisor to wait a short interval for all synchronizers to emit
    /// before aggregating.
    pub fn latency_buffer(mut self, timeout: u64) -> Self {
        self.stream_builder = self.stream_builder.timeout(timeout);
        self
    }

    /// Sets the maximum number of blocks a synchronizer may miss before being marked as `Stale`.
    pub fn max_missed_blocks(mut self, n: u64) -> Self {
        self.stream_builder = self.stream_builder.max_missed_blocks(n);
        self
    }

    /// Sets how long a synchronizer may take to process the initial message.
    ///
    /// Useful for data-intensive protocols where startup decoding takes longer.
    pub fn startup_timeout(mut self, timeout: time::Duration) -> Self {
        self.stream_builder = self
            .stream_builder
            .startup_timeout(timeout);
        self
    }

    /// Configures the stream to exclude state updates.
    ///
    /// This reduces bandwidth and decoding workload if protocol state is not of
    /// interest (e.g. only process new tokens).
    pub fn no_state(mut self, no_state: bool) -> Self {
        self.stream_builder = self.stream_builder.no_state(no_state);
        self
    }

    /// Sets the API key for authenticating with the Tycho server.
    pub fn auth_key(mut self, auth_key: Option<String>) -> Self {
        self.stream_builder = self.stream_builder.auth_key(auth_key);
        self
    }

    /// Disables TLS/ SSL for the connection, using http and ws protocols.
    ///
    /// This is not recommended for production use.
    pub fn no_tls(mut self, no_tls: bool) -> Self {
        self.stream_builder = self.stream_builder.no_tls(no_tls);
        self
    }

    /// Disable compression for the connection.
    pub fn disable_compression(mut self) -> Self {
        self.stream_builder = self
            .stream_builder
            .disable_compression();
        self
    }

    /// Enables partial block updates (flashblocks).
    pub fn enable_partial_blocks(mut self) -> Self {
        self.stream_builder = self
            .stream_builder
            .enable_partial_blocks();
        self
    }

    /// Exclude specific component IDs from all registered exchanges.
    pub fn blocklist_components(mut self, ids: HashSet<String>) -> Self {
        if !ids.is_empty() {
            tracing::info!("Blocklisting {} components", ids.len());
            self.stream_builder = self.stream_builder.blocklisted_ids(ids);
        }
        self
    }

    /// Sets the stream end policy.
    ///
    /// Controls when the stream should stop based on synchronizer states.
    ///
    /// ## Note
    /// The stream always ends latest if all protocols are stale or ended independent of
    /// this configuration. This allows you to end the stream earlier than that.
    ///
    /// See [self::StreamEndPolicy] for possible configuration options.
    pub fn stream_end_policy(mut self, stream_end_policy: StreamEndPolicy) -> Self {
        self.stream_end_policy = stream_end_policy;
        self
    }

    /// Provides token metadata used to decode startup snapshots and initialize protocol states.
    ///
    /// This is not a stream filter — components arriving after startup include their own token
    /// metadata. To restrict to specific tokens, filter in your consumer logic. New tokens
    /// arriving via stream deltas are added automatically if they meet the quality threshold.
    pub async fn set_tokens(self, tokens: HashMap<Bytes, Token>) -> Self {
        self.decoder.set_tokens(tokens).await;
        self
    }

    /// Skips decoding errors for component state updates.
    ///
    /// Allows the stream to continue processing even if some states fail to decode,
    /// logging a warning instead of panicking.
    pub fn skip_state_decode_failures(mut self, skip: bool) -> Self {
        self.decoder
            .skip_state_decode_failures(skip);
        self
    }

    /// Sets the minimum token quality for tokens added via the stream.
    ///
    /// Tokens arriving in stream deltas below this threshold are ignored. Defaults to 100.
    /// Set this to the same value used in [`load_all_tokens()`](crate::utils::load_all_tokens) to
    /// apply consistent filtering.
    pub fn min_token_quality(mut self, quality: u32) -> Self {
        self.decoder.min_token_quality(quality);
        self
    }

    /// Configures the retry policy for websocket reconnects.
    pub fn websocket_retry_config(mut self, config: &RetryConfiguration) -> Self {
        self.stream_builder = self
            .stream_builder
            .websockets_retry_config(config);
        self
    }

    /// Configures the retry policy for state synchronization.
    pub fn state_synchronizer_retry_config(mut self, config: &RetryConfiguration) -> Self {
        self.stream_builder = self
            .stream_builder
            .state_synchronizer_retry_config(config);
        self
    }

    pub fn get_decoder(&self) -> &TychoStreamDecoder<BlockHeader> {
        &self.decoder
    }

    /// Registers a [`TxDeltaIndexer`] for ephemeral pending-block simulation.
    ///
    /// The indexer is associated with `extractor` (the protocol synchronizer name, e.g.
    /// `"uniswap_v3"`). Use [`build_with_pending`](Self::build_with_pending) to obtain both
    /// the confirmed stream and the pending processor.
    ///
    /// Returns an error if `extractor` names a VM protocol (prefix `"vm:"`), which requires
    /// `update_engine()` and cannot be simulated natively.
    pub fn with_pending_indexer(
        mut self,
        extractor: &str,
        indexer: Box<dyn TxDeltaIndexer>,
    ) -> Result<Self, StreamError> {
        if extractor.starts_with("vm:") {
            return Err(StreamError::SetUpError(format!(
                "extractor '{extractor}' is a VM protocol; TxDeltaIndexer only supports native protocols"
            )));
        }
        self.pending_indexers
            .insert(extractor.to_string(), indexer);
        Ok(self)
    }

    /// Builds the confirmed protocol stream and a [`PendingBlockProcessor`] that stays
    /// in sync with it automatically.
    ///
    /// The stream pipeline forwards every confirmed [`FeedMessage`] to the processor via an
    /// internal unbounded channel — it never blocks waiting for the consumer. The consumer
    /// owns the returned `PendingBlockProcessor` exclusively and may wrap it in whatever
    /// synchronisation primitive suits their use case (e.g. `Mutex` for shared access,
    /// nothing for single-threaded use).
    ///
    /// Call [`generate_pending_update`](PendingBlockProcessor::generate_pending_update) to
    /// simulate a candidate bundle; it drains the channel automatically before computing.
    pub async fn build_with_pending(
        self,
    ) -> Result<
        (impl Stream<Item = Result<Update, StreamDecodeError>>, PendingBlockProcessor),
        StreamError,
    > {
        initialize_hook_handlers().map_err(|e| {
            StreamError::SetUpError(format!("Error initializing hook handlers: {e:?}"))
        })?;
        let (_, rx) = self.stream_builder.build().await?;
        let decoder = Arc::new(self.decoder);

        let (advance_tx, advance_rx) =
            tokio::sync::mpsc::unbounded_channel::<FeedMessage<BlockHeader>>();
        let pending = PendingBlockProcessor::new(
            self.pending_indexers,
            decoder.clone(),
            self.chain,
            advance_rx,
        );

        let stream_end_policy = self.stream_end_policy;
        let stream = Box::pin(
            ReceiverStream::new(rx)
                .take_while(move |msg| match msg {
                    Ok(msg) => {
                        let states = msg.sync_states.values();
                        if stream_end_policy.should_end(states) {
                            error!(
                                "Block stream ended due to {:?}: {:?}",
                                stream_end_policy, msg.sync_states
                            );
                            futures::future::ready(false)
                        } else {
                            futures::future::ready(true)
                        }
                    }
                    Err(e) => {
                        error!("Block stream ended with terminal error: {e}");
                        futures::future::ready(false)
                    }
                })
                .then({
                    let decoder = decoder.clone();
                    move |msg| {
                        let decoder = decoder.clone();
                        let advance_tx = advance_tx.clone();
                        async move {
                            let msg = msg.expect("Safe since stream ends if we receive an error");
                            // Non-blocking: if the receiver is gone we just skip the send.
                            let _ = advance_tx.send(msg.clone());
                            decoder.decode(&msg).await.map_err(|e| {
                                debug!(msg=?msg, "Decode error: {}", e);
                                e
                            })
                        }
                    }
                }),
        );
        let stream = inject_native_wrapper(stream, self.chain);
        Ok((stream, pending))
    }

    /// Builds and returns the configured protocol stream.
    ///
    /// See the module-level docs for details on stream behavior and emitted messages.
    /// This method applies all builder settings and starts the stream.
    pub async fn build(
        self,
    ) -> Result<impl Stream<Item = Result<Update, StreamDecodeError>>, StreamError> {
        initialize_hook_handlers().map_err(|e| {
            StreamError::SetUpError(format!("Error initializing hook handlers: {e:?}"))
        })?;
        let (_, rx) = self.stream_builder.build().await?;
        let decoder = Arc::new(self.decoder);
        let chain = self.chain;

        let stream = Box::pin(
            ReceiverStream::new(rx)
                .take_while(move |msg| match msg {
                    Ok(msg) => {
                        let states = msg.sync_states.values();
                        if self
                            .stream_end_policy
                            .should_end(states)
                        {
                            error!(
                                "Block stream ended due to {:?}: {:?}",
                                self.stream_end_policy, msg.sync_states
                            );
                            futures::future::ready(false)
                        } else {
                            futures::future::ready(true)
                        }
                    }
                    Err(e) => {
                        error!("Block stream ended with terminal error: {e}");
                        futures::future::ready(false)
                    }
                })
                .then({
                    let decoder = decoder.clone();
                    move |msg| {
                        let decoder = decoder.clone();
                        async move {
                            let msg = msg.expect("Save since stream ends if we receive an error");
                            decoder.decode(&msg).await.map_err(|e| {
                                debug!(msg=?msg, "Decode error: {}", e);
                                e
                            })
                        }
                    }
                }),
        );
        let stream = inject_native_wrapper(stream, chain);
        Ok(stream)
    }
}

/// Wraps a decoded protocol stream to inject a `NativeWrapperState` component
/// on the first successful update.
///
/// Skips injection for chains where the native and wrapped-native tokens share
/// the same address (e.g. Starknet).
fn inject_native_wrapper(
    inner: impl Stream<Item = Result<Update, StreamDecodeError>> + Unpin + Send + 'static,
    chain: Chain,
) -> impl Stream<Item = Result<Update, StreamDecodeError>> + Send {
    let has_distinct_wrapper = chain.native_token().address != chain.wrapped_native_token().address;

    if !has_distinct_wrapper {
        return Either::Left(inner);
    }

    Either::Right(
        stream::once(async move {
            let mut inner = inner;
            let first = inner.next().await;
            let modified = first.into_iter().map(move |result| {
                result.map(|mut update| {
                    update.new_pairs.insert(
                        NATIVE_WRAPPER_ID.to_string(),
                        NativeWrapperState::component(chain),
                    );
                    update.states.insert(
                        NATIVE_WRAPPER_ID.to_string(),
                        Box::new(NativeWrapperState::new(chain)),
                    );
                    debug!("Injected native_wrapper component for {chain}");
                    update
                })
            });
            stream::iter(modified).chain(inner)
        })
        .flatten(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use futures::{stream, StreamExt};
    use tycho_common::models::Chain;

    use super::*;
    use crate::protocol::models::Update;

    fn empty_update(block: u64) -> Update {
        Update::new(block, HashMap::new(), HashMap::new())
    }

    #[tokio::test]
    async fn test_inject_native_wrapper_first_message_only() {
        let updates = vec![Ok(empty_update(1)), Ok(empty_update(2)), Ok(empty_update(3))];
        let input = stream::iter(updates);

        let results: Vec<_> = inject_native_wrapper(input, Chain::Ethereum)
            .collect()
            .await;

        assert_eq!(results.len(), 3);

        let first = results[0]
            .as_ref()
            .expect("first update ok");
        assert!(
            first
                .new_pairs
                .contains_key(NATIVE_WRAPPER_ID),
            "first message should have native_wrapper component"
        );
        assert!(
            first
                .states
                .contains_key(NATIVE_WRAPPER_ID),
            "first message should have native_wrapper state"
        );

        let second = results[1]
            .as_ref()
            .expect("second update ok");
        assert!(
            !second
                .new_pairs
                .contains_key(NATIVE_WRAPPER_ID),
            "second message should NOT have native_wrapper component"
        );
        assert!(
            !second
                .states
                .contains_key(NATIVE_WRAPPER_ID),
            "second message should NOT have native_wrapper state"
        );
    }
}
