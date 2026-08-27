//! Parity of the native Curve processor against the substreams package built from origin/main.
//!
//! Curve is a VM protocol, so the two halves of its output are checked against different ground
//! truths:
//!
//! * **Component balances** are derived from transfer logs on both sides, so they are compared byte
//!   for byte against the substreams' `map_protocol_changes` output.
//! * **Pool state** has no substreams counterpart — the substreams emit raw contract storage, the
//!   processor emits decoded view-getter readings. It is instead compared against the chain:
//!   readings produced from the parent block plus the pending block's overrides must equal readings
//!   taken directly at the pending block.
//!
//! Required env vars:
//!   ETH_RPC_URL         — archive RPC endpoint (eth_getLogs, eth_getBlockByNumber,
//!                         eth_getStorageAt at historical blocks)
//!   STREAMINGFAST_KEY   — StreamingFast JWT (exchange an api key via
//!                         https://auth.streamingfast.io/v1/auth/issue)
//!   CURVE_SPKG_PATH     — (optional) spkg override; defaults to the build_main_spkg.sh output
//!   CURVE_START_BLOCK   — (optional) window start; defaults to DEFAULT_START_BLOCK
//!   CURVE_STOP_BLOCK    — (optional) window end; defaults to start + DEFAULT_WINDOW
//!   CURVE_SEED_BLOCKS   — (optional) leading blocks used only to register pools

use std::{collections::HashMap, sync::Arc};

