use std::{
    collections::{HashMap, HashSet},
    slice,
    str::FromStr,
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{Duration, NaiveDateTime};
use deepsize::DeepSizeOf;
use metrics::{counter, gauge, histogram};
use mockall::automock;
use prost::Message;
use tokio::{sync::Mutex, task::JoinHandle};
use tracing::{debug, error, info, info_span, instrument, trace, warn, Instrument};
use tycho_common::{
    models::{
        blockchain::{
            Block, BlockAggregatedChanges, BlockTag, DCIUpdate, EntryPoint, TracingParams,
        },
        contract::{Account, AccountBalance, AccountDelta},
        protocol::{
            ComponentBalance, ProtocolComponent, ProtocolComponentState,
            ProtocolComponentStateDelta,
        },
        token::{Token, TokenOwnerStore},
        Address, Balance, BlockHash, Chain, ChangeType, ComponentId, EntryPointId, ExtractionState,
        ExtractorIdentity, ProtocolType, TxHash,
    },
    storage::{
        BlockIdentifier, ChainGateway, ContractStateGateway, EntryPointGateway,
        ExtractionStateGateway, ProtocolGateway, StorageError,
    },
    traits::TokenPreProcessor,
    Bytes,
};
use tycho_storage::postgres::cache::CachedGateway;
use tycho_substreams::pb::tycho::evm::v1 as tycho_substreams;

use crate::{
    extractor::{
        chain_state::ChainState,
        models::BlockChanges,
        protobuf_deserialisation::TryFromMessage,
        protocol_cache::{ProtocolDataCache, ProtocolMemoryCache},
        reorg_buffer::ReorgBuffer,
        BlockUpdateWithCursor, ExtractionError, Extractor, ExtractorExtension, ExtractorMsg,
    },
    pb::sf::substreams::{
        rpc::v2::{BlockScopedData, BlockUndoSignal, ModulesProgress},
        v1::Clock,
    },
};

pub struct Inner {
    cursor: Vec<u8>,
    last_processed_block: Option<Block>,
    /// Used to give more informative logs
    last_report_ts: NaiveDateTime,
    last_report_block_number: u64,
    first_message_processed: bool,
}

type BatchCommitHandle = tracing::instrument::Instrumented<JoinHandle<Result<(), ExtractionError>>>;

#[derive(Default)]
struct GatewayInner<G> {
    inner: Arc<G>,
    commit_handle: Mutex<Option<BatchCommitHandle>>,
    committed_block_height: Arc<Mutex<Option<u64>>>,
    commit_batch_size: usize,
}

/// Accumulates partial block changes for a single block.
///
/// Each incoming partial is merged immediately via [`BlockChanges::merge_partial`].
/// When a full block signal arrives (`output == None`), the accumulated block
/// is taken from the buffer and processed as a complete block.
type PartialBlockBuffer = Option<BlockChanges>;

pub struct ProtocolExtractor<G, T, E> {
    gateway: GatewayInner<G>,
    name: String,
    chain: Chain,
    chain_state: ChainState,
    protocol_system: String,
    token_pre_processor: T,
    protocol_cache: ProtocolMemoryCache,
    inner: Arc<Mutex<Inner>>,
    protocol_types: HashMap<String, ProtocolType>,
    /// Allows to attach some custom logic, e.g. to fix encoding bugs without resync.
    post_processor: Option<fn(BlockChanges) -> BlockChanges>,
    reorg_buffer: Mutex<ReorgBuffer<BlockUpdateWithCursor<BlockChanges>>>,
    partial_block_buffer: Mutex<PartialBlockBuffer>,
    dci_plugin: Option<Arc<Mutex<E>>>,
}

impl<G, T, E> ProtocolExtractor<G, T, E>
where
    G: ExtractorGateway + 'static,
    T: TokenPreProcessor,
    E: ExtractorExtension,
{
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        gateway: G,
        database_insert_batch_size: usize,
        name: &str,
        chain: Chain,
        chain_state: ChainState,
        protocol_system: String,
        protocol_cache: ProtocolMemoryCache,
        protocol_types: HashMap<String, ProtocolType>,
        token_pre_processor: T,
        post_processor: Option<fn(BlockChanges) -> BlockChanges>,
        dci_plugin: Option<E>,
    ) -> Result<Self, ExtractionError> {
        let dci_plugin = dci_plugin.map(|plugin| Arc::new(Mutex::new(plugin)));

        // check if this extractor has state
        let res = match gateway.get_cursor().await {
            Err(StorageError::NotFound(_, _)) => {
                warn!(?name, ?chain, "No cursor found, starting from the beginning");
                ProtocolExtractor {
                    gateway: GatewayInner {
                        inner: Arc::new(gateway),
                        commit_handle: Default::default(),
                        committed_block_height: Default::default(),
                        commit_batch_size: database_insert_batch_size,
                    },
                    name: name.to_string(),
                    chain,
                    chain_state,
                    protocol_system,
                    token_pre_processor,
                    protocol_cache,
                    inner: Arc::new(Mutex::new(Inner {
                        cursor: vec![],
                        last_processed_block: None,
                        last_report_ts: chrono::Utc::now().naive_utc(),
                        last_report_block_number: 0,
                        first_message_processed: false,
                    })),
                    protocol_types,
                    post_processor,
                    reorg_buffer: Mutex::new(ReorgBuffer::new()),
                    partial_block_buffer: Mutex::new(None),
                    dci_plugin,
                }
            }
            Ok((cursor, block_hash)) => {
                let last_processed_block = gateway
                    .get_block(block_hash)
                    .await
                    .map_err(|err| {
                        ExtractionError::Setup(format!("Failed to fetch latest block: {err}"))
                    })?;

                let cursor_str = String::from_utf8_lossy(&cursor);
                info!(
                    ?name,
                    ?chain,
                    cursor = %cursor_str,
                    block_number = last_processed_block.number,
                    "Found existing cursor! Resuming extractor.."
                );
                ProtocolExtractor {
                    gateway: GatewayInner {
                        inner: Arc::new(gateway),
                        commit_handle: Default::default(),
                        committed_block_height: Arc::new(Mutex::new(Some(
                            last_processed_block.number,
                        ))),
                        commit_batch_size: database_insert_batch_size,
                    },
                    name: name.to_string(),
                    chain,
                    chain_state,
                    inner: Arc::new(Mutex::new(Inner {
                        cursor,
                        last_processed_block: Some(last_processed_block),
                        last_report_ts: chrono::Local::now().naive_utc(),
                        last_report_block_number: 0,
                        first_message_processed: false,
                    })),
                    protocol_system,
                    protocol_cache,
                    token_pre_processor,
                    protocol_types,
                    post_processor,
                    reorg_buffer: Mutex::new(ReorgBuffer::new()),
                    partial_block_buffer: Mutex::new(None),
                    dci_plugin,
                }
            }
            Err(err) => return Err(ExtractionError::Setup(err.to_string())),
        };

        res.ensure_protocol_types().await?;
        Ok(res)
    }

    async fn update_cursor(&self, cursor: String) {
        let mut state = self.inner.lock().await;
        state.cursor = cursor.into();
        state.first_message_processed = true;
    }

    /// Merges a partial block into the buffer, or initializes the buffer if empty.
    async fn buffer_partial_block(&self, msg: &BlockChanges) -> Result<(), ExtractionError> {
        let mut buffer_guard = self.partial_block_buffer.lock().await;
        let updated = if let Some(existing) = buffer_guard.take() {
            existing
                .merge_partial(msg.clone())
                .map_err(|e| ExtractionError::PartialBlockBufferError(e.to_string()))?
        } else {
            msg.clone()
        };
        buffer_guard.replace(updated);
        Ok(())
    }

    async fn is_first_message(&self) -> bool {
        !self
            .inner
            .lock()
            .await
            .first_message_processed
    }

    async fn update_last_processed_block(&self, block: Block) {
        let mut state = self.inner.lock().await;
        state.last_processed_block = Some(block);
    }

    /// Reports sync progress if a minute has passed since the last report.
    async fn report_sync_progress(
        &self,
        block: &Block,
        last_report_block_number: u64,
        time_passed: i64,
    ) {
        let current_block = self.chain_state.current_block().await;
        let distance_to_current = current_block - block.number;
        let blocks_processed = block.number - last_report_block_number;
        let blocks_per_minute = blocks_processed as f64 * 60.0 / time_passed as f64;

        let extractor_id = self.get_id();
        gauge!(
            "extractor_sync_block_rate",
            "chain" => extractor_id.chain.to_string(),
            "extractor" => extractor_id.name.to_string(),
        )
        .set(blocks_per_minute);

        if let Some(time_remaining) =
            Duration::try_minutes((distance_to_current as f64 / blocks_per_minute) as i64)
        {
            let hours = time_remaining.num_hours();
            let minutes = (time_remaining.num_minutes()) % 60;
            info!(
                extractor_id = self.name,
                blocks_per_minute = format!("{blocks_per_minute:.2}"),
                blocks_processed,
                height = block.number,
                estimated_current = current_block,
                time_remaining = format!("{:02}h{:02}m", hours, minutes),
                name = "SyncProgress"
            );
        } else {
            warn!(
                "Failed to convert {} to a duration",
                (distance_to_current as f64 / blocks_per_minute) as i64,
            );
            info!(
                extractor_id = self.name,
                blocks_per_minute = format!("{blocks_per_minute:.2}"),
                blocks_processed,
                height = block.number,
                estimated_current = current_block,
                name = "SyncProgress"
            );
        }
    }

    #[instrument(skip_all, fields(chain = % self.chain, name = % self.name, block_number = % msg.block.number))]
    async fn periodically_report_metrics(&self, msg: &BlockChanges, is_syncing: bool) {
        let mut state = self.inner.lock().await;
        let now = chrono::Local::now().naive_utc();

        // On the first call, initialize the last report time and block number
        if state.last_report_block_number == 0 {
            state.last_report_ts = now;
            state.last_report_block_number = msg.block.number;
            return;
        }

        let time_passed = now
            .signed_duration_since(state.last_report_ts)
            .num_seconds();

        if time_passed >= 60 {
            if is_syncing {
                self.report_sync_progress(&msg.block, state.last_report_block_number, time_passed)
                    .await;
            }

            state.last_report_ts = now;
            state.last_report_block_number = msg.block.number;
            drop(state); // Release the lock before doing potentially slow operations

            // Report the memory usage for all buffers and caches
            gauge!(
                "extractor_reorg_buffer_size",
                "chain" => self.chain.to_string(),
                "extractor" => self.name.clone(),
            )
            .set(
                self.reorg_buffer
                    .lock()
                    .await
                    .deep_size_of() as f64,
            );
            gauge!(
                "extractor_partial_block_buffer_size",
                "chain" => self.chain.to_string(),
                "extractor" => self.name.clone(),
            )
            .set(
                self.partial_block_buffer
                    .lock()
                    .await
                    .deep_size_of() as f64,
            );

            // As the protocol cache is shared with other extractors, we only index it by chain
            gauge!(
                "protocol_cache_size",
                "chain" => self.chain.to_string(),
            )
            .set(self.protocol_cache.size_of().await as f64);

            if let Some(dci_plugin) = &self.dci_plugin {
                let dci = dci_plugin.lock().await;
                gauge!(
                    "dci_cache_size",
                    "chain" => self.chain.to_string(),
                    "extractor" => self.name.clone(),
                )
                .set(dci.cache_size() as f64);
                dci.emit_cache_metrics(&self.chain.to_string(), &self.name);
            }
        }
    }

    #[instrument(skip_all, fields(chain = % self.chain, name = % self.name, block_number = % msg.block.number))]
    async fn handle_tvl_changes(
        &self,
        msg: &mut BlockAggregatedChanges,
    ) -> Result<(), ExtractionError> {
        trace!("Calculating tvl changes");
        if msg.component_balances.is_empty() {
            return Ok(());
        }

        let component_ids = msg
            .component_balances
            .keys()
            .cloned()
            .collect::<Vec<_>>();

        let components = self
            .protocol_cache
            .get_protocol_components(self.protocol_system.as_str(), &component_ids)
            .await?;

        let balance_request = components
            .values()
            .flat_map(|pc| pc.tokens.iter().map(|t| (&pc.id, t)))
            .collect::<Vec<_>>();

        // Merge stored balances with new ones
        let balances = {
            let rb = self.reorg_buffer.lock().await;
            let mut balances = self
                .get_component_balances(&rb, &balance_request)
                .await?;
            // we assume the retrieved balances contain all tokens of the component
            // here, doing this merge the other way around would not be safe.
            balances
                .iter_mut()
                .for_each(|(k, bal)| {
                    bal.extend(
                        msg.component_balances
                            .get(k)
                            .cloned()
                            .unwrap_or_else(HashMap::new)
                            .into_iter(),
                    )
                });
            balances
        };

        // collect token decimals and prices to calculate tvl in the next step
        // most of this data should be in the cache.
        let addresses = balances
            .values()
            .flat_map(|b| b.clone().into_keys())
            .collect::<Vec<_>>();

        let prices = self
            .protocol_cache
            .get_token_prices(&addresses)
            .await?
            .into_iter()
            .zip(addresses.iter())
            .filter_map(|(price, address)| {
                if let Some(p) = price {
                    Some((address.clone(), p))
                } else {
                    trace!(?address, "Missing token price!");
                    None
                }
            })
            .collect::<HashMap<_, _>>();

        // calculate new tvl values
        let tvl_updates = balances
            .iter()
            .map(|(cid, bal)| {
                let component_tvl: f64 = bal
                    .iter()
                    .filter_map(|(addr, bal)| {
                        let price = *prices.get(addr)?;
                        let tvl = bal.balance_float / price;
                        Some(tvl)
                    })
                    .sum();
                (cid.clone(), component_tvl)
            })
            .collect::<HashMap<_, _>>();

        msg.component_tvl = tvl_updates;

        Ok(())
    }

    /// Returns component balances at the tip of the reorg buffer.
    ///
    /// Will return the requested balances at the tip of the reorg buffer. Might need
    /// to go to storage to retrieve balances that are not stored within the buffer.
    async fn get_component_balances(
        &self,
        reorg_buffer: &ReorgBuffer<BlockUpdateWithCursor<BlockChanges>>,
        reverted_balances_keys: &[(&String, &Bytes)],
    ) -> Result<HashMap<String, HashMap<Bytes, ComponentBalance>>, ExtractionError> {
        // First search in the buffer
        let (buffered_balances, missing_balances_keys) =
            reorg_buffer.lookup_component_balances(reverted_balances_keys);

        let missing_balances_map: HashMap<String, Vec<Bytes>> = missing_balances_keys
            .into_iter()
            .fold(HashMap::new(), |mut map, (c_id, token)| {
                map.entry(c_id).or_default().push(token);
                map
            });

        trace!(?missing_balances_map, "Missing component balance keys after buffer lookup");

        // Then get the missing balances from db
        let missing_balances: HashMap<String, HashMap<Bytes, ComponentBalance>> = self
            .gateway
            .inner
            .get_components_balances(
                &missing_balances_map
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<&str>>(),
            )
            .await?;

        let empty = HashMap::<Bytes, ComponentBalance>::new();

        let combined_balances: HashMap<String, HashMap<Bytes, ComponentBalance>> =
            missing_balances_map
                .iter()
                .map(|(id, tokens)| {
                    let balances_for_id = missing_balances
                        .get(id)
                        .unwrap_or(&empty);
                    let filtered_balances: HashMap<_, _> = tokens
                        .iter()
                        .map(|token| {
                            let balance = balances_for_id
                                .get(token)
                                .cloned()
                                .unwrap_or_else(|| ComponentBalance {
                                    token: token.clone(),
                                    balance: Bytes::new(),
                                    balance_float: 0.0,
                                    modify_tx: Bytes::new(),
                                    component_id: id.to_string(),
                                });
                            (token.clone(), balance)
                        })
                        .collect();
                    (id.clone(), filtered_balances)
                })
                .chain(buffered_balances)
                .map(|(id, balances)| {
                    (
                        id,
                        balances
                            .into_iter()
                            .collect::<HashMap<_, _>>(),
                    )
                })
                .fold(HashMap::new(), |mut acc, (c_id, b_changes)| {
                    acc.entry(c_id)
                        .or_default()
                        .extend(b_changes);
                    acc
                });
        Ok(combined_balances)
    }

    /// Returns account balances at the tip of the reorg buffer.
    ///
    /// Will return the requested balances at the tip of the reorg buffer. Might need
    /// to go to storage to retrieve account balances that are not stored within the buffer.
    async fn get_account_balances(
        &self,
        reorg_buffer: &ReorgBuffer<BlockUpdateWithCursor<BlockChanges>>,
        reverted_balances_keys: &[(&Address, &Address)],
    ) -> Result<HashMap<Address, HashMap<Address, AccountBalance>>, ExtractionError> {
        // First search in the buffer
        let (buffered_balances, missing_balances_keys) =
            reorg_buffer.lookup_account_balances(reverted_balances_keys);

        let missing_balances_map: HashMap<Address, Vec<Address>> = missing_balances_keys
            .into_iter()
            .fold(HashMap::new(), |mut map, (account, token)| {
                map.entry(account)
                    .or_default()
                    .push(token);
                map
            });

        trace!(?missing_balances_map, "Missing account balance keys after buffer lookup");

        // Then get the missing account balances from db
        let missing_balances = self
            .gateway
            .inner
            .get_account_balances(
                &missing_balances_map
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>(),
            )
            .await?;

        let empty = HashMap::<Address, AccountBalance>::new();

        let combined_balances: HashMap<Address, HashMap<Address, AccountBalance>> =
            missing_balances_map
                .iter()
                .map(|(account, tokens)| {
                    let balances_for_account = missing_balances
                        .get(account)
                        .unwrap_or(&empty);
                    let filtered_balances: HashMap<_, _> = tokens
                        .iter()
                        .map(|token| {
                            let balance = balances_for_account
                                .get(token)
                                .cloned()
                                .unwrap_or_else(|| AccountBalance {
                                    token: token.clone(),
                                    balance: Bytes::new(),
                                    modify_tx: Bytes::new(),
                                    account: account.clone(),
                                });
                            (token.clone(), balance)
                        })
                        .collect();
                    (account.clone(), filtered_balances)
                })
                .chain(buffered_balances)
                .map(|(account, balances)| {
                    (
                        account,
                        balances
                            .into_iter()
                            .collect::<HashMap<_, _>>(),
                    )
                })
                .fold(HashMap::new(), |mut acc, (account, b_changes)| {
                    acc.entry(account)
                        .or_default()
                        .extend(b_changes);
                    acc
                });
        Ok(combined_balances)
    }

    #[instrument(skip_all, fields(new_tokens_count))]
    async fn construct_currency_tokens(
        &self,
        msg: &BlockChanges,
    ) -> Result<HashMap<Address, Token>, StorageError> {
        let new_token_addresses = msg
            .protocol_components()
            .into_iter()
            .flat_map(|pc| pc.tokens.clone().into_iter())
            .collect::<Vec<_>>();

        // Separate between known and unkown tokens
        let is_token_known = self
            .protocol_cache
            .has_token(&new_token_addresses)
            .await;
        let (unknown_tokens, known_tokens) = new_token_addresses
            .into_iter()
            .zip(is_token_known)
            .partition::<Vec<_>, _>(|(_, known)| !*known);
        let known_tokens = known_tokens
            .into_iter()
            .map(|(addr, _)| addr)
            .collect::<Vec<_>>();
        let unknown_tokens = unknown_tokens
            .into_iter()
            .map(|(addr, _)| addr)
            .collect::<Vec<_>>();
        // Construct unkown tokens using rpc
        let balance_map: HashMap<Address, (Address, Balance)> = msg
            .txs_with_update
            .iter()
            .flat_map(|tx| {
                tx.protocol_components
                    .iter()
                    // Filtering to keep only components with ChangeType::Creation
                    .filter(|(_, c_change)| c_change.change == ChangeType::Creation)
                    .filter_map(|(c_id, change)| {
                        tx.state_updates
                            .get(&change.id)
                            .and_then(|state| {
                                state
                                    .updated_attributes
                                    .get("balance_owner")
                                    .cloned()
                            })
                            .or_else(|| {
                                change
                                    .contract_addresses
                                    // TODO: Currently, it's assumed that the pool is always the
                                    // first contract in the
                                    // protocol component. This approach is a temporary
                                    // workaround and needs to be revisited for a more robust
                                    // solution.
                                    .first()
                                    .cloned()
                                    .or_else(|| Bytes::from_str(&change.id).ok())
                            })
                            .map(|owner| (c_id, owner))
                    })
                    .filter_map(|(c_id, addr)| {
                        tx.balance_changes
                            .get(c_id)
                            .map(|balances| {
                                balances
                                    .iter()
                                    // We currently only keep the latest created pool for
                                    // it's token
                                    .map(move |(token, balance)| {
                                        (token.clone(), (addr.clone(), balance.balance.clone()))
                                    })
                            })
                    })
                    .flatten()
            })
            .collect::<HashMap<_, _>>();
        let tf = TokenOwnerStore::new(balance_map);
        let existing_tokens = self
            .protocol_cache
            .get_tokens(&known_tokens)
            .await?
            .into_iter()
            .flatten()
            .map(|t| (t.address.clone(), t));

        tracing::Span::current().record("new_tokens_count", unknown_tokens.len());
        if !unknown_tokens.is_empty() {
            debug!(?unknown_tokens, block_number = msg.block.number, "NewTokens");
        }

        let new_tokens: HashMap<Address, Token> = self
            .token_pre_processor
            .get_tokens(unknown_tokens, Arc::new(tf), BlockTag::Number(msg.block.number))
            .await
            .into_iter()
            .map(|t| (t.address.clone(), t))
            .chain(existing_tokens)
            .collect();
        Ok(new_tokens)
    }

    /// Process a full block
    ///
    /// This includes updating the reorg buffer, committing to the database, emitting sync updates
    /// and metrics and aggregating the changes).
    #[instrument(skip_all, fields(block_number = msg.block.number, final_block_height))]
    async fn process_full_block_message(
        &self,
        msg: BlockChanges,
        cursor: String,
        final_block_height: u64,
    ) -> Result<Option<ExtractorMsg>, ExtractionError> {
        let is_syncing = final_block_height == msg.block.number;
        if final_block_height > msg.block.number {
            return Err(ExtractionError::ReorgBufferError(format!(
                "Final block height ({}) greater than block number ({}) are unsupported by the reorg buffer",
                final_block_height, msg.block.number
            )));
        }

        let blocks_to_commit = {
            let mut reorg_buffer = self.reorg_buffer.lock().await;

            reorg_buffer
                .insert_block(BlockUpdateWithCursor::new(msg.clone(), cursor.clone()))
                .map_err(ExtractionError::Storage)?;

            if reorg_buffer.count_blocks_before(final_block_height) >=
                self.gateway.commit_batch_size
            {
                reorg_buffer
                    .drain_blocks_until(final_block_height)
                    .map_err(ExtractionError::Storage)?
            } else {
                Vec::new()
            }
        };

        if let Some(last_block) = blocks_to_commit.last() {
            let mut commit_handle_guard = self.gateway.commit_handle.lock().await;
            let gateway = self.gateway.inner.clone();
            let committed_block_height = self
                .gateway
                .committed_block_height
                .clone();
            let last_block_height = last_block.block_update.block.number;
            let batch_size = blocks_to_commit.len();
            let (extractor_name, chain) = (self.name.clone(), self.chain);

            if let Some(db_commit_handle_to_join) = commit_handle_guard.take() {
                let needs_await = !db_commit_handle_to_join
                    .inner()
                    .is_finished();
                let now = chrono::Utc::now().naive_utc();

                let result = db_commit_handle_to_join
                    .instrument(info_span!("await_previous_commit"))
                    .await;

                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(storage_err)) => {
                        return Err(storage_err);
                    }
                    Err(join_err) => {
                        return Err(ExtractionError::Storage(StorageError::Unexpected(format!(
                            "Failed to join database commit task: {join_err}"
                        ))));
                    }
                }

                if needs_await {
                    let wait_time = chrono::Utc::now()
                        .naive_utc()
                        .signed_duration_since(now);
                    trace!(batch_size, block_height = last_block_height, extractor_id = self.name.clone(), chain = %self.chain, wait_time = %wait_time, "CommitTaskAwaited");
                }
            }

            let new_handle = tokio::spawn(async move {
                let now = std::time::Instant::now();

                let mut it = blocks_to_commit.iter().peekable();
                while let Some(block) = it.next() {
                    let force_db_commit = if is_syncing { false } else { it.peek().is_none() };

                    gateway
                        .advance(block.block_update(), block.cursor(), force_db_commit)
                        .await
                        .map_err(ExtractionError::Storage)?;
                }

                let mut committed_hieght_guard = committed_block_height.lock().await;
                *committed_hieght_guard = Some(last_block_height);

                trace!(batch_size, block_height = last_block_height, extractor_id = extractor_name, chain = %chain, "CommitTaskCompleted");

                histogram!(
                    "database_commit_duration_ms", "chain" => chain.to_string(), "extractor" => extractor_name
                )
                .record(now.elapsed().as_millis() as f64);

                Ok(())
            });
            let commit_span = info_span!(
                parent: None,
                "commit_blocks_task",
                batch_size,
                block_height = last_block_height,
                extractor_id = self.name.clone(),
                chain = %self.chain,
            );
            commit_span.follows_from(tracing::Span::current().id());
            let new_handle = new_handle.instrument(commit_span);

            *commit_handle_guard = Some(new_handle);

            trace!(batch_size, block_height = last_block_height, extractor_id = self.name.clone(), chain = %self.chain, "CommitTaskQueued");
        };

        self.update_last_processed_block(msg.block.clone())
            .await;

        self.periodically_report_metrics(&msg, is_syncing)
            .await;

        self.update_cursor(cursor).await;

        let committed_block_height = *self
            .gateway
            .committed_block_height
            .lock()
            .await;
        let mut changes = msg.into_aggregated(committed_block_height)?;
        self.handle_tvl_changes(&mut changes)
            .await?;

        if !is_syncing {
            debug!(
                new_components = changes.new_protocol_components.len(),
                new_tokens = changes.new_tokens.len(),
                account_update = changes.account_deltas.len(),
                state_update = changes.state_deltas.len(),
                tvl_changes = changes.component_tvl.len(),
                "ProcessedFullMessage"
            );
        }
        Ok(Some(Arc::new(changes)))
    }
}

