//! Real extractors fed recorded substreams messages, with the RPC boundaries mocked, for tests
//! that check what extractors emit.
//!
//! The streams are written by [`streams`] with the protobuf test builders rather than captured
//! from a live endpoint, so each one covers its extractor's paths with consistent hashes, parents
//! and finality. The RPC services the extractors call answer from [`rpc`] the same way for the
//! same request, so what an extractor emits depends only on the messages it is fed and on the
//! database. Storage is a real Postgres database: what a restarted extractor knows comes from what
//! it committed. Block timestamps depend on the day a stream is written; see
//! [`RecordedStream::base_ts`].

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
    sync::Arc,
};

use chrono::NaiveDateTime;
use diesel_async::{pooled_connection::deadpool::Pool, AsyncPgConnection};
use tycho_common::{
    models::{
        blockchain::{Block, BlockAggregatedChanges},
        Address, Chain, FinancialType, ImplementationType, ProtocolType,
    },
    traits::{MockAccountExtractor, MockEntryPointTracer},
};
use tycho_storage::postgres::{builder::GatewayBuilder, cache::CachedGateway, db_fixtures};

use crate::{
    extractor::{
        chain_state::ChainState,
        dynamic_contract_indexer::dci::DynamicContractIndexer,
        protocol_cache::ProtocolMemoryCache,
        protocol_extractor::{ExtractorPgGateway, ProtocolExtractor},
        runner::ExtractorRunner,
        Extractor,
    },
    pb::sf::substreams::rpc::v2::{BlockScopedData, BlockUndoSignal},
};

mod rpc;
mod streams;
mod tests;

/// Blocks the reorg buffer holds below the finalized height before it commits them.
const COMMIT_BATCH_SIZE: usize = 2;

/// Write operations the gateway collects while syncing before it flushes them.
const SYNC_BATCH_SIZE: usize = 1000;

/// The extractors under test, each with its own recorded stream.
#[derive(Debug, Clone, Copy)]
pub(super) enum ExtractorKind {
    /// A native protocol extractor without extension.
    Native,
    /// A VM protocol extractor with the dynamic contract indexer, whose traces must replay too.
    Dci,
}

impl ExtractorKind {
    /// The extractor name, which is also its protocol system.
    fn name(self) -> &'static str {
        match self {
            Self::Native => "recorded_native",
            Self::Dci => "recorded_dci",
        }
    }

    /// The first block of this kind's stream that an extractor restarted before it replays
    /// differently from the original run, because of a known bug, if any. Replay tests restart
    /// below it.
    ///
    /// The DCI does not persist its trace retry counts (deferred bug 16), so a restarted
    /// extractor retries the paused pool's trace in the block where the original run had run out
    /// of retries.
    pub(super) fn replays_differ_from(self) -> Option<u64> {
        match self {
            Self::Native => None,
            Self::Dci => Some(streams::DCI_RETRIES_RUN_OUT_AT),
        }
    }

    /// The protocol type of every component in this kind's stream.
    fn protocol_type(self) -> ProtocolType {
        match self {
            Self::Native => ProtocolType::new(
                streams::NATIVE_POOL.to_string(),
                FinancialType::Swap,
                None,
                ImplementationType::Custom,
            ),
            Self::Dci => ProtocolType::new(
                streams::VM_POOL.to_string(),
                FinancialType::Swap,
                None,
                ImplementationType::Vm,
            ),
        }
    }
}

/// One recorded substreams message.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Recorded {
    /// A new block.
    Block(Box<BlockScopedData>),
    /// A revert to an earlier block, so revert messages are recorded too.
    Undo(BlockUndoSignal),
}

impl fmt::Display for Recorded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Block(data) => match data.partial_index {
                Some(index) => write!(
                    f,
                    "partial {index} of block {} ({})",
                    block_number(data),
                    block_id(data)
                ),
                None => write!(f, "block {} ({})", block_number(data), block_id(data)),
            },
            Self::Undo(signal) => match &signal.last_valid_block {
                Some(target) => write!(f, "undo to block {} ({})", target.number, target.id),
                None => write!(f, "undo without a target"),
            },
        }
    }
}