use alloy::{
    consensus::Transaction as _,
    eips::BlockNumberOrTag,
    primitives::{Address as AlloyAddress, B256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{BlockTransactions, Filter},
    transports::http::reqwest::Url,
};
use curve_core::processor::CurveProcessor;
use num_bigint::BigInt;
use num_traits::ToPrimitive as _;
use prost::Message;
use tokio_stream::StreamExt;
use tycho_client::feed::BlockHeader;
use tycho_common::{
    models::{
        blockchain::{Block, BlockAggregatedChanges, LogInput, PendingBlock, TxInput},
        contract::AccountDelta,
        protocol::{ComponentBalance, ProtocolComponent},
        Chain, ChangeType,
    },
    traits::TxDeltaIndexer,
    Bytes,
};
use tycho_indexer::{
    pb::sf::substreams::v1::Package,
    substreams::{
        stream::{BlockResponse, SubstreamsStream},
        SubstreamsEndpoint,
    },
};
use tycho_protobuf::pb::tycho::evm::v1 as pb;
use tycho_simulation::evm::{
    engine_db::{
        simulation_db::SimulationDB,
        utils::{get_client, get_runtime},
    },
    protocol::curve::{decode_readings, read_pool_readings, POOL_STATE_ADJUSTED},
    simulation::{PendingOverrides, SimulationEngine},
};

/// Curve deploys pools far less often than UniswapV2, and only pools registered inside the window
/// can be compared, so the default window is wide enough to hold both creations and the activity
/// that follows them. The test fails rather than passes vacuously if coverage comes out empty, so
/// an unsuitable window is reported instead of hidden.
const DEFAULT_START_BLOCK: u64 = 19_500_000;
const DEFAULT_WINDOW: u64 = 20_000;
/// Leading blocks used only to register pools and seed their balances.
const DEFAULT_SEED_BLOCKS: u64 = 4_000;

const EXTRACTOR: &str = "vm:curve";
const ENDPOINT_URL: &str = "https://mainnet.eth.streamingfast.io:443";
/// The final map module of the Curve package, per `ethereum-curve.yaml`.
const MAP_MODULE: &str = "map_protocol_changes";

type SubstreamsBlock = (u64, String, pb::BlockChanges);

// ─── Substreams ground truth ─────────────────────────────────────────────────

/// Streams every block in `[start, stop]` and returns `(number, hash, changes)` in order.
async fn stream_block_range(
    api_key: &str,
    spkg_path: &str,
    start: u64,
    stop: u64,
) -> Vec<SubstreamsBlock> {
    let content = std::fs::read(spkg_path)
        .unwrap_or_else(|e| panic!("Failed to read spkg at {spkg_path}: {e}"));
    let package = Package::decode(content.as_slice())
        .unwrap_or_else(|e| panic!("Failed to decode spkg: {e}"));

    let endpoint = Arc::new(
        SubstreamsEndpoint::new(ENDPOINT_URL, Some(api_key.to_string()))
            .await
            .expect("Failed to create substreams endpoint"),
    );

    let mut stream = SubstreamsStream::new(
        endpoint,
        None,
        Some(package),
        MAP_MODULE.to_string(),
        start as i64,
        stop,
        true,
        "integration-test".to_string(),
        false,
    );

    let mut results = Vec::new();
    loop {
        match stream.next().await {
            Some(Ok(BlockResponse::New(data))) => {
                let (number, hash) = data
                    .clock
                    .as_ref()
                    .map_or((0, String::new()), |c| (c.number, c.id.clone()));
                let map_output = data
                    .output
                    .as_ref()
                    .and_then(|o| o.map_output.as_ref())
                    .expect("BlockScopedData has no map_output");
                let changes = pb::BlockChanges::decode(map_output.value.as_slice())
                    .expect("Failed to decode BlockChanges");
                results.push((number, hash, changes));
            }
            Some(Ok(BlockResponse::Ended)) => break,
            Some(Ok(BlockResponse::Undo(_))) => {
                panic!("Unexpected undo signal in range [{start}, {stop}]")
            }
            Some(Err(e)) => panic!("Substreams stream error: {e}"),
            None => break,
        }
    }

    results.sort_unstable_by_key(|(n, _, _)| *n);
    results
}

/// The per-transaction changes of a block, ordered by transaction index.
///
/// The Curve substreams already sort by index, but the aggregation below is last-write-wins, so
/// the order is asserted here rather than assumed.
fn ordered_changes(changes: &pb::BlockChanges) -> Vec<&pb::TransactionChanges> {
    let mut ordered: Vec<_> = changes.changes.iter().collect();
    ordered.sort_by_key(|tc| {
        tc.tx
            .as_ref()
            .map(|t| t.index)
            .unwrap_or_default()
    });
    ordered
}

/// The block's contract changes as post-execution account deltas.
///
/// This is what a block builder's `PostState` provides in production. Slots are applied in
/// transaction-index order so the last write for each slot wins, which is the block-final value.
fn accounts_from_contract_changes(changes: &pb::BlockChanges) -> HashMap<Bytes, AccountDelta> {
    let mut slots: HashMap<Bytes, HashMap<Bytes, Option<Bytes>>> = HashMap::new();
    let mut balances: HashMap<Bytes, Option<Bytes>> = HashMap::new();

    for tx_changes in ordered_changes(changes) {
        for contract in &tx_changes.contract_changes {
            let address = Bytes::from(contract.address.clone());
            let entry = slots
                .entry(address.clone())
                .or_default();
            for slot in &contract.slots {
                entry.insert(Bytes::from(slot.slot.clone()), Some(Bytes::from(slot.value.clone())));
            }
            if !contract.balance.is_empty() {
                balances.insert(address, Some(Bytes::from(contract.balance.clone())));
            }
        }
    }

    slots
        .into_iter()
        .map(|(address, slots)| {
            let balance = balances
                .get(&address)
                .cloned()
                .unwrap_or(None);
            (
                address.clone(),
                AccountDelta::new(
                    Chain::Ethereum,
                    address,
                    slots,
                    balance,
                    None,
                    ChangeType::Update,
                ),
            )
        })
        .collect()
}

/// Converts a substreams block into the confirmed-stream shape `apply_block` consumes.
fn substreams_to_model(
    changes: &pb::BlockChanges,
    number: u64,
    hash_hex: &str,
    timestamp: i64,
) -> BlockAggregatedChanges {
    let block = Block {
        number,
        chain: Chain::Ethereum,
        hash: Bytes::from(hex::decode(hash_hex.trim_start_matches("0x")).unwrap_or_default()),
        parent_hash: Bytes::default(),
        ts: chrono::DateTime::from_timestamp(timestamp, 0)
            .unwrap_or_default()
            .naive_utc(),
    };

    let mut new_protocol_components: HashMap<String, ProtocolComponent> = HashMap::new();
    let mut component_balances: HashMap<String, HashMap<Bytes, ComponentBalance>> = HashMap::new();

    for tx_changes in ordered_changes(changes) {
        for component in &tx_changes.component_changes {
            new_protocol_components.insert(
                component.id.clone(),
                ProtocolComponent {
                    id: component.id.clone(),
                    chain: Chain::Ethereum,
                    tokens: component
                        .tokens
                        .iter()
                        .map(|t| Bytes::from(t.clone()))
                        .collect(),
                    contract_addresses: component
                        .contracts
                        .iter()
                        .map(|c| Bytes::from(c.clone()))
                        .collect(),
                    static_attributes: component
                        .static_att
                        .iter()
                        .map(|a| (a.name.clone(), Bytes::from(a.value.clone())))
                        .collect(),
                    ..Default::default()
                },
            );
        }

        for balance in &tx_changes.balance_changes {
            let component_id = String::from_utf8(balance.component_id.clone()).unwrap_or_default();
            let token = Bytes::from(balance.token.clone());
            let value = Bytes::from(balance.balance.clone());
            let balance_float = BigInt::from_signed_bytes_be(value.as_ref())
                .to_f64()
                .unwrap_or(f64::MAX);
            component_balances
                .entry(component_id.clone())
                .or_default()
                .insert(
                    token.clone(),
                    ComponentBalance {
                        token,
                        balance: value,
                        balance_float,
                        modify_tx: Bytes::default(),
                        component_id,
                    },
                );
        }
    }

    BlockAggregatedChanges {
        extractor: EXTRACTOR.to_string(),
        chain: Chain::Ethereum,
        block,
        finalized_block_height: number,
        new_protocol_components,
        component_balances,
        ..Default::default()
    }
}

/// The block-final balance the substreams report per `(component id, token hex)`.
///
/// Native ETH (the zero address) is excluded: the substreams derive it from per-call balance
/// changes in the block trace, which a `PendingBlock`'s logs do not carry, so the processor takes
/// the account's own balance instead. That value is checked against the chain, not against this.
fn substreams_balances(
    changes: &pb::BlockChanges,
    known_pools: &HashMap<String, ()>,
) -> HashMap<(String, String), Vec<u8>> {
    let mut balances = HashMap::new();
    for tx_changes in ordered_changes(changes) {
        for balance in &tx_changes.balance_changes {
            let component_id = String::from_utf8(balance.component_id.clone()).unwrap_or_default();
            if !known_pools.contains_key(&component_id) {
                continue;
            }
            if balance.token.iter().all(|b| *b == 0) {
                continue;
            }
            balances.insert((component_id, hex::encode(&balance.token)), balance.balance.clone());
        }
    }
    balances
}

// ─── Archive RPC ─────────────────────────────────────────────────────────────

/// A block's timestamp and its transactions as `TxInput`s, ordered by index.
///
/// Every log in the block is included: the processor only credits transfers whose transactor is a
/// tracked pool, which is the same filter the substreams apply.
async fn fetch_block_inputs(rpc_url: &str, number: u64) -> (i64, Vec<TxInput>) {
    use alloy::network::{BlockResponse as _, TransactionResponse as _};

    let provider = ProviderBuilder::new().connect_http(
        rpc_url
            .parse::<Url>()
            .expect("invalid RPC URL"),
    );
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .full()
        .await
        .unwrap_or_else(|e| panic!("eth_getBlockByNumber({number}) failed: {e}"))
        .unwrap_or_else(|| panic!("block {number} not found"));
    let timestamp = block.header().timestamp as i64;

    let mut meta: HashMap<B256, (Vec<u8>, Vec<u8>, u64)> = HashMap::new();
    if let BlockTransactions::Full(txns) = block.transactions() {
        for tx in txns {
            meta.insert(
                tx.tx_hash(),
                (
                    tx.from().to_vec(),
                    tx.inner
                        .to()
                        .map(|a| a.to_vec())
                        .unwrap_or_default(),
                    tx.transaction_index()
                        .unwrap_or_default(),
                ),
            );
        }
    }

    let logs = provider
        .get_logs(
            &Filter::new()
                .from_block(number)
                .to_block(number),
        )
        .await
        .unwrap_or_else(|e| panic!("eth_getLogs({number}) failed: {e}"));

    let mut by_tx: HashMap<B256, Vec<LogInput>> = HashMap::new();
    for log in &logs {
        let Some(hash) = log.transaction_hash else { continue };
        by_tx
            .entry(hash)
            .or_default()
            .push(LogInput::new(
                Bytes::from(log.address().to_vec()),
                log.topics()
                    .iter()
                    .map(|t| Bytes::from(t.to_vec()))
                    .collect(),
                Bytes::from(log.data().data.to_vec()),
                log.log_index.unwrap_or_default() as u32,
            ));
    }

    let mut txs: Vec<TxInput> = by_tx
        .into_iter()
        .filter_map(|(hash, logs)| {
            let (from, to, index) = meta.get(&hash)?.clone();
            // A transaction that emitted logs did not revert.
            Some(TxInput::new(
                Bytes::from(hash.to_vec()),
                Bytes::from(from),
                Bytes::from(to),
                index,
                logs,
                true,
            ))
        })
        .collect();
    txs.sort_unstable_by_key(|tx| tx.index());
    (timestamp, txs)
}

/// An engine reading confirmed state at `number`, with `timestamp` as its block environment.
fn engine_at(
    rpc_url: &str,
    number: u64,
    timestamp: u64,
) -> SimulationEngine<SimulationDB<tycho_simulation::evm::engine_db::simulation_db::EVMProvider>> {
    let mut db = SimulationDB::new(
        get_client(Some(rpc_url.to_string())).expect("failed to build RPC client"),
        get_runtime().expect("failed to get runtime"),
        None,
    );
    db.set_block(Some(BlockHeader { number, timestamp, ..Default::default() }));
    SimulationEngine::new(db, false)
}

// ─── Test ────────────────────────────────────────────────────────────────────

/// Runs the processor over a block window and checks both halves of its output.
///
/// Per block, in the order production would see them:
///   1. Build the `PendingBlock`: the block's logs as transactions, and the substreams' contract
///      changes as the post-execution accounts a builder would supply.
///   2. `generate_deltas` against an engine pinned at the parent block.
///   3. Compare the emitted balances against the substreams, restricted to pools already
///      registered, and the emitted pool readings against readings taken at the block itself.
///   4. `apply_block` with the substreams ground truth, registering new pools for later blocks.
///
/// The ground-truth spkg is built from origin/main, not the working tree:
///   protocols/crates/curve/scripts/build_main_spkg.sh
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires ETH_RPC_URL, STREAMINGFAST_KEY, and a spkg built via build_main_spkg.sh"]
async fn test_processor_matches_substreams_and_chain() {
    dotenv::dotenv().ok();

    let rpc_url = std::env::var("ETH_RPC_URL").expect("ETH_RPC_URL must be set");
    let api_key = std::env::var("STREAMINGFAST_KEY").expect("STREAMINGFAST_KEY must be set");
    let spkg_path = std::env::var("CURVE_SPKG_PATH").unwrap_or_else(|_| {
        format!("{}/../../../target/spkg/ethereum-curve-main.spkg", env!("CARGO_MANIFEST_DIR"))
    });
    assert!(
        std::path::Path::new(&spkg_path).exists(),
        "spkg not found at {spkg_path}. Build it from origin/main with \
         protocols/crates/curve/scripts/build_main_spkg.sh, or point CURVE_SPKG_PATH at an \
         existing .spkg file."
    );
    let start_block = env_u64("CURVE_START_BLOCK").unwrap_or(DEFAULT_START_BLOCK);
    let stop_block = env_u64("CURVE_STOP_BLOCK").unwrap_or(start_block + DEFAULT_WINDOW);
    let seed_until = start_block + env_u64("CURVE_SEED_BLOCKS").unwrap_or(DEFAULT_SEED_BLOCKS);

    let blocks = stream_block_range(&api_key, &spkg_path, start_block, stop_block).await;
    assert!(!blocks.is_empty(), "Substreams returned no blocks for [{start_block}, {stop_block}]");

    let mut processor = CurveProcessor::with_engine(
        Chain::Ethereum,
        EXTRACTOR.to_string(),
        engine_at(&rpc_url, start_block, 0),
    );
    let mut known_pools: HashMap<String, ()> = HashMap::new();

    let mut balance_mismatches: Vec<String> = Vec::new();
    let mut state_mismatches: Vec<String> = Vec::new();
    let mut compared_balances = 0usize;
    let mut compared_pools = 0usize;
    let mut read_failures = 0usize;

    for (number, hash, changes) in &blocks {
        if changes.changes.is_empty() {
            continue;
        }
        let (timestamp, txs) = fetch_block_inputs(&rpc_url, *number).await;

        if *number >= seed_until && !known_pools.is_empty() {
            let accounts = accounts_from_contract_changes(changes);
            let pending = PendingBlock::new(
                Block {
                    number: *number,
                    chain: Chain::Ethereum,
                    hash: Bytes::from(
                        hex::decode(hash.trim_start_matches("0x")).unwrap_or_default(),
                    ),
                    parent_hash: Bytes::default(),
                    ts: chrono::DateTime::from_timestamp(timestamp, 0)
                        .unwrap_or_default()
                        .naive_utc(),
                },
                txs,
                accounts,
            );

            // The processor must read the parent block's state and reach the pending block only
            // through the overrides.
            processor
                .engine_mut()
                .state
                .set_block(Some(BlockHeader {
                    number: number - 1,
                    timestamp: timestamp as u64,
                    ..Default::default()
                }));
            let generated = processor.generate_deltas(&pending);

            // ── Balances: byte-for-byte against the substreams ───────────────
            let expected = substreams_balances(changes, &known_pools);
            let actual: HashMap<(String, String), Vec<u8>> = generated
                .component_balances
                .iter()
                .flat_map(|(id, tokens)| {
                    tokens
                        .iter()
                        .map(move |(token, balance)| {
                            ((id.clone(), hex::encode(token.as_ref())), balance.balance.to_vec())
                        })
                })
                .collect();
            for (key, want) in &expected {
                compared_balances += 1;
                match actual.get(key) {
                    Some(got) if got == want => {}
                    Some(got) => balance_mismatches.push(format!(
                        "block={number} pool={} token={}: substreams={} processor={}",
                        key.0,
                        key.1,
                        hex::encode(want),
                        hex::encode(got)
                    )),
                    None => balance_mismatches.push(format!(
                        "block={number} pool={} token={}: substreams={} processor=MISSING",
                        key.0,
                        key.1,
                        hex::encode(want)
                    )),
                }
            }

            // ── Pool state: against the chain at this very block ─────────────
            let chain_engine = engine_at(&rpc_url, *number, timestamp as u64);
            for (component_id, delta) in &generated.state_deltas {
                let Some(encoded) = delta
                    .updated_attributes
                    .get(POOL_STATE_ADJUSTED)
                else {
                    state_mismatches
                        .push(format!("block={number} pool={component_id}: no state attribute"));
                    continue;
                };
                let ours =
                    decode_readings(encoded).expect("processor emitted undecodable readings");

                let address = AlloyAddress::from_slice(
                    &hex::decode(component_id.trim_start_matches("0x")).expect("pool id is hex"),
                );
                // The same variant and coin count the processor used, so the chain read issues
                // the identical getter set.
                let variant = processor
                    .pool_variant(component_id)
                    .expect("a pool with a state delta is tracked");
                let theirs = match read_pool_readings(
                    &chain_engine,
                    &address,
                    variant,
                    ours.balances.len(),
                    &PendingOverrides::default(),
                ) {
                    Ok(theirs) => theirs,
                    Err(error) => {
                        read_failures += 1;
                        eprintln!(
                            "  block={number} pool={component_id}: chain read failed: {error}"
                        );
                        continue;
                    }
                };

                compared_pools += 1;
                if ours != theirs {
                    state_mismatches.push(format!(
                        "block={number} pool={component_id}:\n    pending={ours:?}\n    chain={theirs:?}"
                    ));
                }
            }
        }

        let model = substreams_to_model(changes, *number, hash, timestamp);
        processor
            .apply_block(&model)
            .expect("apply_block failed");
        for id in model.new_protocol_components.keys() {
            known_pools.insert(id.clone(), ());
        }
    }

    println!("\n─── Summary ────────────────────────────────────────────────────────────────");
    println!("  Window:                [{start_block}, {stop_block}]");
    println!(
        "  Blocks with changes:   {}",
        blocks
            .iter()
            .filter(|(_, _, c)| !c.changes.is_empty())
            .count()
    );
    println!("  Pools registered:      {}", known_pools.len());
    println!("  Pools tracked:         {}", processor.tracked_pools());
    println!("  Balances compared:     {compared_balances}");
    println!("  Pool states compared:  {compared_pools}");
    println!("  Chain read failures:   {read_failures}");
    println!("  Balance mismatches:    {}", balance_mismatches.len());
    println!("  State mismatches:      {}", state_mismatches.len());
    println!("────────────────────────────────────────────────────────────────────────────");

    assert!(
        balance_mismatches.is_empty(),
        "Balance mismatches ({}):\n{}",
        balance_mismatches.len(),
        balance_mismatches.join("\n")
    );
    assert!(
        state_mismatches.is_empty(),
        "Pool state mismatches ({}):\n{}",
        state_mismatches.len(),
        state_mismatches.join("\n")
    );
    // A window with no registered pool that later traded proves nothing. Fail loudly rather
    // than report a green run with zero coverage.
    assert!(
        compared_balances > 0,
        "No balance comparison happened in [{start_block}, {stop_block}]. Move the window to a \
         period where Curve pools are created and then traded, via CURVE_START_BLOCK / \
         CURVE_STOP_BLOCK."
    );
    assert!(
        compared_pools > 0,
        "No pool state comparison happened in [{start_block}, {stop_block}]. Move the window."
    );
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
}