#[async_trait]
impl<G, T, E> Extractor for ProtocolExtractor<G, T, E>
where
    G: ExtractorGateway + 'static,
    T: TokenPreProcessor,
    E: ExtractorExtension,
{
    fn get_id(&self) -> ExtractorIdentity {
        ExtractorIdentity::new(self.chain, &self.name)
    }

    /// Make sure that the protocol types are present in the database.
    async fn ensure_protocol_types(&self) -> Result<(), ExtractionError> {
        let protocol_types: Vec<ProtocolType> = self
            .protocol_types
            .values()
            .cloned()
            .collect();
        self.gateway
            .inner
            .ensure_protocol_types(&protocol_types)
            .await?;
        Ok(())
    }

    async fn get_cursor(&self) -> String {
        String::from_utf8(self.inner.lock().await.cursor.clone()).expect("Cursor is utf8")
    }

    async fn get_last_processed_block(&self) -> Option<Block> {
        self.inner
            .lock()
            .await
            .last_processed_block
            .clone()
    }

    #[instrument(skip_all, fields(block_number, partial_block_index, final_block_height))]
    async fn handle_tick_scoped_data(
        &self,
        inp: BlockScopedData,
    ) -> Result<Option<ExtractorMsg>, ExtractionError> {
        let data = inp
            .output
            .as_ref()
            .ok_or_else(|| {
                ExtractionError::DecodeError("Missing output in block scoped data".into())
            })?
            .map_output
            .as_ref()
            .ok_or_else(|| {
                ExtractionError::DecodeError(
                    "Missing map_output in block scoped data's output".into(),
                )
            })?;
        let msg = {
            let msg = if data.type_url.ends_with("BlockChanges") {
                let raw_msg = tycho_substreams::BlockChanges::decode(data.value.as_slice())?;
                trace!(?raw_msg, "Received BlockChanges message");
                BlockChanges::try_from_message((
                    raw_msg,
                    &self.name,
                    self.chain,
                    &self.protocol_system,
                    &self.protocol_types,
                    inp.final_block_height,
                    inp.partial_index,
                ))
            } else {
                return Err(ExtractionError::DecodeError(format!(
                    "Unknown message type: {}",
                    data.type_url
                )));
            };

            let msg = match msg {
                Ok(changes) => {
                    tracing::Span::current().record("block_number", changes.block.number);
                    tracing::Span::current().record("partial_block_index", inp.partial_index);
                    tracing::Span::current().record("final_block_height", inp.final_block_height);
                    changes
                }
                Err(ExtractionError::Empty) => {
                    self.update_cursor(inp.cursor).await;
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };

            let mut msg = if let Some(post_process_f) = self.post_processor {
                post_process_f(msg)
            } else {
                msg
            };

            if let Some(last_processed_block) = self.get_last_processed_block().await {
                if msg.block.ts.and_utc().timestamp() ==
                    last_processed_block
                        .ts
                        .and_utc()
                        .timestamp()
                {
                    // Blockchains with fast block times (e.g., Arbitrum) may produce blocks with
                    // identical timestamps (measured in seconds). To ensure accurate ordering, we
                    // adjust each block's timestamp by adding a microsecond offset
                    // based on the number of blocks with the same timestamp encountered
                    // so far.
                    // Blocks have a granularity of 1 second, so by adding 1 microsecond to the
                    // timestamp of each block with the same timestamp, we ensure ordering
                    // and prevent duplicate timestamps from being processed.
                    msg.block.ts = last_processed_block.ts + Duration::microseconds(1);
                    trace!(
                        "Block with identical timestamp detected; adjusted timestamp from {:?} to {:?}",
                        last_processed_block.ts, msg.block.ts
                    );
                }
            }

            // Send message to DCI plugin
            if let Some(dci_plugin) = &self.dci_plugin {
                dci_plugin
                    .lock()
                    .await
                    .process_block_update(&mut msg)
                    .await?;
            }

            msg.new_tokens = self
                .construct_currency_tokens(&msg)
                .await?;
            self.protocol_cache
                .add_tokens(msg.new_tokens.values().cloned())
                .await?;
            self.protocol_cache
                .add_components(msg.protocol_components())
                .await?;

            trace!(?msg, "Processing message");

            msg
        };

        // Partial blocks are buffered and emitted immediately but skip reorg buffer and DB commit.
        // Those happen when the full block signal arrives.
        if inp.partial_index.is_some() {
            self.buffer_partial_block(&msg).await?;

            self.update_cursor(inp.cursor).await;

            let committed_block_height = *self
                .gateway
                .committed_block_height
                .lock()
                .await;
            let mut changes = msg.into_aggregated(committed_block_height)?;
            self.handle_tvl_changes(&mut changes)
                .await?;

            debug!(
                new_components = changes.new_protocol_components.len(),
                new_tokens = changes.new_tokens.len(),
                account_update = changes.account_deltas.len(),
                state_update = changes.state_deltas.len(),
                tvl_changes = changes.component_tvl.len(),
                "ProcessedPartialMessage"
            );

            return Ok(Some(Arc::new(changes)));
        };

        return self
            .process_full_block_message(msg, inp.cursor, inp.final_block_height)
            .await;
    }

    async fn collect_and_process_full_block(
        &self,
        cursor: String,
        final_block_height: u64,
        clock: Option<Clock>,
    ) -> Result<Option<ExtractorMsg>, ExtractionError> {
        let msg = self
            .partial_block_buffer
            .lock()
            .await
            .take()
            .ok_or_else(|| {
                let block_num = clock.as_ref().map(|c| c.number).unwrap_or_default();
                ExtractionError::PartialBlockBufferError(format!(
                    "Could not fetch full block data: partial block buffer is empty at block {block_num}"
                ))
            })?;
        self.process_full_block_message(msg, cursor, final_block_height)
            .await
            .map(|opt| {
                // Set partial_block_index to None to indicate that the block is a full block
                opt.map(|mut arc_msg| {
                    Arc::make_mut(&mut arc_msg).partial_block_index = None;
                    arc_msg
                })
            })
    }

    #[instrument(
        skip_all,
        fields(current_block, current_partial_block_index, target_hash, target_number)
    )]
    #[allow(clippy::mutable_key_type)] // Clippy thinks that tuple with Bytes are a mutable type.
    async fn handle_revert(
        &self,
        inp: BlockUndoSignal,
    ) -> Result<Option<ExtractorMsg>, ExtractionError> {
        let block_ref = inp
            .last_valid_block
            .ok_or_else(|| ExtractionError::DecodeError("Revert without block ref".into()))?;

        let block_hash = Bytes::from_str(&block_ref.id).map_err(|err| {
            ExtractionError::DecodeError(format!(
                "Failed to parse {} as block hash: {}",
                block_ref.id, err
            ))
        })?;

        let last_processed_block_number = self
            .get_last_processed_block()
            .await
            .map_or("None".to_string(), |b| b.number.to_string());
        let current_partial_block_index = self
            .partial_block_buffer
            .lock()
            .await
            .as_ref()
            .and_then(|b| b.partial_block_index)
            .map_or("None".to_string(), |i| i.to_string());

        tracing::Span::current().record("current_block", &last_processed_block_number);
        tracing::Span::current()
            .record("current_partial_block_index", &current_partial_block_index);
        tracing::Span::current().record("target_hash", format!("{block_hash:x}"));
        tracing::Span::current().record("target_number", block_ref.number);

        warn!("Chain reorg detected");

        counter!(
            "extractor_revert",
            "extractor" => self.name.clone(),
            "chain" => self.chain.to_string(),
        )
        .increment(1);

        // It can happen that the first received message is an undo signal. In that case we expect
        // to not have the target block in our buffer, therefore we early return and ignore this
        // revert.
        if self.is_first_message().await {
            info!("First message received was a revert. Nothing to revert in the buffer, ignoring it...");
            self.update_cursor(inp.last_valid_cursor)
                .await;
            return Ok(None);
        }

        // Send revert to DCI plugin
        if let Some(dci_plugin) = &self.dci_plugin {
            dci_plugin
                .lock()
                .await
                .process_revert(&block_hash)
                .await?;
        }

        let mut reorg_buffer = self.reorg_buffer.lock().await;

        // Purge the reorg buffer
        let mut reverted_state = reorg_buffer
            .purge(block_hash)
            .map_err(|e| ExtractionError::ReorgBufferError(e.to_string()))?;

        // Purge partial block buffer
        let partial_block = self
            .partial_block_buffer
            .lock()
            .await
            .take();
        let mut revert_partial_block_index = None;
        if let Some(partial) = partial_block {
            // If we have not reverted any full blocks and the partial block buffer is not empty,
            // mark it as a partial revert to avoid sending to full block subscribers.
            if reverted_state.is_empty() {
                revert_partial_block_index = partial.partial_block_index
            }

            // The partial block's cursor should be the current cursor
            let cursor = self.get_cursor().await;
            reverted_state.push(BlockUpdateWithCursor::new(partial, cursor));
        }

        // Handle created and deleted components
        let (reverted_components_creations, reverted_components_deletions) =
            reverted_state.iter().fold(
                (HashMap::new(), HashMap::new()),
                |(mut reverted_creations, mut reverted_deletions), block_msg| {
                    block_msg
                        .block_update()
                        .txs_with_update
                        .iter()
                        .for_each(|update| {
                            update
                                .protocol_components
                                .iter()
                                .for_each(|(id, new_component)| {
                                    /*
                                    For each component, only the oldest creation/deletion needs to be reverted. For example, if a component is created then deleted within the reverted
                                    range of blocks, we only want to remove it (so undo its creation).
                                    As here we go through the reverted state from the oldest to the newest, we just insert the first time we meet a component and ignore it if we meet it again after.
                                    */
                                    if !reverted_deletions.contains_key(id) &&
                                        !reverted_creations.contains_key(id)
                                    {
                                        match new_component.change {
                                            ChangeType::Update => {}
                                            ChangeType::Deletion => {
                                                let mut reverted_deletion = new_component.clone();
                                                reverted_deletion.change = ChangeType::Creation;
                                                reverted_deletions
                                                    .insert(id.clone(), reverted_deletion);
                                            }
                                            ChangeType::Creation => {
                                                let mut reverted_creation = new_component.clone();
                                                reverted_creation.change = ChangeType::Deletion;
                                                reverted_creations
                                                    .insert(id.clone(), reverted_creation);
                                            }
                                        }
                                    }
                                });
                        });
                    (reverted_creations, reverted_deletions)
                },
            );
        trace!(?reverted_components_creations, "Reverted components creations");
        // TODO: For these reverted deletions we need to fetch the whole state (so get it from the
        //  db and apply buffer update)
        trace!(?reverted_components_deletions, "Reverted components deletions");

        // Handle reverted account state
        let reverted_account_state_keys: HashSet<_> = reverted_state
            .iter()
            .flat_map(|block_msg| {
                block_msg
                    .block_update()
                    .txs_with_update
                    .iter()
                    .flat_map(|update| {
                        update
                            .account_deltas
                            .iter()
                            .filter(|(c_id, _)| {
                                !reverted_components_creations.contains_key(&c_id.to_string())
                            })
                            .flat_map(|(c_id, delta)| {
                                delta
                                    .slots
                                    .keys()
                                    .map(move |key| (c_id, key))
                            })
                    })
            })
            .collect();

        let reverted_account_state_keys_vec = reverted_account_state_keys
            .into_iter()
            .collect::<Vec<_>>();

        trace!(?reverted_account_state_keys_vec, "Reverted account state keys");

        // Fetch previous values for every reverted states
        // First search in the buffer
        let (buffered_state, missing) =
            reorg_buffer.lookup_account_state(&reverted_account_state_keys_vec);

        // Then for every missing previous values in the buffer, get the data from our db
        let missing_map: HashMap<Bytes, Vec<Bytes>> =
            missing
                .into_iter()
                .fold(HashMap::new(), |mut acc, (addr, key)| {
                    acc.entry(addr).or_default().push(key);
                    acc
                });

        trace!(?missing_map, "Missing state keys after buffer lookup");

        let missing_contracts = self
            .gateway
            .inner
            .get_contracts(
                &missing_map
                    .keys()
                    .cloned()
                    .collect::<Vec<Address>>(),
            )
            .await
            .map_err(ExtractionError::Storage)?;

        // Then merge the two and cast it to the expected struct
        let combined_states = buffered_state
            .into_iter()
            .chain(
                missing_map
                    .iter()
                    .flat_map(|(address, keys)| {
                        let missing_state = missing_contracts
                            .iter()
                            .find(|state| &state.address == address);
                        keys.iter().map(move |key| {
                            match missing_state {
                                Some(state) => {
                                    // If the state is found, attempt to get the value for the key
                                    state.slots.get(key).map_or_else(
                                        // If the value for this key is not found, return empty
                                        // Bytes
                                        || ((state.address.clone(), key.clone()), Bytes::new()),
                                        // If the key is found, return its value
                                        |value| {
                                            ((state.address.clone(), key.clone()), value.clone())
                                        },
                                    )
                                }
                                None => {
                                    // If the whole account state is not found, return empty Bytes
                                    // for the key
                                    ((address.clone(), key.clone()), Bytes::new())
                                }
                            }
                        })
                    }),
            )
            .collect::<Vec<_>>();

        let account_deltas =
            combined_states
                .into_iter()
                .fold(HashMap::new(), |mut acc, ((addr, key), value)| {
                    acc.entry(addr.clone())
                        .or_insert_with(|| {
                            AccountDelta::new(
                                self.chain,
                                addr,
                                HashMap::new(),
                                None, //TODO: handle balance changes
                                None, //TODO: handle code changes
                                ChangeType::Update,
                            )
                        })
                        .slots
                        .insert(key, Some(value));
                    acc
                });

        // Handle reverted protocol state

        // First pass: collect attributes first introduced with ChangeType::Creation across the
        // entire reverted range. These have no prior state and must be deleted on revert — no
        // buffer or DB lookup is needed or possible for them.
        let reverted_created_attrs: HashMap<String, HashSet<String>> = reverted_state
            .iter()
            .flat_map(|block_msg| {
                block_msg
                    .block_update()
                    .txs_with_update
                    .iter()
                    .flat_map(|update| {
                        update
                            .state_updates
                            .iter()
                            .filter(|(c_id, _)| !reverted_components_creations.contains_key(*c_id))
                            .flat_map(|(c_id, delta)| {
                                delta
                                    .created_attributes
                                    .iter()
                                    .map(move |attr| (c_id.clone(), attr.clone()))
                            })
                    })
            })
            .fold(HashMap::new(), |mut acc, (c_id, attr)| {
                acc.entry(c_id)
                    .or_default()
                    .insert(attr);
                acc
            });

        // Second pass: build the lookup key set, excluding creation attributes (no prior state).
        let reverted_protocol_state_keys: HashSet<_> = reverted_state
            .iter()
            .flat_map(|block_msg| {
                block_msg
                    .block_update()
                    .txs_with_update
                    .iter()
                    .flat_map(|update| {
                        update
                            .state_updates
                            .iter()
                            .filter(|(c_id, _)| !reverted_components_creations.contains_key(*c_id))
                            .flat_map(|(c_id, delta)| {
                                delta
                                    .updated_attributes
                                    .keys()
                                    .filter(|attr| {
                                        !reverted_created_attrs
                                            .get(c_id.as_str())
                                            .is_some_and(|created| created.contains(*attr))
                                    })
                                    .chain(delta.deleted_attributes.iter())
                                    .map(move |key| (c_id, key))
                            })
                    })
            })
            .collect();

        let reverted_protocol_state_keys_vec = reverted_protocol_state_keys
            .into_iter()
            .collect::<Vec<_>>();

        trace!("Reverted state keys {:?}", &reverted_protocol_state_keys_vec);

        // Fetch previous values for every reverted states
        // First search in the buffer
        let (buffered_state, missing) =
            reorg_buffer.lookup_protocol_state(&reverted_protocol_state_keys_vec);

        // Then for every missing previous values in the buffer, get the data from our db
        let missing_map: HashMap<String, Vec<String>> =
            missing
                .into_iter()
                .fold(HashMap::new(), |mut acc, (c_id, key)| {
                    acc.entry(c_id).or_default().push(key);
                    acc
                });

        trace!("Missing state keys after buffer lookup {:?}", &missing_map);

        let missing_components_states = self
            .gateway
            .inner
            .get_protocol_states(
                &missing_map
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<&str>>(),
            )
            .await
            .map_err(ExtractionError::Storage)?;

        let missing_components_states_map: Vec<(ProtocolComponentState, Vec<String>)> = missing_map
            .into_iter()
            .map(|(component_id, keys)| {
                let state = missing_components_states
                    .iter()
                    .find(|comp| comp.component_id == component_id)
                    .cloned()
                    .ok_or(ExtractionError::Storage(StorageError::NotFound(
                        "Component".to_owned(),
                        component_id.to_string(),
                    )))?;
                Ok((state, keys))
            })
            .collect::<Result<Vec<_>, ExtractionError>>()?;

        let mut not_found: HashMap<_, HashSet<_>> = HashMap::new();
        let mut db_states: HashMap<(String, String), Bytes> = HashMap::new();

        for (state, keys) in missing_components_states_map {
            for key in keys {
                if let Some(value) = state.attributes.get(&key) {
                    db_states.insert((state.component_id.clone(), key.clone()), value.clone());
                } else {
                    not_found
                        .entry(state.component_id.clone())
                        .or_default()
                        .insert(key);
                }
            }
        }

        let empty = HashSet::<String>::new();

        let mut state_deltas: HashMap<String, ProtocolComponentStateDelta> = db_states
            .into_iter()
            .chain(buffered_state)
            .fold(HashMap::new(), |mut acc, ((c_id, key), value)| {
                acc.entry(c_id.clone())
                    .or_insert_with(|| ProtocolComponentStateDelta {
                        component_id: c_id.clone(),
                        updated_attributes: HashMap::new(),
                        deleted_attributes: not_found
                            .get(&c_id)
                            .unwrap_or(&empty)
                            .clone(),
                        created_attributes: HashSet::new(),
                    })
                    .updated_attributes
                    .insert(key.clone(), value);
                acc
            });

        // Safety net: components with attrs absent from both buffer and DB still need a delta.
        for (c_id, deleted_keys) in &not_found {
            state_deltas
                .entry(c_id.clone())
                .or_insert_with(|| {
                    ProtocolComponentStateDelta::new(c_id, HashMap::new(), deleted_keys.clone())
                });
        }

        // Revert ChangeType::Creation attributes by emitting deletions — they had no prior state.
        for (c_id, created_keys) in reverted_created_attrs {
            state_deltas
                .entry(c_id.clone())
                .or_insert_with(|| {
                    ProtocolComponentStateDelta::new(&c_id, HashMap::new(), HashSet::new())
                })
                .deleted_attributes
                .extend(created_keys);
        }

        // Handle component balance changes
        let reverted_component_balances_keys: HashSet<(&String, Bytes)> = reverted_state
            .iter()
            .flat_map(|block_msg| {
                block_msg
                    .block_update()
                    .txs_with_update
                    .iter()
                    .flat_map(|update| {
                        update
                            .balance_changes
                            .iter()
                            .filter(|(c_id, _)| !reverted_components_creations.contains_key(*c_id))
                            .flat_map(|(id, balance_change)| {
                                balance_change
                                    .keys()
                                    .map(move |token| (id, token.clone()))
                            })
                    })
            })
            .collect();

        let reverted_component_balances_keys_vec = reverted_component_balances_keys
            .iter()
            .map(|(id, token)| (*id, token))
            .collect::<Vec<_>>();

        trace!("Reverted component balance keys {:?}", &reverted_component_balances_keys_vec);

        let combined_component_balances = self
            .get_component_balances(&reorg_buffer, &reverted_component_balances_keys_vec)
            .await?;

        // Handle account balance changes
        let reverted_account_balances_keys: HashSet<(Bytes, Bytes)> = reverted_state
            .iter()
            .flat_map(|block_msg| {
                block_msg
                    .block_update()
                    .txs_with_update
                    .iter()
                    .flat_map(|update| {
                        update
                            .account_balance_changes
                            .iter()
                            .filter(|(account, _)| account_deltas.contains_key(*account))
                            .flat_map(|(account, balance_change)| {
                                balance_change
                                    .keys()
                                    .map(|token| (account.clone(), token.clone()))
                            })
                    })
            })
            .collect();

        let reverted_account_balances_keys_vec = reverted_account_balances_keys
            .iter()
            .map(|(account, token)| (account, token))
            .collect::<Vec<_>>();

        trace!("Reverted account balance keys {:?}", &reverted_account_balances_keys_vec);

        let combined_account_balances = self
            .get_account_balances(&reorg_buffer, &reverted_account_balances_keys_vec)
            .await?;

        let new_latest_block = reorg_buffer
            .get_most_recent_block()
            .ok_or(ExtractionError::ReorgBufferError("Reorg buffer is empty after purge".into()))?;

        // The latest finalized block height is the one of the last block in the reverted_state
        // (i.e. the most recent block that is reverted)
        let finalized_block_height = reverted_state
            .last()
            .ok_or(ExtractionError::ReorgBufferError("Reorg buffer is empty after purge".into()))?
            .block_update
            .finalized_block_height;

        let revert_message = BlockAggregatedChanges {
            extractor: self.name.clone(),
            chain: self.chain,
            block: new_latest_block.clone(),
            db_committed_block_height: None,
            finalized_block_height,
            revert: true,
            state_deltas,
            account_deltas,
            new_tokens: HashMap::new(),
            new_protocol_components: reverted_components_deletions,
            deleted_protocol_components: reverted_components_creations,
            component_balances: combined_component_balances,
            account_balances: combined_account_balances,
            component_tvl: HashMap::new(),
            dci_update: DCIUpdate::default(), // TODO: get reverted entrypoint info?
            partial_block_index: revert_partial_block_index,
        };

        debug!("Successfully retrieved all previous states during revert!");

        self.update_last_processed_block(new_latest_block)
            .await;
        self.update_cursor(inp.last_valid_cursor)
            .await;

        Ok(Some(Arc::new(revert_message)))
    }

    #[instrument(skip_all)]
    async fn handle_progress(&self, _inp: ModulesProgress) -> Result<(), ExtractionError> {
        todo!()
    }
}
pub struct ExtractorPgGateway {
    name: String,
    chain: Chain,
    db_tx_batch_size: usize,
    state_gateway: CachedGateway,
}