/// How a recorded stream sends its blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Delivery {
    /// One message per block.
    FullBlocks,
    /// Partial blocks, as flashblock chains send them: one partial per transaction, where every
    /// partial but the last carries a temporary block hash, and the last one is flagged and
    /// carries the block's hash. The extractor runs with partial blocks enabled.
    PartialBlocks,
}

/// A recorded stream, how it sends its blocks, and the timestamp its block timestamps count
/// from.
pub(super) struct RecordedStream {
    /// How the stream sends its blocks, which [`feed`] needs to dispatch them as the runner does.
    pub(super) delivery: Delivery,
    /// Timestamp of block 0; block `n` is `n` times [`streams::BLOCK_TIME`] seconds later, on
    /// every branch. It is midnight of the day before the stream is written, since the test
    /// database only has partitions for recent days, so block timestamps and the component
    /// creation times taken from them change from day to day.
    pub(super) base_ts: NaiveDateTime,
    /// The messages, oldest first.
    pub(super) messages: Vec<Recorded>,
}

/// The recorded stream for `kind`. It spans several commit batches, reverts on both sides of
/// its halfway point, and has blocks that are not final yet at that point.
pub(super) fn recorded_stream(kind: ExtractorKind) -> RecordedStream {
    match kind {
        ExtractorKind::Native => streams::native(Delivery::FullBlocks),
        ExtractorKind::Dci => streams::dci(),
    }
}

/// The blocks of the [`ExtractorKind::Native`] stream sent as partial blocks, as flashblock
/// chains send them. The first block arrives whole, the undo signals interrupt the last block of
/// each orphaned branch before its last partial, and one more undo signal names the block being
/// streamed.
pub(super) fn recorded_partial_block_stream() -> RecordedStream {
    streams::native(Delivery::PartialBlocks)
}

/// The blocks of `stream` that no later undo signal orphaned, oldest first: what substreams
/// replays after a restart.
///
/// # Panics
/// If an undo signal names a block that is not on the chain at that point.
pub(super) fn canonical_blocks(stream: &[Recorded]) -> Vec<BlockScopedData> {
    let mut chain: Vec<BlockScopedData> = Vec::new();
    for message in stream {
        match message {
            Recorded::Block(data) => chain.push(data.as_ref().clone()),
            Recorded::Undo(signal) => {
                let target = signal
                    .last_valid_block
                    .as_ref()
                    .unwrap_or_else(|| panic!("{message} must name its target"));
                while chain
                    .last()
                    .is_some_and(|data| block_number(data) > target.number)
                {
                    chain.pop();
                }
                assert_eq!(
                    chain.last().map(block_id),
                    Some(target.id.as_str()),
                    "{message} names a block that is not on the chain"
                );
            }
        }
    }
    chain
}

/// How far the chain finalized while a restarted extractor was down.
#[derive(Debug, Clone, Copy)]
pub(super) enum Downtime {
    /// Back before the chain finalized another block: the final height is the last one the
    /// extractor saw.
    Short,
    /// Back after the chain finalized every block the extractor saw.
    Long,
}

/// What substreams sends an extractor restarted after `last_committed`, given the `stream` it
/// saw before: the canonical blocks above that block, oldest first, without undo signals. Each
/// block carries the final height of the chain at the restart, capped at its own number: blocks
/// at or below that height arrive final.
///
/// The final height never drops below the one the extractor last saw, which is above
/// `last_committed`, since only blocks below the final height are committed.
pub(super) fn replay_after_restart(
    stream: &[Recorded],
    last_committed: u64,
    downtime: Downtime,
) -> Vec<Recorded> {
    let final_at_restart = match downtime {
        Downtime::Short => last_final_height(stream),
        Downtime::Long => u64::MAX,
    };
    let mut replay = Vec::new();
    for mut data in canonical_blocks(stream) {
        let number = block_number(&data);
        if number <= last_committed {
            continue;
        }
        data.final_block_height = data
            .final_block_height
            .max(final_at_restart)
            .min(number);
        replay.push(Recorded::Block(Box::new(data)));
    }
    replay
}

/// The final height of the last block in `stream`, or 0 without blocks.
fn last_final_height(stream: &[Recorded]) -> u64 {
    let mut final_height = 0;
    for message in stream {
        match message {
            Recorded::Block(data) => final_height = data.final_block_height,
            Recorded::Undo(_) => {}
        }
    }
    final_height
}

/// A test database that holds what every recorded stream needs before an extractor first
/// starts. Only [`seed_database`] makes one.
pub(super) struct SeededDatabase(());

/// Seeds the test database behind `pool` like an indexer database before the extractor's first
/// start: the chain, and the priced tokens with their prices, so TVL does not depend on when
/// prices were last refreshed. Call it once per test.
pub(super) async fn seed_database(pool: &Pool<AsyncPgConnection>) -> SeededDatabase {
    let mut conn = pool
        .get()
        .await
        .expect("pool should get a connection");
    let chain_id = db_fixtures::insert_chain(&mut conn, "ethereum").await;
    let mut prices = Vec::new();
    for token in streams::TOKENS {
        let Some(price) = token.price else {
            continue;
        };
        let (_, token_id) = db_fixtures::insert_token(
            &mut conn,
            chain_id,
            token.address,
            token.symbol,
            token.decimals as i32,
            Some(100),
        )
        .await;
        prices.push((token_id, price));
    }
    db_fixtures::insert_token_prices(&prices, &mut conn).await;
    SeededDatabase(())
}

/// The dynamic contract indexer of [`ExtractorKind::Dci`], with its RPC services mocked.
type RecordedDci =
    DynamicContractIndexer<MockAccountExtractor, MockEntryPointTracer, CachedGateway>;

/// An extractor of any [`ExtractorKind`]; native extractors run without the extension.
pub(super) struct RecordedExtractor(
    ProtocolExtractor<ExtractorPgGateway, rpc::TokenMetadata, RecordedDci>,
);

impl RecordedExtractor {
    /// The last block the extractor processed. Right after [`build_extractor`], that is the last
    /// block it found committed in the database.
    pub(super) async fn get_last_processed_block(&self) -> Option<Block> {
        self.0.get_last_processed_block().await
    }
}

/// Builds a fresh extractor of `kind` the way the indexer builds one when it starts: a new
/// gateway and protocol cache loaded from the database, the kind's protocol type and, for
/// [`ExtractorKind::Dci`], a dynamic contract indexer initialised from the database. Only the RPC
/// services are mocked. Once the database holds blocks this extractor committed, the new
/// extractor resumes after the last of them.
pub(super) async fn build_extractor(
    _seeded: &SeededDatabase,
    kind: ExtractorKind,
) -> RecordedExtractor {
    let database_url = std::env::var("DATABASE_URL").expect("Database URL must be set for testing");
    let (cached_gw, _writer) = GatewayBuilder::new(&database_url)
        .set_chains(&[Chain::Ethereum])
        .set_protocol_systems(&[kind.name().to_string()])
        .build()
        .await
        .expect("failed to build postgres gateway");

    let protocol_cache = ProtocolMemoryCache::new(
        Chain::Ethereum,
        chrono::Duration::seconds(900),
        Arc::new(cached_gw.clone()),
    );
    protocol_cache
        .populate()
        .await
        .expect("protocol cache should load from the database");

    let dci = match kind {
        ExtractorKind::Native => None,
        ExtractorKind::Dci => {
            let mut dci = DynamicContractIndexer::new(
                Chain::Ethereum,
                kind.name().to_string(),
                cached_gw.clone(),
                rpc::account_extractor(),
                rpc::tracer(),
            );
            dci.initialize()
                .await
                .expect("dynamic contract indexer should load from the database");
            Some(dci)
        }
    };

    let protocol_type = kind.protocol_type();
    let extractor = ProtocolExtractor::new(
        ExtractorPgGateway::new(kind.name(), Chain::Ethereum, SYNC_BATCH_SIZE, cached_gw),
        COMMIT_BATCH_SIZE,
        kind.name(),
        Chain::Ethereum,
        ChainState::default(),
        kind.name().to_string(),
        protocol_cache,
        HashMap::from([(protocol_type.name.clone(), protocol_type)]),
        rpc::TokenMetadata,
        None,
        dci,
    )
    .await
    .expect("Failed to create extractor");
    RecordedExtractor(extractor)
}