#[automock]
#[async_trait]
pub trait ExtractorGateway: Send + Sync {
    /// Returns the last persisted Substreams cursor together with the hash of the block it was
    /// saved at.
    ///
    /// # Errors
    /// Returns [`StorageError::NotFound`] when no cursor has ever been persisted for this
    /// extractor, i.e. on the very first run before any block has been processed. Callers use
    /// this to distinguish a fresh extractor from a resumed one.
    async fn get_cursor(&self) -> Result<(Vec<u8>, Bytes), StorageError>;

    /// Idempotently registers `new_protocol_types`, inserting any that do not yet exist and
    /// leaving already-present types untouched.
    ///
    /// # Errors
    /// Returns a [`StorageError`] if the protocol types could not be persisted.
    async fn ensure_protocol_types(
        &self,
        new_protocol_types: &[ProtocolType],
    ) -> Result<(), StorageError>;

    /// Persists every change in `changes` for a single block and records `new_cursor` as the
    /// latest processed position.
    ///
    /// All writes for the block are staged within one transaction. When `force_commit` is `true`
    /// the transaction is flushed to the store immediately; otherwise it is only flushed once
    /// enough blocks have accumulated to reach the configured batch size.
    ///
    /// # Errors
    /// Returns a [`StorageError`] if any staged write or the commit fails. On error the block's
    /// changes are not committed.
    async fn advance(
        &self,
        changes: &BlockChanges,
        new_cursor: &str,
        force_commit: bool,
    ) -> Result<(), StorageError>;

    /// Returns the current state of the protocol components identified by `component_ids`.
    ///
    /// Only components that exist in the store are returned: unknown ids are silently omitted
    /// rather than producing an error, so the result may be shorter than `component_ids` (and
    /// empty if none are found).
    ///
    /// # Errors
    /// Returns a [`StorageError`] only on an underlying store failure, never for missing ids.
    async fn get_protocol_states<'a>(
        &self,
        component_ids: &[&'a str],
    ) -> Result<Vec<ProtocolComponentState>, StorageError>;

    /// Returns the contracts at the given `addresses`, including their storage slots.
    ///
    /// Only addresses that exist in the store are returned: unknown addresses are silently
    /// omitted rather than producing an error, so the result may be shorter than `addresses`
    /// (and empty if none are found).
    ///
    /// # Errors
    /// Returns a [`StorageError`] only on an underlying store failure, never for missing
    /// addresses.
    async fn get_contracts(&self, addresses: &[Address]) -> Result<Vec<Account>, StorageError>;

    /// Returns component balances keyed by component id and then by token address.
    ///
    /// Only components that have stored balances appear in the map: unknown or balance-less
    /// `component_ids` are simply absent rather than producing an error, so the map may have
    /// fewer keys than `component_ids` (and be empty if none are found).
    ///
    /// # Errors
    /// Returns a [`StorageError`] only on an underlying store failure, never for missing ids.
    async fn get_components_balances<'a>(
        &self,
        component_ids: &[&'a str],
    ) -> Result<HashMap<String, HashMap<Bytes, ComponentBalance>>, StorageError>;