/// Feeds `stream` to `extractor` in order and returns every message it emitted, in order. Blocks
/// go through the runner's dispatch (`ExtractorRunner::process_block_data`), with partial blocks
/// enabled when `delivery` sends them. Then a whole block also yields a partial copy, and a final
/// partial also yields the full-block message merged from the block's partials. The extractor's
/// commit task finishes before the next message. A commit writes the drained blocks to the
/// database unless the extractor was syncing when it drained them: those stay in the gateway's
/// open transaction until a later commit flushes it.
///
/// # Panics
/// If the extractor fails on a message or its commit fails.
pub(super) async fn feed(
    extractor: &RecordedExtractor,
    stream: &[Recorded],
    delivery: Delivery,
) -> Vec<BlockAggregatedChanges> {
    let RecordedExtractor(extractor) = extractor;
    let partial_blocks = match delivery {
        Delivery::FullBlocks => false,
        Delivery::PartialBlocks => true,
    };
    let mut emitted = Vec::new();
    for message in stream {
        match message {
            Recorded::Block(data) => {
                let outputs = ExtractorRunner::process_block_data(extractor, data, partial_blocks)
                    .await
                    .unwrap_or_else(|err| panic!("{message} failed: {err}"));
                emitted.extend(
                    outputs
                        .into_iter()
                        .map(Arc::unwrap_or_clone),
                );
            }
            Recorded::Undo(signal) => {
                let output = extractor
                    .handle_revert(signal.clone())
                    .await
                    .unwrap_or_else(|err| panic!("{message} failed: {err}"));
                emitted.extend(output.map(Arc::unwrap_or_clone));
            }
        }
        extractor
            .wait_for_commit()
            .await
            .unwrap_or_else(|err| panic!("commit after {message} failed: {err}"));
    }
    emitted
}

/// `message` without its watermarks, which record how far the database writes and the chain's
/// finality had got when the extractor sent it rather than anything about the block: the
/// committed height depends on when the writes landed, and a block replayed after a restart can
/// arrive final where it first arrived pending.
pub(super) fn deltas_only(message: &BlockAggregatedChanges) -> BlockAggregatedChanges {
    BlockAggregatedChanges {
        db_committed_block_height: None,
        finalized_block_height: 0,
        ..message.clone()
    }
}

/// Every new token of `message` with all its metadata, by address. `Token` equality compares
/// addresses only, so comparing messages alone misses metadata changes.
pub(super) fn token_details(message: &BlockAggregatedChanges) -> BTreeMap<Address, String> {
    message
        .new_tokens
        .iter()
        .map(|(address, token)| (address.clone(), format!("{token:?}")))
        .collect()
}

/// The number of the block `data` carries.
pub(super) fn block_number(data: &BlockScopedData) -> u64 {
    data.clock
        .as_ref()
        .map(|clock| clock.number)
        .expect("recorded blocks carry a clock")
}

/// The hash of the block `data` carries, as undo signals name it.
fn block_id(data: &BlockScopedData) -> &str {
    data.clock
        .as_ref()
        .map(|clock| clock.id.as_str())
        .expect("recorded blocks carry a clock")
}