    /// Returns the block identified by the given hash.
    ///
    /// # Errors
    /// Returns [`StorageError::NotFound`] when no block with that hash exists in the store. Unlike
    /// the collection getters, a missing block is an error because a single value is expected.
    async fn get_block(&self, block_hash: Bytes) -> Result<Block, StorageError>;

    /// Returns account balances keyed by account address and then by token address.
    ///
    /// Only accounts that have stored balances appear in the map: unknown or balance-less
    /// accounts are simply absent rather than producing an error, so the map may have fewer keys
    /// than `accounts` (and be empty if none are found).
    ///
    /// # Errors
    /// Returns a [`StorageError`] only on an underlying store failure, never for missing accounts.
    async fn get_account_balances(
        &self,
        accounts: &[Address],
    ) -> Result<HashMap<Address, HashMap<Address, AccountBalance>>, StorageError>;
}

impl ExtractorPgGateway {
    pub fn new(
        name: &str,
        chain: Chain,
        db_tx_batch_size: usize,
        state_gateway: CachedGateway,
    ) -> Self {
        Self { name: name.to_owned(), chain, db_tx_batch_size, state_gateway }
    }

    #[instrument(skip_all)]
    async fn save_cursor(
        &self,
        new_cursor: &str,
        block_hash: BlockHash,
    ) -> Result<(), StorageError> {
        let state = ExtractionState::new(
            self.name.to_string(),
            self.chain,
            None,
            new_cursor.as_bytes(),
            block_hash,
        );
        self.state_gateway
            .save_state(&state)
            .await?;
        Ok(())
    }

    async fn get_last_extraction_state(&self) -> Result<ExtractionState, StorageError> {
        let state = self
            .state_gateway
            .get_state(&self.name, &self.chain)
            .await?;
        Ok(state)
    }
}

#[async_trait]
impl ExtractorGateway for ExtractorPgGateway {
    async fn get_block(&self, block_hash: Bytes) -> Result<Block, StorageError> {
        self.state_gateway
            .get_block(&BlockIdentifier::Hash(block_hash))
            .await
    }
    async fn get_cursor(&self) -> Result<(Vec<u8>, Bytes), StorageError> {
        let extraction_state = self.get_last_extraction_state().await;
        match extraction_state {
            Ok(state) => Ok((state.cursor, state.block_hash)),
            Err(e) => Err(e),
        }
    }

    async fn ensure_protocol_types(
        &self,
        new_protocol_types: &[ProtocolType],
    ) -> Result<(), StorageError> {
        self.state_gateway
            .add_protocol_types(new_protocol_types)
            .await
    }

    async fn advance(
        &self,
        changes: &BlockChanges,
        new_cursor: &str,
        force_commit: bool,
    ) -> Result<(), StorageError> {
        self.state_gateway
            .start_transaction(&changes.block, Some(self.name.as_str()))
            .await;

        // Insert new tokens
        if !changes.new_tokens.is_empty() {
            let new_tokens = changes
                .new_tokens
                .values()
                .cloned()
                .collect::<Vec<_>>();

            // Commented out to avoid spamming the logs. After https://github.com/propeller-heads/tycho-indexer/commit/94cd54a5a6de99336e467c3abe89b4bcdf5491b2 we are logging every token found in a block, not only new ones.
            // debug!(new_tokens=?new_tokens.iter().map(|t| &t.address).collect::<Vec<_>>(),
            // block_number=changes.block.number, "NewTokens");
            self.state_gateway
                .add_tokens(&new_tokens)
                .await?;
        }

        // Insert block
        self.state_gateway
            .upsert_block(slice::from_ref(&changes.block))
            .await?;

        // Collect transaction aggregated changes
        let mut new_protocol_components: Vec<ProtocolComponent> = vec![];
        let mut state_updates: Vec<(TxHash, ProtocolComponentStateDelta)> = vec![];
        let mut account_changes: Vec<(Bytes, AccountDelta)> = vec![];
        let mut component_balance_changes: Vec<ComponentBalance> = vec![];
        let mut account_balance_changes: Vec<AccountBalance> = vec![];
        let mut protocol_tokens: HashSet<Bytes> = HashSet::new();
        let mut new_entrypoints: HashMap<ComponentId, HashSet<EntryPoint>> = HashMap::new();
        let mut new_entrypoint_params: HashMap<
            EntryPointId,
            HashSet<(TracingParams, ComponentId)>,
        > = HashMap::new();

        for tx_update in changes.txs_with_update.iter() {
            trace!(tx_hash = ?tx_update.tx.hash, "Processing tx");

            // Insert transaction
            self.state_gateway
                .upsert_tx(slice::from_ref(&tx_update.tx))
                .await?;

            let hash: TxHash = tx_update.tx.hash.clone();

            // Map new protocol components
            for new_protocol_component in tx_update.protocol_components.values() {
                new_protocol_components.push(new_protocol_component.clone());
                protocol_tokens.extend(new_protocol_component.tokens.clone());
            }

            // Map new accounts/contracts
            for account_update in tx_update.account_deltas.values() {
                if account_update.is_creation() {
                    let new: Account = account_update.ref_into_account(&tx_update.tx);
                    info!(block_number = ?changes.block.number, contract_address = ?new.address, "NewContract");

                    // Insert new account static values
                    self.state_gateway
                        .insert_contract(&new)
                        .await?;

                    // Collect new account dynamic values for block-scoped batch insert (necessary
                    // for correct versioning)
                    let mut account_delta_creation = account_update.clone();

                    // Set default dynamic values for creation.
                    account_delta_creation.balance = Some(
                        account_delta_creation
                            .balance
                            .unwrap_or_default(),
                    );
                    account_delta_creation.set_code(
                        account_delta_creation
                            .code()
                            .clone()
                            .unwrap_or_default(),
                    );
                    account_changes.push((tx_update.tx.hash.clone(), account_delta_creation));
                } else if account_update.is_update() {
                    account_changes.push((tx_update.tx.hash.clone(), account_update.clone()));
                } else {
                    // log error
                    error!(?account_update, "Invalid account update type");
                }
            }

            // Map protocol state changes
            state_updates.extend(
                tx_update
                    .state_updates
                    .values()
                    .map(|state_change| (hash.clone(), state_change.clone())),
            );

            // Map component balance changes
            component_balance_changes.extend(
                tx_update
                    .balance_changes
                    .clone()
                    .into_values()
                    .flat_map(|tokens_balances| tokens_balances.into_values()),
            );

            // Map account balance changes
            account_balance_changes.extend(
                tx_update
                    .account_balance_changes
                    .clone()
                    .into_values()
                    .flat_map(|tokens_balances| tokens_balances.into_values()),
            );

            // Map new entrypoints
            for (component_id, entrypoints) in tx_update
                .entrypoints
                .clone()
                .into_iter()
            {
                new_entrypoints
                    .entry(component_id)
                    .or_default()
                    .extend(entrypoints);
            }

            // Map new entrypoint params
            for (entrypoint_id, params) in tx_update
                .clone()
                .entrypoint_params
                .into_iter()
            {
                new_entrypoint_params
                    .entry(entrypoint_id)
                    .or_default()
                    .extend(params);
            }
        }

        // Insert new protocol components
        if !new_protocol_components.is_empty() {
            debug!(
                protocol_components = ?new_protocol_components
                    .iter()
                    .map(|pc| &pc.id)
                    .collect::<Vec<_>>(),
                block_number = changes.block.number,
                "NewProtocolComponents"
            );
            self.state_gateway
                .add_protocol_components(new_protocol_components.as_slice())
                .await?;
        }

        // Insert changed accounts
        if !account_changes.is_empty() {
            self.state_gateway
                .update_contracts(account_changes.as_slice())
                .await?;
        }

        // Insert protocol state changes
        if !state_updates.is_empty() {
            self.state_gateway
                .update_protocol_states(state_updates.as_slice())
                .await?;
        }

        // Insert component balance changes
        if !component_balance_changes.is_empty() {
            self.state_gateway
                .add_component_balances(component_balance_changes.as_slice())
                .await?;
        }

        // Insert account balance changes
        if !account_balance_changes.is_empty() {
            self.state_gateway
                .add_account_balances(account_balance_changes.as_slice())
                .await?;
        }

        // Insert new entrypoints
        if !new_entrypoints.is_empty() {
            self.state_gateway
                .insert_entry_points(&new_entrypoints)
                .await?;
        }

        // Insert new entrypoint params
        if !new_entrypoint_params.is_empty() {
            self.state_gateway
                .insert_entry_point_tracing_params(&new_entrypoint_params)
                .await?;
        }

        // Insert trace results
        if !changes.trace_results.is_empty() {
            self.state_gateway
                .upsert_traced_entry_points(changes.trace_results.as_slice())
                .await?;
        }

        self.save_cursor(new_cursor, changes.block.hash.clone())
            .await?;

        let batch_size = if force_commit { 0 } else { self.db_tx_batch_size };
        self.state_gateway
            .commit_transaction(batch_size)
            .await
    }

    async fn get_protocol_states<'a>(
        &self,
        component_ids: &[&'a str],
    ) -> Result<Vec<ProtocolComponentState>, StorageError> {
        self.state_gateway
            .get_protocol_states(&self.chain, None, None, Some(component_ids), false, None)
            .await
            .map(|state_data| state_data.entity)
    }

    async fn get_contracts(&self, addresses: &[Address]) -> Result<Vec<Account>, StorageError> {
        self.state_gateway
            .get_contracts(&self.chain, Some(addresses), None, true, None)
            .await
            .map(|contract_data| contract_data.entity)
    }

    async fn get_components_balances<'a>(
        &self,
        component_ids: &[&'a str],
    ) -> Result<HashMap<String, HashMap<Bytes, ComponentBalance>>, StorageError> {
        self.state_gateway
            .get_component_balances(&self.chain, Some(component_ids), None)
            .await
    }

    async fn get_account_balances(
        &self,
        accounts: &[Address],
    ) -> Result<HashMap<Address, HashMap<Address, AccountBalance>>, StorageError> {
        self.state_gateway
            .get_account_balances(&self.chain, Some(accounts), None)
            .await
    }
}

#[cfg(test)]
mod test {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        thread::sleep,
    };

    use float_eq::assert_float_eq;
    use futures03::FutureExt;
    use mockall::mock;
    use tycho_common::{models::blockchain::TxWithChanges, traits::TokenOwnerFinding};

    use super::*;
    use crate::{
        extractor::MockExtractorExtension,
        pb::sf::substreams::v1::BlockRef,
        testing::{fixtures as pb_fixtures, MockGateway},
    };

    mock! {
        pub TokenPreProcessor {}

        #[async_trait::async_trait]
        impl TokenPreProcessor for TokenPreProcessor {
            async fn get_tokens(
                &self,
                addresses: Vec<Bytes>,
                token_finder: Arc<dyn TokenOwnerFinding>,
                block: BlockTag,
            ) -> Vec<Token>;
        }
    }

    const EXTRACTOR_NAME: &str = "TestExtractor";
    const TEST_PROTOCOL: &str = "TestProtocol";
    async fn create_extractor_with_batch_size(
        gw: MockExtractorGateway,
        batch_size: usize,
    ) -> ProtocolExtractor<MockExtractorGateway, MockTokenPreProcessor, MockExtractorExtension>
    {
        let protocol_types = HashMap::from([("pt_1".to_string(), ProtocolType::default())]);
        let protocol_cache = ProtocolMemoryCache::new(
            Chain::Ethereum,
            chrono::Duration::seconds(900),
            Arc::new(MockGateway::new()),
        );
        let mut preprocessor = MockTokenPreProcessor::new();
        preprocessor
            .expect_get_tokens()
            .returning(|_, _, _| Vec::new());
        ProtocolExtractor::new(
            gw,
            batch_size,
            EXTRACTOR_NAME,
            Chain::Ethereum,
            ChainState::default(),
            TEST_PROTOCOL.to_string(),
            protocol_cache,
            protocol_types,
            preprocessor,
            None,
            None,
        )
        .await
        .expect("Failed to create extractor")
    }

    async fn create_extractor(
        gw: MockExtractorGateway,
    ) -> ProtocolExtractor<MockExtractorGateway, MockTokenPreProcessor, MockExtractorExtension>
    {
        // Default value that flushes the buffer once a single finalized block lands in the buffer.
        // This behavior is consistent with flushing on every finalized block.
        create_extractor_with_batch_size(gw, 1).await
    }

    #[tokio::test]
    async fn test_get_cursor() {
        let mut gw = MockExtractorGateway::new();
        gw.expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));
        gw.expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));

        let extractor = create_extractor(gw).await;
        let res = extractor.get_cursor().await;

        assert_eq!(res, "cursor");
    }

    #[tokio::test]
    async fn test_handle_tick_scoped_data() {
        let mut gw = MockExtractorGateway::new();
        gw.expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));
        gw.expect_advance()
            .times(1)
            .returning(|_, _, _| Ok(()));
        gw.expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));

        let extractor = create_extractor(gw).await;

        extractor
            .handle_tick_scoped_data(pb_fixtures::pb_block_scoped_data(
                tycho_substreams::BlockChanges {
                    block: Some(pb_fixtures::pb_blocks(1)),
                    ..Default::default()
                },
                Some(format!("cursor@{}", 1).as_str()),
                Some(1),
            ))
            .await
            .map(|o| o.map(|_| ()))
            .unwrap()
            .unwrap();

        extractor
            .handle_tick_scoped_data(pb_fixtures::pb_block_scoped_data(
                tycho_substreams::BlockChanges {
                    block: Some(pb_fixtures::pb_blocks(2)),
                    ..Default::default()
                },
                Some(format!("cursor@{}", 2).as_str()),
                Some(2),
            ))
            .await
            .map(|o| o.map(|_| ()))
            .unwrap()
            .unwrap();

        assert_eq!(extractor.get_cursor().await, "cursor@2");
    }

    #[tokio::test]
    async fn test_handle_tick_scoped_respects_batch_async_commit() {
        let mut gw = MockExtractorGateway::new();

        gw.expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));
        gw.expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));

        // Use blocking channels as mock does not support async
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();
        gw.expect_advance()
            .times(4)
            .returning(move |_, _, _| {
                rx.recv().unwrap();
                call_count_clone.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });

        let commit_batch_size = 2;
        let extractor = create_extractor_with_batch_size(gw, commit_batch_size).await;

        let scoped = |n: u64, fin: u64| {
            pb_fixtures::pb_block_scoped_data(
                tycho_substreams::BlockChanges {
                    block: Some(pb_fixtures::pb_blocks(n)),
                    ..Default::default()
                },
                Some(&format!("cursor@{n}")),
                Some(fin),
            )
        };
        let commit_task_is_running = async |ex: &ProtocolExtractor<_, _, _>| -> bool {
            let guard = ex.gateway.commit_handle.lock().await;
            guard
                .as_ref()
                .is_some_and(|h| !h.inner().is_finished())
        };
        let commit_task_is_none = async |ex: &ProtocolExtractor<_, _, _>| -> bool {
            ex.gateway
                .commit_handle
                .lock()
                .await
                .is_none()
        };
        let count_before = async |ex: &ProtocolExtractor<_, _, _>, n: u64| -> usize {
            let g = ex.reorg_buffer.lock().await;
            g.count_blocks_before(n)
        };

        // #1 — no database commit yet (counted 0 out of 2 needed to trigger commit)
        extractor
            .handle_tick_scoped_data(scoped(1, 1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
        assert!(commit_task_is_none(&extractor).await);
        assert_eq!(count_before(&extractor, 1).await, 0);

        // #2 — no database commit yet (counted 1 out of 2 needed to trigger commit)
        extractor
            .handle_tick_scoped_data(scoped(2, 2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
        assert!(commit_task_is_none(&extractor).await);
        assert_eq!(count_before(&extractor, 2).await, 1);

        // #3 — reaches batch size → kicks off first async commit task (which is blocked by channel)
        extractor
            .handle_tick_scoped_data(scoped(3, 3))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
        assert!(commit_task_is_running(&extractor).await);
        assert_eq!(count_before(&extractor, 3).await, 0);

        // #4 — no database commit yet (counted 1 out of 2)
        // new tick processing should still succeed despite the commit task being blocked
        extractor
            .handle_tick_scoped_data(scoped(4, 4))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(call_count.load(Ordering::SeqCst), 0);
        assert!(commit_task_is_running(&extractor).await);
        assert_eq!(count_before(&extractor, 4).await, 1);

        // #5 — should trigger second commit task, however as the first one is still blocked,
        // this tick processing should also be blocked until the first commit task is done
        let mut fifth_tick_future = extractor.handle_tick_scoped_data(scoped(5, 5));

        // Sleep for a short while to ensure the fifth call processing is blocked
        sleep(std::time::Duration::from_millis(100));

        // Confirm the fifth call is *pending* (blocked on the first commit still running)
        assert!(
            fifth_tick_future
                .as_mut()
                .now_or_never()
                .is_none(),
            "should be pending"
        );
        assert_eq!(call_count.load(Ordering::SeqCst), 0);

        // ---- Unblock first commit (allow it to finish) ----
        tx.send(true).unwrap();
        tx.send(true).unwrap();

        // The fifth call (which also triggers a commit) can now complete
        fifth_tick_future
            .await
            .unwrap()
            .unwrap();
        assert_eq!(call_count.load(Ordering::SeqCst), 2, "first commit should be counted");

        // A second commit task should be running now async, still blocked
        assert!(commit_task_is_running(&extractor).await);

        // ---- Unblock second commit and wait for its task handle ----
        tx.send(true).unwrap();
        tx.send(true).unwrap();

        // Await for the commit task to finish
        let handle = {
            let mut g = extractor
                .gateway
                .commit_handle
                .lock()
                .await;
            g.take()
        }
        .expect("expected a running commit task");
        handle.await.unwrap().unwrap();

        assert_eq!(call_count.load(Ordering::SeqCst), 4, "second commit should be counted");
    }

    #[tokio::test]
    async fn test_handle_tick_scoped_data_unknown_message_type() {
        let mut gw = MockExtractorGateway::new();
        gw.expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));
        gw.expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));

        let extractor = create_extractor(gw).await;

        // Construct a BlockScopedData with an unknown type_url
        let data = {
            use crate::pb::sf::substreams::{rpc::v2::*, v1::Clock};
            BlockScopedData {
                output: Some(MapModuleOutput {
                    name: "map_changes".to_owned(),
                    map_output: Some(prost_types::Any {
                        type_url: "tycho.evm.v1.UnknownType".to_owned(),
                        value: vec![],
                    }),
                    debug_info: None,
                }),
                clock: Some(Clock {
                    id: "0x0000000000000000000000000000000000000000000000000000000000000000"
                        .to_owned(),
                    number: 1,
                    timestamp: Some(prost_types::Timestamp { seconds: 1000, nanos: 0 }),
                }),
                cursor: "cursor@1".to_owned(),
                final_block_height: 1,
                debug_map_outputs: vec![],
                debug_store_outputs: vec![],
                attestation: "test".to_owned(),
                is_partial: false,
                partial_index: None,
                is_last_partial: None,
            }
        };

        let result = extractor
            .handle_tick_scoped_data(data)
            .await;
        assert!(
            matches!(result, Err(ExtractionError::DecodeError(msg)) if msg.contains("Unknown message type"))
        );
    }

    #[tokio::test]
    async fn test_handle_tick_scoped_data_skip() {
        let mut gw = MockExtractorGateway::new();
        gw.expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));
        gw.expect_advance()
            .times(0)
            .returning(|_, _, _| Ok(()));
        gw.expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));

        let extractor = create_extractor(gw).await;

        let inp = pb_fixtures::pb_block_scoped_data((), None, None);
        let res = extractor
            .handle_tick_scoped_data(inp)
            .await;

        match res {
            Ok(Some(_)) => panic!("Expected Ok(None) but got Ok(Some(..))"),
            Ok(None) => (), // This is the expected case
            Err(_) => panic!("Expected Ok(None) but got Err(..)"),
        }
        assert_eq!(extractor.get_cursor().await, "cursor@420");
    }

    #[tokio::test]
    async fn test_handle_tick_scoped_data_same_ts() {
        // This test is to ensure that the extractor can handle multiple blocks with the same
        // timestamp
        let mut gw = MockExtractorGateway::new();
        gw.expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));
        gw.expect_advance()
            .times(1)
            .returning(|_, _, _| Ok(()));
        gw.expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));

        let extractor = create_extractor(gw).await;

        let block_1 = pb_fixtures::pb_blocks(1);
        let mut block_2 = pb_fixtures::pb_blocks(2);
        let mut block_3 = pb_fixtures::pb_blocks(3);
        let block_1_ts = block_1.ts;

        block_2.ts = block_1_ts;
        block_3.ts = block_1_ts;

        extractor
            .handle_tick_scoped_data(pb_fixtures::pb_block_scoped_data(
                tycho_substreams::BlockChanges { block: Some(block_1), ..Default::default() },
                Some(format!("cursor@{}", 1).as_str()),
                Some(1),
            ))
            .await
            .map(|o| o.map(|_| ()))
            .unwrap()
            .unwrap();

        extractor
            .handle_tick_scoped_data(pb_fixtures::pb_block_scoped_data(
                tycho_substreams::BlockChanges { block: Some(block_2), ..Default::default() },
                Some(format!("cursor@{}", 2).as_str()),
                Some(2),
            ))
            .await
            .map(|o| o.map(|_| ()))
            .unwrap()
            .unwrap();

        assert_eq!(extractor.get_cursor().await, "cursor@2");
        assert_eq!(
            extractor
                .get_last_processed_block()
                .await
                .unwrap()
                .ts
                .and_utc()
                .timestamp_subsec_micros(),
            1
        );

        extractor
            .handle_tick_scoped_data(pb_fixtures::pb_block_scoped_data(
                tycho_substreams::BlockChanges { block: Some(block_3), ..Default::default() },
                Some(format!("cursor@{}", 3).as_str()),
                Some(2),
            ))
            .await
            .map(|o| o.map(|_| ()))
            .unwrap()
            .unwrap();

        assert_eq!(extractor.get_cursor().await, "cursor@3");
        assert_eq!(
            extractor
                .get_last_processed_block()
                .await
                .unwrap()
                .ts
                .and_utc()
                .timestamp_subsec_micros(),
            2
        );
    }

    fn token_prices() -> HashMap<Bytes, f64> {
        HashMap::from([
            (
                Bytes::from("0x0000000000000000000000000000000000000001"),
                344101538937875300000000000.0,
            ),
            (Bytes::from("0x0000000000000000000000000000000000000002"), 2980881444.0),
        ])
    }

    #[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 2))]
    async fn test_construct_tokens() {
        let msg = BlockChanges::new(
            "ex".to_string(),
            Chain::Ethereum,
            Block::default(),
            0,
            false,
            vec![TxWithChanges {
                protocol_components: HashMap::from([(
                    "TestComponent".to_string(),
                    ProtocolComponent {
                        id: "TestComponent".to_string(),
                        tokens: vec![
                            "0x0000000000000000000000000000000000000001"
                                .parse()
                                .unwrap(),
                            "0x0000000000000000000000000000000000000003"
                                .parse()
                                .unwrap(),
                        ],
                        change: ChangeType::Creation,
                        ..Default::default()
                    },
                )]),
                state_updates: HashMap::from([(
                    "TestComponent".to_string(),
                    ProtocolComponentStateDelta::new(
                        "TestComponent",
                        HashMap::from([(
                            "balance_owner".to_string(),
                            Bytes::from_str("0000000000000000000000000000000000000b0b").unwrap(),
                        )]),
                        HashSet::new(),
                    ),
                )]),
                balance_changes: HashMap::from([(
                    "TestComponent".to_string(),
                    HashMap::from([
                        (
                            Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
                            ComponentBalance {
                                token: Bytes::from_str("0x0000000000000000000000000000000000000001")
                                    .unwrap(),
                                balance: Bytes::from(1000_i32.to_be_bytes()),
                                balance_float: 36522027799.0,
                                modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000011121314").unwrap(),
                                component_id: "TestComponent".to_string(),
                            },
                        ),
                        (
                            Bytes::from_str("0x0000000000000000000000000000000000000003").unwrap(),
                            ComponentBalance {
                                token: Bytes::from_str("0x0000000000000000000000000000000000000003")
                                    .unwrap(),
                                balance: Bytes::from(10000_i32.to_be_bytes()),
                                balance_float: 36522027799.0,
                                modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000011121314").unwrap(),
                                component_id: "TestComponent".to_string(),
                            },
                        ),
                    ]),
                )]),
                ..Default::default()
            }],
            Vec::new(),
        );

        let protocol_gw = MockGateway::new();
        let protocol_cache = ProtocolMemoryCache::new(
            Chain::Ethereum,
            chrono::Duration::seconds(1),
            Arc::new(protocol_gw),
        );
        let t1 = Token::new(
            &Bytes::from("0x0000000000000000000000000000000000000001"),
            "TOK1",
            18,
            0,
            &[],
            Chain::Ethereum,
            100,
        );
        protocol_cache
            .add_tokens([t1.clone()])
            .await
            .expect("adding tokens failed");

        let mut preprocessor = MockTokenPreProcessor::new();
        let t3 = Token::new(
            &Bytes::from_str("0000000000000000000000000000000000000003").unwrap(),
            "TOK3",
            18,
            0,
            &[],
            Chain::Ethereum,
            100,
        );
        let ret = vec![t3.clone()];
        preprocessor
            .expect_get_tokens()
            .return_once(|_, balance_owner_store, _| {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        assert_eq!(
                            balance_owner_store
                                .find_owner(
                                    Bytes::from_str("0000000000000000000000000000000000000003")
                                        .unwrap(),
                                    Bytes::from(1000_i32.to_be_bytes()),
                                )
                                .await
                                .unwrap()
                                .unwrap(),
                            (
                                Bytes::from_str("0000000000000000000000000000000000000b0b")
                                    .unwrap(),
                                Bytes::from(10000_i32.to_be_bytes())
                            )
                        );
                        assert_eq!(
                            balance_owner_store
                                .find_owner(
                                    Bytes::from_str("0000000000000000000000000000000000000001")
                                        .unwrap(),
                                    Bytes::from(1000_i32.to_be_bytes()),
                                )
                                .await
                                .unwrap()
                                .unwrap(),
                            (
                                Bytes::from_str("0000000000000000000000000000000000000b0b")
                                    .unwrap(),
                                Bytes::from(1000_i32.to_be_bytes())
                            )
                        );
                    });
                });
                ret
            });
        let mut extractor_gw = MockExtractorGateway::new();
        extractor_gw
            .expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        extractor_gw
            .expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));

        extractor_gw
            .expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));

        let extractor = ProtocolExtractor::<
            MockExtractorGateway,
            MockTokenPreProcessor,
            MockExtractorExtension,
        >::new(
            extractor_gw,
            1,
            EXTRACTOR_NAME,
            Chain::Ethereum,
            ChainState::default(),
            TEST_PROTOCOL.to_string(),
            protocol_cache,
            HashMap::from([("pt_1".to_string(), ProtocolType::default())]),
            preprocessor,
            None,
            None,
        )
        .await
        .expect("Extractor init failed");
        let exp = HashMap::from([(t1.address.clone(), t1), (t3.address.clone(), t3)]);

        let res = extractor
            .construct_currency_tokens(&msg)
            .await
            .expect("construct_currency_tokens failed");

        assert_eq!(res, exp);
    }

    #[test_log::test(tokio::test)]
    async fn test_handle_tvl_changes() {
        let mut msg = BlockAggregatedChanges {
            component_balances: HashMap::from([(
                "comp1".to_string(),
                HashMap::from([(
                    Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
                    ComponentBalance {
                        token: Bytes::from_str("0x0000000000000000000000000000000000000001")
                            .unwrap(),
                        balance: Bytes::from(
                            "0x00000000000000000000000000000000000000000000003635c9adc5dea00000",
                        ),
                        balance_float: 11_304_207_639.4e18,
                        modify_tx: Bytes::zero(32),
                        component_id: "comp1".to_string(),
                    },
                ),
                    (
                        Bytes::from_str("0x0000000000000000000000000000000000000002").unwrap(),
                        ComponentBalance {
                            token: Bytes::from_str("0x0000000000000000000000000000000000000002")
                                .unwrap(),
                            balance: Bytes::from(
                                "0x00000000000000000000000000000000000000000000003635c9adc5dea00000",
                            ),
                            balance_float: 100_000e6,
                            modify_tx: Bytes::zero(32),
                            component_id: "comp1".to_string(),
                        },
                    )
                ]),
            )]),
            ..Default::default()
        };

        let mut protocol_gw = MockGateway::new();
        protocol_gw
            .expect_get_token_prices()
            .return_once(|_| Box::pin(async { Ok(token_prices()) }));
        let protocol_cache = ProtocolMemoryCache::new(
            Chain::Ethereum,
            chrono::Duration::seconds(1),
            Arc::new(protocol_gw),
        );
        protocol_cache
            .add_components([ProtocolComponent::new(
                "comp1",
                "system1",
                "pt_1",
                Chain::Ethereum,
                vec![
                    Bytes::from("0x0000000000000000000000000000000000000001"),
                    Bytes::from("0x0000000000000000000000000000000000000002"),
                ],
                Vec::new(),
                HashMap::new(),
                ChangeType::Creation,
                Bytes::default(),
                NaiveDateTime::default(),
            )])
            .await
            .expect("adding components failed");
        protocol_cache
            .add_tokens([
                Token::new(
                    &Bytes::from("0x0000000000000000000000000000000000000001"),
                    "PEPE",
                    18,
                    0,
                    &[],
                    Chain::Ethereum,
                    100,
                ),
                Token::new(
                    &Bytes::from("0x0000000000000000000000000000000000000002"),
                    "USDC",
                    6,
                    0,
                    &[],
                    Chain::Ethereum,
                    100,
                ),
            ])
            .await
            .expect("adding tokens failed");

        let preprocessor = MockTokenPreProcessor::new();
        let mut extractor_gw = MockExtractorGateway::new();
        extractor_gw
            .expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        extractor_gw
            .expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));
        extractor_gw
            .expect_get_components_balances()
            .return_once(|_| Ok(HashMap::new()));
        extractor_gw
            .expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));

        let extractor = ProtocolExtractor::<
            MockExtractorGateway,
            MockTokenPreProcessor,
            MockExtractorExtension,
        >::new(
            extractor_gw,
            1,
            "vm_name",
            Chain::Ethereum,
            ChainState::default(),
            "system1".to_string(),
            protocol_cache,
            HashMap::from([("pt_1".to_string(), ProtocolType::default())]),
            preprocessor,
            None,
            None,
        )
        .await
        .expect("extractor init failed");

        let exp_tvl = 66.39849612683253;

        extractor
            .handle_tvl_changes(&mut msg)
            .await
            .expect("handle_tvl_call failed");
        let res = msg
            .component_tvl
            .get("comp1")
            .expect("comp1 tvl not present");

        assert_eq!(msg.component_tvl.len(), 1);
        assert_float_eq!(*res, exp_tvl, rmax <= 0.000_001);
    }

    // ── Partial block handling tests ──────────────────────────────────────

    /// Helper: build a partial `BlockScopedData` populated with changes from the
    /// `pb_vm_block_changes(block_num)`. This method strips balance changes and token balances
    /// to avoid triggering TVL code paths, which would require the CachedGateway to be setup.
    fn make_partial_block_with_data(block_num: u64, partial_index: u32) -> BlockScopedData {
        let mut msg = pb_fixtures::pb_vm_block_changes(block_num as u8 + partial_index as u8);
        msg.block = Some(pb_fixtures::pb_blocks(block_num));
        for tx in &mut msg.changes {
            // Rename a protocol type to be consistent with extractor fixtures.
            for comp in &mut tx.component_changes {
                if let Some(pt) = comp.protocol_type.as_mut() {
                    pt.name = "pt_1".to_string();
                }
            }
            // Strip balance changes to avoid triggering TVL code path.
            tx.balance_changes.clear();
            // Strip token balances to avoid triggering TVL code path.
            for cc in &mut tx.contract_changes {
                cc.token_balances.clear();
            }
        }
        let mut bsd = pb_fixtures::pb_block_scoped_data(
            msg,
            Some(&format!("cursor@{block_num}_p{partial_index}")),
            Some(block_num),
        );
        bsd.partial_index = Some(partial_index);
        bsd.is_partial = true;
        bsd
    }

    /// Helper: build a full `BlockScopedData` with complete block data from
    /// `pb_vm_block_changes(block_num)`. This method strips balance changes and token balances to
    /// avoid triggering TVL code paths, which would require the CachedGateway to be setup.
    fn make_full_block_with_data(block_num: u64, final_block_height: u64) -> BlockScopedData {
        let mut msg = pb_fixtures::pb_vm_block_changes(block_num as u8);
        msg.block = Some(pb_fixtures::pb_blocks(block_num));
        for tx in &mut msg.changes {
            // Rename a protocol type to be consistent with extractor fixtures.
            for comp in &mut tx.component_changes {
                if let Some(pt) = comp.protocol_type.as_mut() {
                    pt.name = "pt_1".to_string();
                }
            }
            // Strip balance changes to avoid triggering TVL code path.
            tx.balance_changes.clear();
            // Strip token balances to avoid triggering TVL code path.
            for cc in &mut tx.contract_changes {
                cc.token_balances.clear();
            }
        }
        pb_fixtures::pb_block_scoped_data(
            msg,
            Some(&format!("cursor@{block_num}")),
            Some(final_block_height),
        )
    }

    #[tokio::test]
    async fn test_partial_blocks_buffered_merged_and_drained() {
        // Lifecycle: partial_0 (data v0) → partial_1 (data v1) → drain
        //
        // Both partials carry non-empty but distinct contract data (different fixture
        // versions produce different contract addresses). The test proves merge by
        // checking that the drained result contains account_delta keys from both.
        let mut gw = MockExtractorGateway::new();
        gw.expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));
        gw.expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));
        // No DB flush: with batch_size=1, a single block at final_block_height == block.number
        // doesn't satisfy count_blocks_before(final_block_height) >= 1.
        gw.expect_advance()
            .times(0)
            .returning(|_, _, _| Ok(()));

        let extractor = create_extractor(gw).await;

        // ── Partial 1 ──
        let result_1 = extractor
            .handle_tick_scoped_data(make_partial_block_with_data(1, 0))
            .await
            .unwrap()
            .unwrap();

        // After partial 0: buffer populated, reorg buffer untouched.
        assert!(extractor
            .partial_block_buffer
            .lock()
            .await
            .is_some());
        assert_eq!(
            extractor
                .reorg_buffer
                .lock()
                .await
                .count_blocks_before(u64::MAX),
            0
        );

        // ── Partial 2 ──
        let result_2 = extractor
            .handle_tick_scoped_data(make_partial_block_with_data(1, 1))
            .await
            .unwrap()
            .unwrap();

        // After partial 1: still buffered, reorg buffer still empty.
        assert!(extractor
            .partial_block_buffer
            .lock()
            .await
            .is_some());
        assert_eq!(
            extractor
                .reorg_buffer
                .lock()
                .await
                .count_blocks_before(u64::MAX),
            0
        );

        // ── Drain: runner calls collect_and_process_full_block after last partial ──
        let full_result = extractor
            .collect_and_process_full_block("cursor@1".to_string(), 1, None)
            .await
            .unwrap()
            .unwrap();

        // After drain: buffer is empty, block entered the reorg buffer.
        assert!(
            extractor
                .partial_block_buffer
                .lock()
                .await
                .is_none(),
            "Buffer should be empty after drain"
        );
        assert_eq!(
            extractor
                .reorg_buffer
                .lock()
                .await
                .count_blocks_before(u64::MAX),
            1,
            "Drained block should be in the reorg buffer (full-block processing path)"
        );

        // Drained block has the correct block number.
        assert_eq!(full_result.block.number, 1);

        // Drained block contains account deltas from BOTH partials, proving merge occurred.
        // If either partial were silently dropped, its addresses would be missing.
        assert!(!full_result.account_deltas.is_empty());
        for addr in result_1.account_deltas.keys() {
            assert!(
                full_result
                    .account_deltas
                    .contains_key(addr),
                "Drained block missing account delta at {addr:x} from partial_1"
            );
        }
        for addr in result_2.account_deltas.keys() {
            assert!(
                full_result
                    .account_deltas
                    .contains_key(addr),
                "Drained block missing account delta at {addr:x} from partial_2"
            );
        }
    }

    #[tokio::test]
    async fn test_collect_and_process_full_block_with_empty_buffer_errors() {
        // collect_and_process_full_block with no buffered partials (runner called it without
        // sending partials first) is an error.
        let mut gw = MockExtractorGateway::new();
        gw.expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));
        gw.expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));
        gw.expect_advance()
            .times(0)
            .returning(|_, _, _| Ok(()));

        let extractor = create_extractor(gw).await;

        let result = extractor
            .collect_and_process_full_block("cursor@1".to_string(), 1, None)
            .await;

        assert!(matches!(result, Err(ExtractionError::PartialBlockBufferError(_))));
    }

    #[tokio::test]
    async fn test_normal_full_block_does_not_interact_with_buffer() {
        // A normal full block (output present, no partial_index) should process normally
        // and leave the buffer untouched (None). Two blocks are sent so that the second
        // triggers the reorg buffer commit (batch_size=1).
        let mut gw = MockExtractorGateway::new();
        gw.expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));
        gw.expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));
        gw.expect_advance()
            .times(0)
            .returning(|_, _, _| Ok(()));

        let extractor = create_extractor(gw).await;

        // Block 1
        extractor
            .handle_tick_scoped_data(pb_fixtures::pb_block_scoped_data(
                tycho_substreams::BlockChanges {
                    block: Some(pb_fixtures::pb_blocks(1)),
                    ..Default::default()
                },
                Some("cursor@1"),
                Some(1),
            ))
            .await
            .unwrap()
            .unwrap();

        // Partial Block Buffer remains unchanged
        assert!(extractor
            .partial_block_buffer
            .lock()
            .await
            .is_none());
        assert_eq!(extractor.get_cursor().await, "cursor@1");
    }

    #[tokio::test]
    async fn test_revert_of_partial_blocks_only() {
        // Gateway needs to handle: initial setup + revert lookups.
        // With batch_size=1 and final_block_height=1, block 1 stays in the reorg buffer
        // (count_blocks_before(1) = 0 < 1) — no advance/commit occurs.
        let mut gw = MockExtractorGateway::new();
        gw.expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));
        gw.expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));
        gw.expect_advance()
            .times(0)
            .returning(|_, _, _| Ok(()));
        // Revert lookups: contracts, protocol states, component balances, account balances.
        gw.expect_get_contracts()
            .returning(|_| Ok(Vec::new()));
        gw.expect_get_protocol_states()
            .returning(|_| Ok(Vec::new()));
        gw.expect_get_components_balances()
            .returning(|_| Ok(HashMap::new()));
        gw.expect_get_account_balances()
            .returning(|_| Ok(HashMap::new()));

        let extractor = create_extractor(gw).await;

        // ── Block 1: empty anchor ──
        extractor
            .handle_tick_scoped_data(pb_fixtures::pb_block_scoped_data(
                tycho_substreams::BlockChanges {
                    block: Some(pb_fixtures::pb_blocks(1)),
                    ..Default::default()
                },
                Some("cursor@1"),
                Some(1),
            ))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            extractor
                .reorg_buffer
                .lock()
                .await
                .count_blocks_before(u64::MAX),
            1,
            "Block 1 should be in the reorg buffer"
        );

        // ── Block 2: partial (not yet finalized) ──
        let result = extractor
            .handle_tick_scoped_data(make_partial_block_with_data(2, 0))
            .await
            .unwrap()
            .unwrap();

        // Verify partial block buffer is populated.
        assert!(
            extractor
                .partial_block_buffer
                .lock()
                .await
                .is_some(),
            "Partial block buffer should be populated after partial block 2"
        );

        // ── Revert to block 1 ──
        let revert_msg = extractor
            .handle_revert(BlockUndoSignal {
                last_valid_block: Some(BlockRef { id: format!("0x{:0>64x}", 1_u64), number: 1 }),
                last_valid_cursor: "cursor@1".into(),
            })
            .await
            .unwrap()
            .unwrap();

        // Partial block buffer must be purged.
        assert!(
            extractor
                .partial_block_buffer
                .lock()
                .await
                .is_none(),
            "Partial block buffer should be purged after revert"
        );

        // Revert message should be marked as a revert.
        assert!(revert_msg.revert, "Revert message should have revert=true");

        // Revert message should be marked as a partial block revert (no full blocks were reverted)
        assert_eq!(
            revert_msg.partial_block_index, result.partial_block_index,
            "Revert message should indicate the index of the reverted partial block"
        );

        // The revert message should include account deltas from the partial block.
        for addr in result.account_deltas.keys() {
            assert!(
                revert_msg
                    .account_deltas
                    .contains_key(addr),
                "Drained block missing account delta at {addr:x} from partial_1"
            );
        }
    }

    #[tokio::test]
    async fn test_revert_of_full_and_partial_blocks() {
        // Revert spanning a full block in the reorg buffer (block 2) *and* a partial
        // block in the partial buffer (block 3). Verifies that state from both sources
        // is included in the revert message.
        let mut gw = MockExtractorGateway::new();
        gw.expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));
        gw.expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));
        gw.expect_advance()
            .times(0)
            .returning(|_, _, _| Ok(()));
        gw.expect_get_contracts()
            .returning(|_| Ok(Vec::new()));
        gw.expect_get_protocol_states()
            .returning(|_| Ok(Vec::new()));
        gw.expect_get_components_balances()
            .returning(|_| Ok(HashMap::new()));
        gw.expect_get_account_balances()
            .returning(|_| Ok(HashMap::new()));

        let extractor = create_extractor_with_batch_size(gw, 1).await;

        // ── Block 1: empty anchor ──
        extractor
            .handle_tick_scoped_data(pb_fixtures::pb_block_scoped_data(
                tycho_substreams::BlockChanges {
                    block: Some(pb_fixtures::pb_blocks(1)),
                    ..Default::default()
                },
                Some("cursor@1"),
                Some(1),
            ))
            .await
            .unwrap()
            .unwrap();

        // ── Block 2: full block, not yet finalized ──
        let result_full = extractor
            .handle_tick_scoped_data(make_full_block_with_data(2, 1))
            .await
            .unwrap()
            .unwrap();

        // ── Block 3: partial ──
        let result_partial = extractor
            .handle_tick_scoped_data(make_partial_block_with_data(3, 0))
            .await
            .unwrap()
            .unwrap();

        // Pre-revert state
        assert_eq!(
            extractor
                .reorg_buffer
                .lock()
                .await
                .count_blocks_before(u64::MAX),
            2,
            "Blocks 1 and 2 should be in the reorg buffer"
        );
        assert!(
            extractor
                .partial_block_buffer
                .lock()
                .await
                .is_some(),
            "Partial block 3 should be buffered"
        );

        // ── Revert to block 1 ──
        let revert_msg = extractor
            .handle_revert(BlockUndoSignal {
                last_valid_block: Some(BlockRef { id: format!("0x{:0>64x}", 1_u64), number: 1 }),
                last_valid_cursor: "cursor@1".into(),
            })
            .await
            .unwrap()
            .unwrap();

        // Partial buffer must be purged.
        assert!(extractor
            .partial_block_buffer
            .lock()
            .await
            .is_none());

        assert!(revert_msg.revert);
        assert_eq!(revert_msg.block.number, 1);
        // Full blocks were also reverted, so partial_block_index should be None.
        assert_eq!(
            revert_msg.partial_block_index, None,
            "Mixed full+partial revert should not be marked as partial"
        );

        // Account deltas from both blocks should be present.
        for addr in result_full.account_deltas.keys() {
            assert!(
                revert_msg
                    .account_deltas
                    .contains_key(addr),
                "Revert message missing account delta at {addr:x} from full block 2"
            );
        }
        for addr in result_partial.account_deltas.keys() {
            assert!(
                revert_msg
                    .account_deltas
                    .contains_key(addr),
                "Revert message missing account delta at {addr:x} from partial block 3"
            );
        }
    }

    // Tests that reverting a partial block containing a brand-new attribute on a non-finalized
    // component does not crash. Prior to the fix, the code errored with "Could not find Component"
    // because the component was non-finalized (absent from DB) and the attribute had no prior
    // value in the remaining buffer. The expected behavior is to emit a deletion for the attribute.
    #[tokio::test]
    async fn test_revert_new_attribute_on_non_finalized_component() {
        use ::tycho_substreams::pb::tycho::evm::v1::{
            Attribute, BlockChanges as PbBlockChanges, ChangeType as PbChangeType, EntityChanges,
            ProtocolComponent as PbProtocolComponent, ProtocolType, TransactionChanges,
        };

        let mut gw = MockExtractorGateway::new();
        gw.expect_ensure_protocol_types()
            .times(1)
            .returning(|_| Ok(()));
        gw.expect_get_cursor()
            .times(1)
            .returning(|| Ok(("cursor".into(), Bytes::default())));
        gw.expect_get_block()
            .times(1)
            .returning(|_| Ok(Block::default()));
        gw.expect_advance()
            .times(0)
            .returning(|_, _, _| Ok(()));
        gw.expect_get_contracts()
            .returning(|_| Ok(Vec::new()));
        // Component is non-finalized: not in DB.
        gw.expect_get_protocol_states()
            .returning(|_| Ok(Vec::new()));
        gw.expect_get_components_balances()
            .returning(|_| Ok(HashMap::new()));
        gw.expect_get_account_balances()
            .returning(|_| Ok(HashMap::new()));

        let extractor = create_extractor(gw).await;

        // Block 1: empty anchor.
        extractor
            .handle_tick_scoped_data(pb_fixtures::pb_block_scoped_data(
                PbBlockChanges { block: Some(pb_fixtures::pb_blocks(1)), ..Default::default() },
                Some("cursor@1"),
                Some(1),
            ))
            .await
            .unwrap()
            .unwrap();

        // Block 2: create `pool_x` with initial attributes. Non-finalized (stays in buffer).
        extractor
            .handle_tick_scoped_data(pb_fixtures::pb_block_scoped_data(
                PbBlockChanges {
                    block: Some(pb_fixtures::pb_blocks(2)),
                    changes: vec![TransactionChanges {
                        tx: Some(pb_fixtures::pb_transactions(2, 0)),
                        component_changes: vec![PbProtocolComponent {
                            id: "pool_x".to_string(),
                            change: PbChangeType::Creation.into(),
                            protocol_type: Some(ProtocolType {
                                name: "pt_1".to_string(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }],
                        entity_changes: vec![EntityChanges {
                            component_id: "pool_x".to_string(),
                            attributes: vec![
                                Attribute {
                                    name: "sqrt_price_x96".to_string(),
                                    value: Bytes::from(1000_u64)
                                        .lpad(32, 0)
                                        .to_vec(),
                                    change: PbChangeType::Creation.into(),
                                },
                                Attribute {
                                    name: "tick".to_string(),
                                    value: Bytes::from(100_u64)
                                        .lpad(32, 0)
                                        .to_vec(),
                                    change: PbChangeType::Creation.into(),
                                },
                            ],
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                Some("cursor@2"),
                Some(1),
            ))
            .await
            .unwrap()
            .unwrap();

        // Partial block 3: first-ever tick attribute on `pool_x` (ChangeType::Creation).
        let mut partial = pb_fixtures::pb_block_scoped_data(
            PbBlockChanges {
                block: Some(pb_fixtures::pb_blocks(3)),
                changes: vec![TransactionChanges {
                    tx: Some(pb_fixtures::pb_transactions(3, 0)),
                    entity_changes: vec![EntityChanges {
                        component_id: "pool_x".to_string(),
                        attributes: vec![Attribute {
                            name: "ticks/100/net-liquidity".to_string(),
                            value: Bytes::from(5000_u64)
                                .lpad(32, 0)
                                .to_vec(),
                            change: PbChangeType::Creation.into(),
                        }],
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            },
            Some("cursor@3_p0"),
            Some(1),
        );
        partial.partial_index = Some(0);
        partial.is_partial = true;
        extractor
            .handle_tick_scoped_data(partial)
            .await
            .unwrap()
            .unwrap();

        // Revert to block 2 — partial block 3 is reverted.
        let revert_msg = extractor
            .handle_revert(BlockUndoSignal {
                last_valid_block: Some(BlockRef { id: format!("0x{:0>64x}", 2_u64), number: 2 }),
                last_valid_cursor: "cursor@2".into(),
            })
            .await
            .expect("handle_revert should not error for non-finalized component with new attr")
            .expect("handle_revert should return a revert message");

        assert!(revert_msg.revert);

        // The revert delta for pool_x must tell consumers to delete the new tick attribute.
        let pool_x_delta = revert_msg
            .state_deltas
            .get("pool_x")
            .expect("state_deltas should contain pool_x");
        assert!(
            pool_x_delta
                .deleted_attributes
                .contains("ticks/100/net-liquidity"),
            "Expected ticks/100/net-liquidity in deleted_attributes, got: {:?}",
            pool_x_delta.deleted_attributes
        );
        assert!(
            pool_x_delta
                .updated_attributes
                .is_empty(),
            "Expected no updated_attributes for pool_x, got: {:?}",
            pool_x_delta.updated_attributes
        );
    }
}

/// It is notoriously hard to mock postgres here, we would need to have traits and abstractions
/// for the connection pooling as well as for transaction handling so the easiest way
/// forward is to just run these tests against a real postgres instance.
///
/// The challenge here is to leave the database empty. So we need to initiate a test transaction
/// and should avoid calling the trait methods which start a transaction of their own. So we do
/// that by moving the main logic of each trait method into a private method and test this
/// method instead.
///
/// Note that it is ok to use higher level db methods here as there is a layer of abstraction
/// between this component and the actual db interactions
#[cfg(test)]
mod test_serial_db {
    use diesel_async::{pooled_connection::deadpool::Pool, AsyncPgConnection};
    use mockall::mock;
    use tycho_common::{
        models::{
            blockchain::TxWithChanges, protocol::QualityRange, ContractId, FinancialType,
            ImplementationType,
        },
        storage::BlockOrTimestamp,
        traits::TokenOwnerFinding,
    };
    use tycho_storage::postgres::{builder::GatewayBuilder, db_fixtures, testing::run_against_db};

    use super::*;
    use crate::{
        extractor::{models::fixtures, MockExtractorExtension},
        pb::sf::substreams::v1::BlockRef,
        testing::fixtures as pb_fixtures,
    };

    mock! {
        pub TokenPreProcessor {}

        #[async_trait::async_trait]
        impl TokenPreProcessor for TokenPreProcessor {
            async fn get_tokens(
                &self,
                addresses: Vec<Bytes>,
                token_finder: Arc<dyn TokenOwnerFinding>,
                block: BlockTag,
            ) -> Vec<Token>;
        }
    }

    const WETH_ADDRESS: &str = "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
    const USDC_ADDRESS: &str = "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";

    // Native contract creation fixtures
    const NATIVE_BLOCK_HASH_0: &str =
        "0xc520bd7f8d7b964b1a6017a3d747375fcefea0f85994e3cc1810c2523b139da8";
    const NATIVE_CREATED_CONTRACT: &str = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";

    // VM contract creation fixtures
    const VM_TX_HASH_0: &str = "0x2f6350a292c0fc918afe67cb893744a080dacb507b0cea4cc07437b8aff23cdb";
    const VM_TX_HASH_1: &str = "0x0d9e0da36cf9f305a189965b248fc79c923619801e8ab5ef158d4fd528a291ad";

    // Ambient Contract
    const VM_CONTRACT: [u8; 20] = [
        0xaa, 0xaa, 0xaa, 0xaa, 0xa2, 0x4e, 0xee, 0xb8, 0xd5, 0x7d, 0x43, 0x12, 0x24, 0xf7, 0x38,
        0x32, 0xbc, 0x34, 0xf6, 0x88,
    ]; // 0xaaaaaaaaa24eeeb8d57d431224f73832bc34f688

    const DATABASE_INSERT_BATCH_SIZE: usize = 2;

    // SETUP
    fn get_mocked_token_pre_processor() -> MockTokenPreProcessor {
        let mut mock_processor = MockTokenPreProcessor::new();
        let new_tokens = vec![
            Token::new(
                &Bytes::from_str(WETH_ADDRESS).expect("Invalid address"),
                "WETH",
                18,
                0,
                &[],
                Default::default(),
                100,
            ),
            Token::new(
                &Bytes::from_str(USDC_ADDRESS).expect("Invalid address"),
                "USDC",
                6,
                0,
                &[],
                Default::default(),
                100,
            ),
            Token::new(
                &Bytes::from_str("6b175474e89094c44da98b954eedeac495271d0f")
                    .expect("Invalid address"),
                "DAI",
                18,
                0,
                &[],
                Default::default(),
                100,
            ),
            Token::new(
                &Bytes::from_str("dAC17F958D2ee523a2206206994597C13D831ec7")
                    .expect("Invalid address"),
                "USDT",
                6,
                0,
                &[],
                Default::default(),
                100,
            ),
        ];
        mock_processor
            .expect_get_tokens()
            .returning(move |_, _, _| new_tokens.clone());

        mock_processor
    }

    async fn setup_gw(
        pool: Pool<AsyncPgConnection>,
        implementation_type: ImplementationType,
    ) -> (ExtractorPgGateway, i64) {
        let mut conn = pool
            .get()
            .await
            .expect("pool should get a connection");
        let chain_id = db_fixtures::insert_chain(&mut conn, "ethereum").await;
        db_fixtures::insert_token(
            &mut conn,
            chain_id,
            "0000000000000000000000000000000000000000",
            "ETH",
            18,
            Some(100),
        )
        .await;

        match implementation_type {
            ImplementationType::Custom => {
                db_fixtures::insert_protocol_type(
                    &mut conn,
                    "pool",
                    Some(FinancialType::Swap),
                    None,
                    Some(ImplementationType::Custom),
                )
                .await;
            }
            ImplementationType::Vm => {
                db_fixtures::insert_protocol_type(&mut conn, "vm:pool", None, None, None).await;
            }
        }

        db_fixtures::insert_token(&mut conn, chain_id, WETH_ADDRESS, "WETH", 18, None).await;
        db_fixtures::insert_token(&mut conn, chain_id, USDC_ADDRESS, "USDC", 6, None).await;

        let db_url = std::env::var("DATABASE_URL").expect("Database URL must be set for testing");
        let (cached_gw, _jh) = GatewayBuilder::new(db_url.as_str())
            .set_chains(&[Chain::Ethereum])
            .set_protocol_systems(&["test".to_string()])
            .build()
            .await
            .expect("failed to build postgres gateway");

        let gw = ExtractorPgGateway::new("test", Chain::Ethereum, 1000, cached_gw);
        (gw, chain_id)
    }

    #[tokio::test]
    async fn test_get_cursor() {
        run_against_db(|pool| async move {
            let (gw, _) = setup_gw(pool, ImplementationType::Vm).await;
            let evm_gw = gw.state_gateway.clone();
            let state = ExtractionState::new(
                "test".to_string(),
                Chain::Ethereum,
                None,
                "cursor@420".as_bytes(),
                Bytes::from_str("88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6")
                    .unwrap(),
            );
            evm_gw
                .start_transaction(&Block::default(), None)
                .await;
            evm_gw
                .upsert_block(&[Block {
                    number: 1,
                    chain: Chain::Ethereum,
                    hash: Bytes::from_str(
                        "88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6",
                    )
                    .unwrap(),
                    parent_hash: Bytes::default(),
                    ts: db_fixtures::yesterday_half_past_midnight(),
                }])
                .await
                .expect("block insertion succeeded");
            evm_gw
                .save_state(&state)
                .await
                .expect("extaction state insertion succeeded");
            evm_gw
                .commit_transaction(0)
                .await
                .expect("gw transaction failed");

            let extraction_state = gw
                .get_last_extraction_state()
                .await
                .expect("get cursor should succeed");

            assert_eq!(extraction_state.cursor, "cursor@420".as_bytes());
        })
        .await;
    }

    fn native_pool_creation() -> BlockChanges {
        BlockChanges::new_with_tokens(
            "native:test".to_owned(),
            Chain::Ethereum,
            Block::new(
                0,
                Chain::Ethereum,
                NATIVE_BLOCK_HASH_0.parse().unwrap(),
                NATIVE_BLOCK_HASH_0.parse().unwrap(),
                "2020-01-01T01:00:00".parse().unwrap(),
            ),
            0,
            false,
            HashMap::from([
                (
                    Bytes::from_str(USDC_ADDRESS).unwrap(),
                    Token::new(
                        &Bytes::from_str(USDC_ADDRESS).unwrap(),
                        "USDC",
                        6,
                        0,
                        &[],
                        Default::default(),
                        100,
                    ),
                ),
                (
                    Bytes::from(WETH_ADDRESS),
                    Token::new(
                        &Bytes::from(WETH_ADDRESS),
                        "WETH",
                        18,
                        0,
                        &[],
                        Default::default(),
                        100,
                    ),
                ),
            ]),
            vec![TxWithChanges {
                tx: fixtures::create_transaction(fixtures::HASH_256_0, NATIVE_BLOCK_HASH_0, 10),
                protocol_components: HashMap::from([(
                    "pool".to_string(),
                    ProtocolComponent {
                        id: NATIVE_CREATED_CONTRACT.to_string(),
                        protocol_system: "test".to_string(),
                        protocol_type_name: "pool".to_string(),
                        chain: Chain::Ethereum,
                        tokens: vec![
                            Bytes::from_str(USDC_ADDRESS).unwrap(),
                            Bytes::from_str(WETH_ADDRESS).unwrap(),
                        ],
                        contract_addresses: vec![],
                        creation_tx: Default::default(),
                        static_attributes: Default::default(),
                        created_at: Default::default(),
                        change: Default::default(),
                    },
                )]),
                ..Default::default()
            }],
        )
    }

    fn vm_account(at_version: u64) -> Account {
        match at_version {
            0 => Account::new(
                Chain::Ethereum,
                "0xaaaaaaaaa24eeeb8d57d431224f73832bc34f688"
                    .parse()
                    .unwrap(),
                "0xaaaaaaaaa24eeeb8d57d431224f73832bc34f688".to_owned(),
                fixtures::slots([(1, 200)]),
                Bytes::from(1000_u64).lpad(32, 0),
                HashMap::from([(
                    Bytes::from_str(WETH_ADDRESS).unwrap(),
                    AccountBalance {
                        token: Bytes::from_str(WETH_ADDRESS).unwrap(),
                        balance: Bytes::from(&[0u8]),
                        modify_tx: Bytes::zero(32),
                        account: "0xaaaaaaaaa24eeeb8d57d431224f73832bc34f688"
                            .parse()
                            .unwrap(),
                    },
                )]),
                vec![0, 0, 0, 0].into(),
                "0xe8e77626586f73b955364c7b4bbf0bb7f7685ebd40e852b164633a4acbd3244c"
                    .parse()
                    .unwrap(),
                Bytes::zero(32),
                VM_TX_HASH_0.parse().unwrap(),
                None,
            ),
            _ => panic!("Unknown version"),
        }
    }

    // Creates a BlockChanges object with a VM contract creation and an account update. Based on an
    // Ambient pool creation
    fn vm_creation_and_update() -> BlockChanges {
        let base_token = Bytes::from_str(WETH_ADDRESS).unwrap();
        let quote_token = Bytes::from_str(USDC_ADDRESS).unwrap();
        let component_id = "ambient_USDC_ETH".to_string();
        BlockChanges::new(
            "vm:ambient".to_owned(),
            Chain::Ethereum,
            Block { hash: Bytes::zero(32), ..Default::default() },
            0,
            false,
            vec![
                TxWithChanges {
                    tx: fixtures::create_transaction(VM_TX_HASH_0, fixtures::HASH_256_0, 1),
                    protocol_components: HashMap::from([(
                        component_id.clone(),
                        ProtocolComponent {
                            id: component_id.clone(),
                            protocol_system: "test".to_string(),
                            protocol_type_name: "vm:pool".to_string(),
                            chain: Chain::Ethereum,
                            tokens: vec![base_token.clone(), quote_token],
                            contract_addresses: vec![Bytes::from(VM_CONTRACT)],
                            static_attributes: Default::default(),
                            change: Default::default(),
                            creation_tx: VM_TX_HASH_0.parse().unwrap(),
                            created_at: Default::default(),
                        },
                    )]),
                    account_deltas: HashMap::from([(
                        VM_CONTRACT.into(),
                        AccountDelta::new(
                            Chain::Ethereum,
                            VM_CONTRACT.into(),
                            HashMap::new(),
                            None,
                            Some(vec![0, 0, 0, 0].into()),
                            ChangeType::Creation,
                        ),
                    )]),
                    balance_changes: HashMap::from([(
                        component_id.clone(),
                        HashMap::from([(
                            base_token.clone(),
                            ComponentBalance {
                                token: base_token.clone(),
                                balance: Bytes::from(&[0u8]),
                                balance_float: 10.0,
                                modify_tx: VM_TX_HASH_0.parse().unwrap(),
                                component_id: component_id.clone(),
                            },
                        )]),
                    )]),
                    account_balance_changes: HashMap::from([(
                        VM_CONTRACT.into(),
                        HashMap::from([(
                            base_token.clone(),
                            AccountBalance {
                                token: base_token.clone(),
                                balance: Bytes::from(&[0u8]),
                                modify_tx: VM_TX_HASH_0.parse().unwrap(),
                                account: VM_CONTRACT.into(),
                            },
                        )]),
                    )]),
                    ..Default::default()
                },
                TxWithChanges {
                    tx: fixtures::create_transaction(VM_TX_HASH_1, fixtures::HASH_256_0, 2),
                    account_deltas: HashMap::from([(
                        VM_CONTRACT.into(),
                        AccountDelta::new(
                            Chain::Ethereum,
                            VM_CONTRACT.into(),
                            fixtures::optional_slots([(1, 200)]),
                            Some(Bytes::from(1000_u64).lpad(32, 0)),
                            None,
                            ChangeType::Update,
                        ),
                    )]),
                    balance_changes: HashMap::from([(
                        component_id.clone(),
                        HashMap::from([(
                            base_token.clone(),
                            ComponentBalance {
                                token: base_token.clone(),
                                balance: Bytes::from(&[0u8]),
                                balance_float: 10.0,
                                modify_tx: VM_TX_HASH_1.parse().unwrap(),
                                component_id: component_id.clone(),
                            },
                        )]),
                    )]),
                    account_balance_changes: HashMap::from([(
                        VM_CONTRACT.into(),
                        HashMap::from([(
                            base_token.clone(),
                            AccountBalance {
                                token: base_token,
                                balance: Bytes::from(&[0u8]),
                                modify_tx: VM_TX_HASH_1.parse().unwrap(),
                                account: VM_CONTRACT.into(),
                            },
                        )]),
                    )]),
                    ..Default::default()
                },
            ],
            Vec::new(),
        )
    }

    // Tests a forward call with a native contract creation and an account update
    #[ignore]
    #[tokio::test]
    async fn test_forward_native_protocol() {
        run_against_db(|pool| async move {
            let (gw, _) = setup_gw(pool, ImplementationType::Custom).await;
            let msg = native_pool_creation();

            let exp = [ProtocolComponent {
                id: NATIVE_CREATED_CONTRACT.to_string(),
                protocol_system: "test".to_string(),
                protocol_type_name: "pool".to_string(),
                chain: Chain::Ethereum,
                tokens: vec![
                    Bytes::from_str(USDC_ADDRESS).unwrap(),
                    Bytes::from_str(WETH_ADDRESS).unwrap(),
                ],
                creation_tx: Bytes::from_str(
                    "0x88e96d4537bea4d9c05d12549907b32561d3bf31f45aae734cdc119f13406cb6",
                )
                .unwrap(),
                ..Default::default()
            }];

            gw.advance(&msg, "cursor@500", false)
                .await
                .expect("upsert should succeed");

            let cached_gw: CachedGateway = gw.state_gateway;
            let res = cached_gw
                .get_protocol_components(
                    &Chain::Ethereum,
                    None,
                    Some([NATIVE_CREATED_CONTRACT].as_slice()),
                    None,
                    None,
                )
                .await
                .expect("test successfully inserted native contract")
                .entity;

            assert_eq!(res, exp);
        })
        .await;
    }

    // Tests processing a new block where a new pool is created and its balances get updated
    #[tokio::test]
    async fn test_forward_vm_protocol() {
        run_against_db(|pool| async move {
            let (gw, _) = setup_gw(pool, ImplementationType::Vm).await;
            let msg = vm_creation_and_update();
            let exp = vm_account(0);

            gw.advance(&msg, "cursor@500", true)
                .await
                .expect("upsert should succeed");

            let cached_gw: CachedGateway = gw.state_gateway;

            let res = cached_gw
                .get_contract(&ContractId::new(Chain::Ethereum, VM_CONTRACT.into()), None, true)
                .await
                .expect("test successfully inserted ambient contract");
            assert_eq!(res, exp);

            let tokens = cached_gw
                .get_tokens(Chain::Ethereum, None, QualityRange::None(), None, None)
                .await
                .unwrap()
                .entity;
            assert_eq!(tokens.len(), 3);

            let protocol_components = cached_gw
                .get_protocol_components(&Chain::Ethereum, None, None, None, None)
                .await
                .unwrap()
                .entity;
            assert_eq!(protocol_components.len(), 1);
            assert_eq!(protocol_components[0].creation_tx, Bytes::from(VM_TX_HASH_0));

            let component_balances = cached_gw
                .get_balance_deltas(
                    &Chain::Ethereum,
                    None,
                    &BlockOrTimestamp::Block(BlockIdentifier::Number((
                        Chain::Ethereum,
                        msg.block.number as i64,
                    ))),
                )
                .await
                .unwrap();

            // TODO: improve asserts
            dbg!(&component_balances);
            assert_eq!(component_balances.len(), 1);
            assert_eq!(component_balances[0].component_id, "ambient_USDC_ETH");
        })
        .await;
    }

    #[test_log::test(tokio::test)]
    async fn test_handle_native_revert() {
        run_against_db(|pool| async move {
            let mut conn = pool
                .get()
                .await
                .expect("pool should get a connection");

            let database_url =
                std::env::var("DATABASE_URL").expect("Database URL must be set for testing");

            db_fixtures::insert_protocol_type(
                &mut conn,
                "pt_1",
                Some(FinancialType::Swap),
                None,
                Some(ImplementationType::Custom),
            )
                .await;

            db_fixtures::insert_protocol_type(
                &mut conn,
                "pt_2",
                Some(FinancialType::Swap),
                None,
                Some(ImplementationType::Custom),
            )
                .await;

            let (cached_gw, _gw_writer_thread) = GatewayBuilder::new(database_url.as_str())
                .set_chains(&[Chain::Ethereum])
                .set_protocol_systems(&["native_protocol_system".to_string()])
                .build()
                .await
                .unwrap();

            let gw = ExtractorPgGateway::new(
                "native_name",
                Chain::Ethereum,
                0,
                cached_gw.clone(),
            );

            let protocol_types = HashMap::from([
                ("pt_1".to_string(), ProtocolType::default()),
                ("pt_2".to_string(), ProtocolType::default()),
            ]);
            let protocol_cache = ProtocolMemoryCache::new(
                Chain::Ethereum,
                chrono::Duration::seconds(900),
                Arc::new(cached_gw),
            );
            let extractor = ProtocolExtractor::<
                ExtractorPgGateway,
                MockTokenPreProcessor,
                MockExtractorExtension,
            >::new(
                gw,
                DATABASE_INSERT_BATCH_SIZE,
                "native_name",
                Chain::Ethereum,
                ChainState::default(),
                "native_protocol_system".to_string(),
                protocol_cache,
                protocol_types,
                get_mocked_token_pre_processor(),
                None,
                None,
            )
                .await
                .expect("Failed to create extractor");

            // Process a sequence of block scoped data.
            for inp in get_native_inp_sequence() {
                extractor
                    .handle_tick_scoped_data(inp)
                    .await
                    .unwrap();
            }

            // Wait for the extractor to finish processing
            if let Some(handle) = extractor.gateway.commit_handle.lock().await.take() {
                handle.await.unwrap().unwrap();
            }

            let client_msg = extractor
                .handle_revert(BlockUndoSignal {
                    last_valid_block: Some(BlockRef {
                        id: "0x0000000000000000000000000000000000000000000000000000000000000003".to_string(),
                        number: 3,
                    }),
                    last_valid_cursor: "cursor@3".into(),
                })
                .await
                .unwrap()
                .unwrap();


            let base_ts = db_fixtures::yesterday_midnight().and_utc().timestamp();
            let block_entity_changes_result = BlockAggregatedChanges {
                extractor: "native_name".to_string(),
                chain: Chain::Ethereum,
                block: Block::new(
                    3,
                    Chain::Ethereum,
                    Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000003").unwrap(),
                    Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000002").unwrap(),
                    chrono::DateTime::from_timestamp(base_ts + 3000, 0).unwrap().naive_utc(),
                ),
                db_committed_block_height: None,
                finalized_block_height: 3,
                revert: true,
                state_deltas: HashMap::from([
                    ("pc_1".to_string(), ProtocolComponentStateDelta {
                        component_id: "pc_1".to_string(),
                        updated_attributes: HashMap::from([
                            ("attr_2".to_string(), Bytes::from(2_u64).lpad(32, 0)),
                            ("attr_1".to_string(), Bytes::from(1000_u64).lpad(32, 0)),
                        ]),
                        deleted_attributes: HashSet::new(),
                        ..Default::default()
                    }),
                ]),
                new_protocol_components: HashMap::from([
                    ("pc_2".to_string(), ProtocolComponent {
                        id: "pc_2".to_string(),
                        protocol_system: "native_protocol_system".to_string(),
                        protocol_type_name: "pt_1".to_string(),
                        chain: Chain::Ethereum,
                        tokens: vec![
                            Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap(),
                            Bytes::from_str(USDC_ADDRESS).unwrap(),
                        ],
                        contract_addresses: vec![],
                        static_attributes: HashMap::new(),
                        change: ChangeType::Creation,
                        creation_tx: Bytes::from_str("0x000000000000000000000000000000000000000000000000000000000000c351").unwrap(),
                        created_at: chrono::DateTime::from_timestamp(base_ts + 5000, 0).unwrap().naive_utc(),
                    }),
                ]),
                deleted_protocol_components: HashMap::from([
                    ("pc_3".to_string(), ProtocolComponent {
                        id: "pc_3".to_string(),
                        protocol_system: "native_protocol_system".to_string(),
                        protocol_type_name: "pt_2".to_string(),
                        chain: Chain::Ethereum,
                        tokens: vec![
                            Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap(),
                            Bytes::from_str(WETH_ADDRESS).unwrap(),
                        ],
                        contract_addresses: vec![],
                        static_attributes: HashMap::new(),
                        change: ChangeType::Deletion,
                        creation_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000009c41").unwrap(),
                        created_at: chrono::DateTime::from_timestamp(base_ts + 4000, 0).unwrap().naive_utc(),
                    }),
                ]),
                component_balances: HashMap::from([
                    ("pc_1".to_string(), HashMap::from([
                        (Bytes::from_str(USDC_ADDRESS).unwrap(), ComponentBalance {
                            token: Bytes::from_str(USDC_ADDRESS).unwrap(),
                            balance: Bytes::from("0x00000001"),
                            balance_float: 1.0,
                            modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
                            component_id: "pc_1".to_string(),
                        }),
                        (Bytes::from_str(WETH_ADDRESS).unwrap(), ComponentBalance {
                            token: Bytes::from_str(WETH_ADDRESS).unwrap(),
                            balance: Bytes::from("0x000003e8"),
                            balance_float: 1000.0,
                            modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000007531").unwrap(),
                            component_id: "pc_1".to_string(),
                        }),
                    ])),
                ]),
                ..Default::default()
            };

            assert_eq!(
                *client_msg,
                block_entity_changes_result
            );
        })
            .await;
    }

    #[test_log::test(tokio::test)]
    async fn test_handle_vm_revert() {
        run_against_db(|pool| async move {
            let mut conn = pool
                .get()
                .await
                .expect("pool should get a connection");

            let database_url =
                std::env::var("DATABASE_URL").expect("Database URL must be set for testing");

            db_fixtures::insert_protocol_type(
                &mut conn,
                "pt_1",
                Some(FinancialType::Swap),
                None,
                Some(ImplementationType::Vm),
            )
                .await;

            db_fixtures::insert_protocol_type(
                &mut conn,
                "pt_2",
                Some(FinancialType::Swap),
                None,
                Some(ImplementationType::Vm),
            )
                .await;

            let (cached_gw, _gw_writer_thread) = GatewayBuilder::new(database_url.as_str())
                .set_chains(&[Chain::Ethereum])
                .set_protocol_systems(&["vm_protocol_system".to_string()])
                .build()
                .await
                .unwrap();

            let gw = ExtractorPgGateway::new(
                "vm_name",
                Chain::Ethereum,
                0,
                cached_gw.clone(),
            );
            let protocol_types = HashMap::from([
                ("pt_1".to_string(), ProtocolType::default()),
                ("pt_2".to_string(), ProtocolType::default()),
            ]);
            let protocol_cache = ProtocolMemoryCache::new(
                Chain::Ethereum,
                chrono::Duration::seconds(900),
                Arc::new(cached_gw),
            );
            let preprocessor = get_mocked_token_pre_processor();
            let extractor = ProtocolExtractor::<
                ExtractorPgGateway,
                MockTokenPreProcessor,
                MockExtractorExtension,
            >::new(
                gw,
                DATABASE_INSERT_BATCH_SIZE,
                "vm_name",
                Chain::Ethereum,
                ChainState::default(),
                "vm_protocol_system".to_string(),
                protocol_cache,
                protocol_types,
                preprocessor,
                None,
                None,
            )
                .await
                .expect("Failed to create extractor");

            // Process a sequence of block scoped data.
            for inp in get_vm_inp_sequence() {
                extractor
                    .handle_tick_scoped_data(inp)
                    .await
                    .unwrap();
            }

            // Wait for the extractor to finish processing
            if let Some(handle) = extractor.gateway.commit_handle.lock().await.take() {
                handle.await.unwrap().unwrap();
            }

            let client_msg = extractor
                .handle_revert(BlockUndoSignal {
                    last_valid_block: Some(BlockRef {
                        id: "0x0000000000000000000000000000000000000000000000000000000000000003".to_string(),
                        number: 3,
                    }),
                    last_valid_cursor: "cursor@3".into(),
                })
                .await
                .unwrap()
                .unwrap();


            let base_ts = db_fixtures::yesterday_midnight().and_utc().timestamp();
            let account1 = Bytes::from_str("0000000000000000000000000000000000000001").unwrap();
            let account2 = Bytes::from_str("0000000000000000000000000000000000000002").unwrap();
            let block_account_expected = BlockAggregatedChanges {
                extractor: "vm_name".to_string(),
                chain: Chain::Ethereum,
                block: Block::new(
                    3,
                    Chain::Ethereum,
                    Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000003").unwrap(),
                    Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000002").unwrap(),
                    chrono::DateTime::from_timestamp(base_ts + 3000, 0).unwrap().naive_utc(),
                ),
                db_committed_block_height: None,
                finalized_block_height: 3,
                revert: true,
                account_deltas: HashMap::from([
                    (account1.clone(), AccountDelta::new(
                        Chain::Ethereum,
                        account1.clone(),
                        HashMap::from([
                            (Bytes::from("0x03"), Some(Bytes::new())),
                            (Bytes::from("0x01"), Some(Bytes::from("0x01"))),
                        ]),
                        None,
                        None,
                        ChangeType::Update,
                    )),
                    (account2.clone(), AccountDelta::new(
                        Chain::Ethereum,
                        account2.clone(),
                        HashMap::from([
                            (Bytes::from("0x01"), Some(Bytes::from("0x02"))),
                        ]),
                        None,
                        None,
                        ChangeType::Update,
                    )),
                ]),
                deleted_protocol_components: HashMap::from([
                    ("pc_3".to_string(), ProtocolComponent {
                        id: "pc_3".to_string(),
                        protocol_system: "vm_protocol_system".to_string(),
                        protocol_type_name: "pt_1".to_string(),
                        chain: Chain::Ethereum,
                        tokens: vec![
                            Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap(),
                            Bytes::from_str(USDC_ADDRESS).unwrap(),
                        ],
                        contract_addresses: vec![
                            account1.clone(),
                        ],
                        static_attributes: HashMap::new(),
                        change: ChangeType::Deletion,
                        creation_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000009c41").unwrap(),
                        created_at: chrono::DateTime::from_timestamp(base_ts + 4000, 0).unwrap().naive_utc(),
                    }),
                ]),
                component_balances: HashMap::from([
                    ("pc_1".to_string(), HashMap::from([
                        (Bytes::from_str(USDC_ADDRESS).unwrap(), ComponentBalance {
                            token: Bytes::from_str(USDC_ADDRESS).unwrap(),
                            balance: Bytes::from("0x00000064"),
                            balance_float: 100.0,
                            modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000007532").unwrap(),
                            component_id: "pc_1".to_string(),
                        }),
                        (Bytes::from_str(WETH_ADDRESS).unwrap(), ComponentBalance {
                            token: Bytes::from_str(WETH_ADDRESS).unwrap(),
                            balance: Bytes::from("0x00000001"),
                            balance_float: 1.0,
                            modify_tx: Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
                            component_id: "pc_1".to_string(),
                        }),
                    ])),
                ]),
                account_balances: HashMap::from([
                    (account1.clone(), HashMap::from([
                        (Bytes::from_str(WETH_ADDRESS).unwrap(), AccountBalance {
                        token: Bytes::from_str(WETH_ADDRESS).unwrap(),
                        balance: Bytes::from("0x00000001"),
                        modify_tx:Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000000").unwrap(),
                        account: account1.clone(),
                        }),
                        (Bytes::from_str(USDC_ADDRESS).unwrap(), AccountBalance {
                        token: Bytes::from_str(USDC_ADDRESS).unwrap(),
                        balance: Bytes::from("0x00000064"),
                        modify_tx:Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000007532").unwrap(),
                        account: account1.clone(),
                        }),
                    ])),
                    (account2.clone(), HashMap::from([
                        (Bytes::from_str(USDC_ADDRESS).unwrap(), AccountBalance {
                        token: Bytes::from_str(USDC_ADDRESS).unwrap(),
                        balance: Bytes::from("0x00000001"),
                        modify_tx:Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000007531").unwrap(),
                        account: account2.clone(),
                        }),
                    ]))
                ]),
                ..Default::default()
            };

            assert_eq!(
                *client_msg,
                block_account_expected
            );
        })
            .await;
    }

    #[test_log::test(tokio::test)]
    async fn test_timestamp_conflict_resolution_with_revert() {
        run_against_db(|pool| async move {
            let mut conn = pool
                .get()
                .await
                .expect("pool should get a connection");

            let database_url =
                std::env::var("DATABASE_URL").expect("Database URL must be set for testing");

            db_fixtures::insert_protocol_type(
                &mut conn,
                "pt_1",
                Some(FinancialType::Swap),
                None,
                Some(ImplementationType::Vm),
            )
            .await;

            db_fixtures::insert_protocol_type(
                &mut conn,
                "pt_2",
                Some(FinancialType::Swap),
                None,
                Some(ImplementationType::Vm),
            )
            .await;

            let (cached_gw, _gw_writer_thread) = GatewayBuilder::new(database_url.as_str())
                .set_chains(&[Chain::Ethereum])
                .set_protocol_systems(&["vm_protocol_system".to_string()])
                .build()
                .await
                .unwrap();

            let gw = ExtractorPgGateway::new("vm_name", Chain::Ethereum, 0, cached_gw.clone());
            let protocol_types = HashMap::from([
                ("pt_1".to_string(), ProtocolType::default()),
                ("pt_2".to_string(), ProtocolType::default()),
            ]);
            let protocol_cache = ProtocolMemoryCache::new(
                Chain::Ethereum,
                chrono::Duration::seconds(900),
                Arc::new(cached_gw),
            );
            let preprocessor = get_mocked_token_pre_processor();
            let extractor = ProtocolExtractor::<
                ExtractorPgGateway,
                MockTokenPreProcessor,
                MockExtractorExtension,
            >::new(
                gw,
                DATABASE_INSERT_BATCH_SIZE,
                "vm_name",
                Chain::Ethereum,
                ChainState::default(),
                "vm_protocol_system".to_string(),
                protocol_cache,
                protocol_types,
                preprocessor,
                None,
                None,
            )
            .await
            .expect("Failed to create extractor");

            // Send a sequence of block scoped data with the same timestamp.
            let base_ts = db_fixtures::yesterday_midnight()
                .and_utc()
                .timestamp() as u64;
            let versions = [1, 2, 3, 4];

            let inp_sequence = versions
                .into_iter()
                .map(|version| {
                    pb_fixtures::pb_block_scoped_data(
                        tycho_substreams::BlockChanges {
                            block: Some(tycho_substreams::Block {
                                number: version,
                                hash: Bytes::from(version)
                                    .lpad(32, 0)
                                    .to_vec(),
                                parent_hash: Bytes::from(version - 1)
                                    .lpad(32, 0)
                                    .to_vec(),
                                ts: {
                                    if version == 4 {
                                        base_ts + 1
                                    } else {
                                        base_ts
                                    }
                                },
                            }),
                            ..Default::default()
                        },
                        Some(format!("cursor@{version}").as_str()),
                        Some(1), // Buffered
                    )
                })
                .collect::<Vec<_>>() // materialize into Vec
                .into_iter();

            for inp in inp_sequence {
                extractor
                    .handle_tick_scoped_data(inp)
                    .await
                    .unwrap();
            }

            // Wait for the extractor to finish processing
            if let Some(handle) = extractor
                .gateway
                .commit_handle
                .lock()
                .await
                .take()
            {
                handle.await.unwrap().unwrap();
            }

            // Revert block #4, which had a timestamp of 1 second after block #3.
            extractor
                .handle_revert(BlockUndoSignal {
                    last_valid_block: Some(BlockRef {
                        id: "0x0000000000000000000000000000000000000000000000000000000000000003"
                            .to_string(),
                        number: 3,
                    }),
                    last_valid_cursor: "cursor@3".into(),
                })
                .await
                .unwrap()
                .unwrap();

            // New block #4 should have the same timestamp as block #3.
            extractor
                .handle_tick_scoped_data(pb_fixtures::pb_block_scoped_data(
                    tycho_substreams::BlockChanges {
                        block: Some(tycho_substreams::Block {
                            number: 4,
                            hash: Bytes::from(4_u64).lpad(32, 0).to_vec(),
                            parent_hash: Bytes::from(3_u64).lpad(32, 0).to_vec(),
                            ts: base_ts,
                        }),
                        ..Default::default()
                    },
                    Some(format!("cursor@{}", 4).as_str()),
                    Some(1), // Buffered
                ))
                .await
                .unwrap()
                .unwrap();

            assert_eq!(extractor.get_cursor().await, "cursor@4");
            // New block #4 should have the same timestamp as block #3 + 3 microseconds
            assert_eq!(
                extractor
                    .get_last_processed_block()
                    .await
                    .unwrap()
                    .ts
                    .and_utc()
                    .timestamp_subsec_micros(),
                3
            );
        })
        .await;
    }

    fn get_native_inp_sequence(
    ) -> impl Iterator<Item = crate::pb::sf::substreams::rpc::v2::BlockScopedData> {
        vec![
            pb_fixtures::pb_block_scoped_data(
                pb_fixtures::pb_native_block_changes(1),
                Some(format!("cursor@{}", 1).as_str()),
                Some(1), // Syncing (buffered)
            ),
            pb_fixtures::pb_block_scoped_data(
                pb_fixtures::pb_native_block_changes(2),
                Some(format!("cursor@{}", 2).as_str()),
                Some(1), // Buffered
            ),
            pb_fixtures::pb_block_scoped_data(
                pb_fixtures::pb_native_block_changes(3),
                Some(format!("cursor@{}", 3).as_str()),
                Some(1), // Buffered
            ),
            pb_fixtures::pb_block_scoped_data(
                pb_fixtures::pb_native_block_changes(4),
                Some(format!("cursor@{}", 4).as_str()),
                Some(1), // Buffered
            ),
            pb_fixtures::pb_block_scoped_data(
                pb_fixtures::pb_native_block_changes(5),
                Some(format!("cursor@{}", 5).as_str()),
                Some(3), // Buffered + flush 1 + 2
            ),
        ]
        .into_iter()
    }

    fn get_vm_inp_sequence(
    ) -> impl Iterator<Item = crate::pb::sf::substreams::rpc::v2::BlockScopedData> {
        vec![
            pb_fixtures::pb_block_scoped_data(
                pb_fixtures::pb_vm_block_changes(1),
                Some(format!("cursor@{}", 1).as_str()),
                Some(1), // Syncing (buffered)
            ),
            pb_fixtures::pb_block_scoped_data(
                pb_fixtures::pb_vm_block_changes(2),
                Some(format!("cursor@{}", 2).as_str()),
                Some(1), // Buffered
            ),
            pb_fixtures::pb_block_scoped_data(
                pb_fixtures::pb_vm_block_changes(3),
                Some(format!("cursor@{}", 3).as_str()),
                Some(1), // Buffered
            ),
            pb_fixtures::pb_block_scoped_data(
                pb_fixtures::pb_vm_block_changes(4),
                Some(format!("cursor@{}", 4).as_str()),
                Some(1), // Buffered
            ),
            pb_fixtures::pb_block_scoped_data(
                pb_fixtures::pb_vm_block_changes(5),
                Some(format!("cursor@{}", 5).as_str()),
                Some(3), // Buffered + flush 1 + 2
            ),
        ]
        .into_iter()
    }
}
