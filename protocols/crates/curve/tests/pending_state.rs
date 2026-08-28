//! Does the pending quote match the chain?
//!
//! Needs an archive RPC endpoint and nothing else. For each block in an active window the test
//! assembles the `PendingBlock` a builder would supply — the block's logs as transactions, and the
//! block's real post-execution state diff as accounts — runs the processor against the **parent**
//! block, and checks the result against the chain **at that block**:
//!
//! 1. the pool readings the processor emits equal readings taken directly at the block;
//! 2. a quote built from those readings equals the pool's own `get_dy` at the block;
//! 3. the component balances the processor derives from logs equal `balanceOf(pool)` at the block.
//!
//! The substreams package is not involved. `tests/integration.rs` covers the one thing it alone
//! can prove — that the emitted balance bytes are encoded exactly as the package encodes them —
//! and that test needs a StreamingFast key; this one does not.
//!
//! Required env vars:
//!   ETH_RPC_URL (or RPC_URL) — archive endpoint supporting `debug_traceBlockByNumber` with the
//!                              `prestateTracer` in `diffMode`
//!   CURVE_START_BLOCK        — (optional) window start; default DEFAULT_START_BLOCK
//!   CURVE_BLOCKS             — (optional) active blocks to check; default DEFAULT_BLOCKS

use std::{collections::HashMap, fmt::Debug};

use alloy::{
    consensus::Transaction as _,
    eips::BlockNumberOrTag,
    primitives::{Address as AlloyAddress, B256, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{BlockTransactions, Filter},
    sol,
    sol_types::SolCall,
    transports::http::reqwest::Url,
};
use curve_core::processor::CurveProcessor;
use num_bigint::{BigInt, Sign};
use revm::DatabaseRef;
use serde_json::json;
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
use tycho_simulation::evm::{
    engine_db::{
        engine_db_interface::EngineDatabaseInterface,
        simulation_db::{EVMProvider, SimulationDB},
        utils::{get_client, get_runtime},
    },
    protocol::curve::{
        build_from_readings, decode_readings, read_pool_readings, CurvePoolReadings, CurveVariant,
        POOL_STATE_ADJUSTED,
    },
    simulation::{PendingOverrides, SimulationEngine},
};

/// Mainnet pools spanning the math variants: legacy StableSwap V1/V2/STETH, a 4-coin pool, the
/// TriCrypto V1 and NG shapes, and a StableSwap-NG factory pool.
const POOLS: &[&str] = &[
    "0xbebc44782c7db0a1a60cb6fe97d0b483032ff1c7", // 3pool          StableSwapV1
    "0xdc24316b9ae028f1497c275eb9192a3ea0f67022", // steth          StableSwapSTETH
    "0xd51a44d3fae010294c616388b506acda1bfaae46", // tricrypto2     TriCryptoV1
    "0xa5407eae9ba41422680e2e00537571bcc53efbfd", // susd (4 coins) probed
    "0xdcef968d416a41cdac0ed8702fac8128a64241a2", // fraxusdc       StableSwapV2
    "0x7f86bf177dd4f3494b841a37e810a34dd56c829b", // tricrypto NG   TriCryptoNG
    "0xf55b0f6f2da5ffddb104b58a60f2862745960442", // USDe/crvUSD    StableSwapNG
    "0x4dece678ceceb27446b35c672dc7d61f30bad69e", // crvUSD/USDC    StableSwapNG via `factory`
    "0xb576491f1e6e5e62f1d8f26062ee822b40b0e0d4", // CVX/ETH        TwoCrypto, probed
];

/// Verified to hold 338 blocks of activity across these pools in its first 2000 blocks.
///
/// The default block count is kept modest because each block costs a `debug_traceBlockByNumber`
/// plus a getter sweep per affected pool — roughly ten seconds. Raise `CURVE_BLOCKS` for a deeper
/// run: 30 blocks here and 20 at block 25_800_000 covered seven variants with no mismatches.
const DEFAULT_START_BLOCK: u64 = 23_000_000;
const SCAN_WINDOW: u64 = 2_000;
const DEFAULT_BLOCKS: usize = 12;

/// stETH rebases without emitting transfers, so `balanceOf` moves without a log to explain it.
/// The substreams have the same blind spot; a DCI entrypoint covers it there, not the log path.
const REBASING_TOKENS: &[&str] = &["0xae7ab96520de3a18e5e111b5eaab095312d7fe84"];

const EXTRACTOR: &str = "vm:curve";
const ETH_SENTINEL: &str = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

sol! {
    #[allow(missing_docs)]
    interface IPool {
        function coins(uint256 i) external view returns (address);
        function factory() external view returns (address);
        function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256);
    }
    #[allow(missing_docs)]
    interface IPoolOld {
        function coins(int128 i) external view returns (address);
    }
    #[allow(missing_docs)]
    interface ICryptoPool {
        function get_dy(uint256 i, uint256 j, uint256 dx) external view returns (uint256);
    }
    #[allow(missing_docs)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
    }
}

type Engine = SimulationEngine<SimulationDB<EVMProvider>>;

// ─── Engine helpers ──────────────────────────────────────────────────────────

/// An engine reading confirmed state at `number` under that block's `timestamp`.
fn engine_at(rpc_url: &str, number: u64, timestamp: u64) -> Engine {
    let mut db = SimulationDB::new(
        get_client(Some(rpc_url.to_string())).expect("failed to build RPC client"),
        get_runtime().expect("failed to get runtime"),
        None,
    );
    db.set_block(Some(BlockHeader { number, timestamp, ..Default::default() }));
    SimulationEngine::new(db, false)
}

fn call_opt<D, C, R>(engine: &SimulationEngine<D>, to: &AlloyAddress, sol_call: C) -> Option<R>
where
    D: EngineDatabaseInterface + Clone + Debug,
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
    C: SolCall<Return = R>,
{
    let params = PendingOverrides::default().view_call(*to, sol_call.abi_encode());
    let result = engine.simulate(&params).ok()?;
    C::abi_decode_returns(result.result.as_ref()).ok()
}

// ─── Pool discovery ──────────────────────────────────────────────────────────

/// The component the confirmed stream would deliver for `pool`, assembled from the chain.
///
/// Carries what the processor actually consumes: the coin list in on-chain order (as both the
/// token list and the `coins` attribute the substreams write) and the `factory` attribute when the
/// pool exposes one. Pools without a factory fall through to the legacy table or on-chain probing,
/// exactly as they do in production.
fn component_from_chain(engine: &Engine, pool: &AlloyAddress) -> Option<ProtocolComponent> {
    let mut coins: Vec<AlloyAddress> = Vec::new();
    for i in 0..8usize {
        let coin = call_opt(engine, pool, IPool::coinsCall { i: U256::from(i) })
            .or_else(|| call_opt(engine, pool, IPoolOld::coinsCall { i: i as i128 }));
        match coin {
            Some(coin) if !coin.is_zero() => coins.push(coin),
            _ => break,
        }
    }
    if coins.len() < 2 {
        return None;
    }

    let coins_json = format!(
        "[{}]",
        coins
            .iter()
            .map(|c| format!("\"{}\"", to_hex(c)))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut static_attributes =
        HashMap::from([("coins".to_string(), Bytes::from(coins_json.into_bytes()))]);
    if let Some(factory) = call_opt(engine, pool, IPool::factoryCall {}) {
        if !factory.is_zero() {
            // The substreams store addresses as the ASCII bytes of their "0x…" form.
            static_attributes
                .insert("factory".to_string(), Bytes::from(to_hex(&factory).into_bytes()));
        }
    }

    Some(ProtocolComponent {
        id: to_hex(pool),
        chain: Chain::Ethereum,
        tokens: coins
            .iter()
            .map(|coin| normalize_eth(to_hex(coin)))
            .collect(),
        static_attributes,
        ..Default::default()
    })
}

fn to_hex(address: &AlloyAddress) -> String {
    format!("0x{}", hex::encode(address.as_slice()))
}

/// Curve's ETH sentinel becomes the zero address in the emitted component tokens.
fn normalize_eth(hex_address: String) -> Bytes {
    let bytes = if hex_address == ETH_SENTINEL {
        vec![0u8; 20]
    } else {
        hex::decode(hex_address.trim_start_matches("0x")).expect("address is hex")
    };
    Bytes::from(bytes)
}

/// Blocks in `[start, start + SCAN_WINDOW]` where at least one of `POOLS` emitted a log.
async fn active_blocks(provider: &impl Provider, start: u64, wanted: usize) -> Vec<u64> {
    let addresses: Vec<AlloyAddress> = POOLS
        .iter()
        .map(|p| p.parse().expect("pool address"))
        .collect();
    let logs = provider
        .get_logs(
            &Filter::new()
                .from_block(start)
                .to_block(start + SCAN_WINDOW)
                .address(addresses),
        )
        .await
        .unwrap_or_else(|e| panic!("eth_getLogs scan failed: {e}"));

    let mut blocks: Vec<u64> = logs
        .iter()
        .filter_map(|log| log.block_number)
        .collect();
    blocks.sort_unstable();
    blocks.dedup();
    blocks.truncate(wanted);
    blocks
}

// ─── The pending block ───────────────────────────────────────────────────────

/// The block's real post-execution account state, from `debug_traceBlockByNumber`.
///
/// This is what a builder's `PostState` provides: for every account a transaction touched, the
/// storage slots and native balance it left behind. Per-transaction diffs are merged in execution
/// order so the last write to a slot wins.
async fn post_state(provider: &impl Provider, number: u64) -> HashMap<Bytes, AccountDelta> {
    let traces: serde_json::Value = provider
        .raw_request(
            "debug_traceBlockByNumber".into(),
            (
                format!("0x{number:x}"),
                json!({ "tracer": "prestateTracer", "tracerConfig": { "diffMode": true } }),
            ),
        )
        .await
        .unwrap_or_else(|e| {
            panic!(
                "debug_traceBlockByNumber({number}) failed: {e}. The endpoint must expose the \
                 debug namespace with the prestateTracer."
            )
        });

    let mut slots: HashMap<Bytes, HashMap<Bytes, Option<Bytes>>> = HashMap::new();
    let mut balances: HashMap<Bytes, Bytes> = HashMap::new();

    for trace in traces
        .as_array()
        .expect("trace result is an array")
    {
        let Some(post) = trace
            .get("result")
            .and_then(|r| r.get("post"))
            .and_then(|p| p.as_object())
        else {
            continue;
        };
        for (address, state) in post {
            let address = Bytes::from(
                hex::decode(address.trim_start_matches("0x")).expect("trace address is hex"),
            );
            if let Some(storage) = state
                .get("storage")
                .and_then(|s| s.as_object())
            {
                let entry = slots
                    .entry(address.clone())
                    .or_default();
                for (slot, value) in storage {
                    entry.insert(word(slot), Some(word(value.as_str().unwrap_or("0x0"))));
                }
            }
            if let Some(balance) = state
                .get("balance")
                .and_then(|b| b.as_str())
            {
                balances.insert(address, Bytes::from(trimmed_word(balance)));
            }
        }
    }

    let addresses: Vec<Bytes> = slots
        .keys()
        .chain(balances.keys())
        .cloned()
        .collect();
    let mut accounts = HashMap::new();
    for address in addresses {
        if accounts.contains_key(&address) {
            continue;
        }
        accounts.insert(
            address.clone(),
            AccountDelta::new(
                Chain::Ethereum,
                address.clone(),
                slots
                    .get(&address)
                    .cloned()
                    .unwrap_or_default(),
                balances.get(&address).cloned(),
                None,
                ChangeType::Update,
            ),
        );
    }
    accounts
}

/// A 32-byte big-endian word from a `0x`-prefixed hex string of any length.
fn word(hex_value: &str) -> Bytes {
    let value = U256::from_str_radix(hex_value.trim_start_matches("0x"), 16).unwrap_or(U256::ZERO);
    Bytes::from(value.to_be_bytes::<32>().to_vec())
}

/// The same value with leading zero bytes trimmed, as the indexer stores balances.
fn trimmed_word(hex_value: &str) -> Vec<u8> {
    let bytes = word(hex_value).to_vec();
    let first = bytes
        .iter()
        .position(|b| *b != 0)
        .unwrap_or(bytes.len() - 1);
    bytes[first..].to_vec()
}

/// The block's transactions with their logs, ordered by index, plus its timestamp.
async fn block_inputs(provider: &impl Provider, number: u64) -> (u64, Vec<TxInput>) {
    use alloy::network::{BlockResponse as _, TransactionResponse as _};

    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .full()
        .await
        .unwrap_or_else(|e| panic!("eth_getBlockByNumber({number}) failed: {e}"))
        .unwrap_or_else(|| panic!("block {number} not found"));
    let timestamp = block.header().timestamp;

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

    // A transaction that emitted logs did not revert.
    let mut txs: Vec<TxInput> = by_tx
        .into_iter()
        .filter_map(|(hash, logs)| {
            let (from, to, index) = meta.get(&hash)?.clone();
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

/// The confirmed block that seeds the processor for a comparison at `number`: the components, and
/// each pool's real token balances at the parent block.
fn seed_block(
    engine: &Engine,
    components: &HashMap<String, ProtocolComponent>,
    number: u64,
) -> BlockAggregatedChanges {
    let mut component_balances: HashMap<String, HashMap<Bytes, ComponentBalance>> = HashMap::new();
    for (id, component) in components {
        let pool: AlloyAddress = id
            .parse()
            .expect("component id is an address");
        let mut balances = HashMap::new();
        for token in &component.tokens {
            if token.iter().all(|b| *b == 0) {
                continue; // native ETH; the processor reads it from the account
            }
            let token_address = AlloyAddress::from_slice(token.as_ref());
            let Some(balance) =
                call_opt(engine, &token_address, IERC20::balanceOfCall { account: pool })
            else {
                continue;
            };
            balances.insert(
                token.clone(),
                ComponentBalance {
                    token: token.clone(),
                    balance: Bytes::from(u256_to_signed_be(balance)),
                    balance_float: 0.0,
                    modify_tx: Bytes::default(),
                    component_id: id.clone(),
                },
            );
        }
        component_balances.insert(id.clone(), balances);
    }

    BlockAggregatedChanges {
        extractor: EXTRACTOR.to_string(),
        chain: Chain::Ethereum,
        block: Block {
            number,
            chain: Chain::Ethereum,
            hash: Bytes::default(),
            parent_hash: Bytes::default(),
            ts: Default::default(),
        },
        finalized_block_height: number,
        new_protocol_components: components.clone(),
        component_balances,
        ..Default::default()
    }
}

fn u256_to_signed_be(value: U256) -> Vec<u8> {
    BigInt::from_bytes_be(Sign::Plus, &value.to_be_bytes::<32>()).to_signed_bytes_be()
}

// ─── Test ────────────────────────────────────────────────────────────────────

/// Synchronous on purpose. `SimulationDB` owns its own Tokio runtime and blocks on it for every
/// storage read, which panics inside an outer runtime, so the async RPC fetches run on a separate
/// runtime and never overlap with engine work.
#[test]
#[ignore = "requires ETH_RPC_URL: an archive endpoint with the debug namespace"]
fn test_pending_state_matches_the_chain() {
    dotenv::dotenv().ok();
    let rpc_runtime = tokio::runtime::Runtime::new().expect("failed to build test runtime");
    let rpc_url = std::env::var("ETH_RPC_URL")
        .or_else(|_| std::env::var("RPC_URL"))
        .expect("ETH_RPC_URL or RPC_URL must be set");
    let start_block = env_u64("CURVE_START_BLOCK").unwrap_or(DEFAULT_START_BLOCK);
    let wanted_blocks = env_u64("CURVE_BLOCKS").unwrap_or(DEFAULT_BLOCKS as u64) as usize;

    let provider = ProviderBuilder::new().connect_http(
        rpc_url
            .parse::<Url>()
            .expect("invalid RPC URL"),
    );

    // Pool definitions read once, from a block inside the window.
    let discovery_engine = engine_at(&rpc_url, start_block, 0);
    let mut components: HashMap<String, ProtocolComponent> = HashMap::new();
    for pool in POOLS {
        let address: AlloyAddress = pool.parse().expect("pool address");
        match component_from_chain(&discovery_engine, &address) {
            Some(component) => {
                components.insert(component.id.clone(), component);
            }
            None => println!("  skipping {pool}: could not read its coins"),
        }
    }
    assert!(!components.is_empty(), "no pool definitions could be read from the chain");
    println!("  pools discovered: {}", components.len());

    let blocks = rpc_runtime.block_on(active_blocks(&provider, start_block, wanted_blocks));
    assert!(
        !blocks.is_empty(),
        "no activity for these pools in [{start_block}, {}]. Move CURVE_START_BLOCK.",
        start_block + SCAN_WINDOW
    );

    let mut state_mismatches: Vec<String> = Vec::new();
    let mut quote_mismatches: Vec<String> = Vec::new();
    let mut balance_mismatches: Vec<String> = Vec::new();
    let mut compared_states = 0usize;
    let mut compared_quotes = 0usize;
    let mut compared_balances = 0usize;
    let mut variants: HashMap<CurveVariant, usize> = HashMap::new();

    for number in &blocks {
        let (timestamp, txs) = rpc_runtime.block_on(block_inputs(&provider, *number));
        let accounts = rpc_runtime.block_on(post_state(&provider, *number));
        let parent_engine = engine_at(&rpc_url, number - 1, timestamp);
        let chain_engine = engine_at(&rpc_url, *number, timestamp);

        // Seed from the parent block, then let the pending block's overrides do the rest.
        let mut processor = CurveProcessor::with_engine(
            Chain::Ethereum,
            EXTRACTOR.to_string(),
            engine_at(&rpc_url, number - 1, timestamp),
        );
        processor
            .apply_block(&seed_block(&parent_engine, &components, number - 1))
            .expect("apply_block failed");

        let pending = PendingBlock::new(
            Block {
                number: *number,
                chain: Chain::Ethereum,
                hash: Bytes::default(),
                parent_hash: Bytes::default(),
                ts: chrono::DateTime::from_timestamp(timestamp as i64, 0)
                    .expect("block timestamp fits")
                    .naive_utc(),
            },
            txs,
            accounts,
        );
        if *number == blocks[0] {
            println!("  pools tracked: {} of {}", processor.tracked_pools(), components.len());
            for id in components.keys() {
                if processor.pool_variant(id).is_none() {
                    // Registration failed, so the pool is invisible to the comparison. In
                    // production it would be dropped the same way, which is worth seeing.
                    println!("    unresolved variant, pool not tracked: {id}");
                }
            }
        }

        let generated = processor.generate_deltas(&pending);

        for (component_id, delta) in &generated.state_deltas {
            let encoded = delta
                .updated_attributes
                .get(POOL_STATE_ADJUSTED)
                .expect("a state delta must carry the readings");
            let ours: CurvePoolReadings =
                decode_readings(encoded).expect("emitted readings must decode");
            let pool: AlloyAddress = component_id
                .parse()
                .expect("component id is an address");
            let variant = processor
                .pool_variant(component_id)
                .expect("a pool with a delta is tracked");
            *variants.entry(variant).or_default() += 1;

            // 1. Readings from parent + overrides vs readings taken at the block itself.
            let theirs = match read_pool_readings(
                &chain_engine,
                &pool,
                variant,
                ours.balances.len(),
                &PendingOverrides::default(),
            ) {
                Ok(theirs) => theirs,
                Err(error) => {
                    state_mismatches.push(format!(
                        "block={number} pool={component_id}: chain read failed: {error}"
                    ));
                    continue;
                }
            };
            compared_states += 1;
            if ours != theirs {
                state_mismatches.push(format!(
                    "block={number} pool={component_id} variant={variant:?}\n    \
                     pending={ours:?}\n    chain  ={theirs:?}"
                ));
                continue;
            }

            // 2. A quote built from those readings vs the pool's own get_dy at the block.
            if let Some((ours_out, theirs_out, dx)) =
                quote_pair(&chain_engine, &pool, variant, &ours, &components[component_id])
            {
                compared_quotes += 1;
                if ours_out != theirs_out {
                    quote_mismatches.push(format!(
                        "block={number} pool={component_id} dx={dx}: ours={ours_out} \
                         get_dy={theirs_out}"
                    ));
                }
            }
        }

        // 3. Component balances vs balanceOf / eth_getBalance at the block.
        for (component_id, tokens) in &generated.component_balances {
            let pool: AlloyAddress = component_id
                .parse()
                .expect("component id is an address");
            for (token, balance) in tokens {
                let ours = BigInt::from_signed_bytes_be(balance.balance.as_ref());
                let token_hex = format!("0x{}", hex::encode(token.as_ref()));
                if REBASING_TOKENS.contains(&token_hex.as_str()) {
                    continue;
                }
                let theirs = if token.iter().all(|b| *b == 0) {
                    // Native ETH: read through the engine's database so the whole comparison
                    // phase stays free of provider calls.
                    match chain_engine.state.basic_ref(pool) {
                        Ok(Some(account)) => account.balance,
                        Ok(None) => U256::ZERO,
                        Err(error) => {
                            balance_mismatches.push(format!(
                                "block={number} pool={component_id}: native balance read failed: \
                                 {error:?}"
                            ));
                            continue;
                        }
                    }
                } else {
                    let token_address = AlloyAddress::from_slice(token.as_ref());
                    match call_opt(
                        &chain_engine,
                        &token_address,
                        IERC20::balanceOfCall { account: pool },
                    ) {
                        Some(balance) => balance,
                        None => continue,
                    }
                };
                compared_balances += 1;
                let theirs = BigInt::from_bytes_be(Sign::Plus, &theirs.to_be_bytes::<32>());
                if ours != theirs {
                    balance_mismatches.push(format!(
                        "block={number} pool={component_id} token={token_hex}: \
                         processor={ours} chain={theirs}"
                    ));
                }
            }
        }
    }

    println!("\n─── Summary ────────────────────────────────────────────────────────────────");
    println!("  Blocks checked:        {}", blocks.len());
    println!("  Pool states compared:  {compared_states}");
    println!("  Quotes compared:       {compared_quotes}");
    println!("  Balances compared:     {compared_balances}");
    println!("  Variants covered:      {variants:?}");
    println!("  State mismatches:      {}", state_mismatches.len());
    println!("  Quote mismatches:      {}", quote_mismatches.len());
    println!("  Balance mismatches:    {}", balance_mismatches.len());
    println!("────────────────────────────────────────────────────────────────────────────");

    assert!(
        state_mismatches.is_empty(),
        "Pool state mismatches ({}):\n{}",
        state_mismatches.len(),
        state_mismatches.join("\n")
    );
    assert!(
        quote_mismatches.is_empty(),
        "Quote mismatches ({}):\n{}",
        quote_mismatches.len(),
        quote_mismatches.join("\n")
    );
    assert!(
        balance_mismatches.is_empty(),
        "Balance mismatches ({}):\n{}",
        balance_mismatches.len(),
        balance_mismatches.join("\n")
    );
    assert!(compared_states > 0, "no pool state was compared; move the window");
    assert!(compared_balances > 0, "no balance was compared; move the window");
}

/// Our quote and the pool's own `get_dy` for a small trade between coins 0 and 1.
///
/// `None` when the pool exposes neither `get_dy` signature or the swap is not quotable, which is
/// not a failure — assertions 1 and 3 still cover the pool.
fn quote_pair(
    engine: &Engine,
    pool: &AlloyAddress,
    variant: CurveVariant,
    readings: &CurvePoolReadings,
    component: &ProtocolComponent,
) -> Option<(U256, U256, U256)> {
    let decimals = coin_decimals(engine, component);
    let built = build_from_readings(readings, variant, &decimals).ok()?;
    // A thousandth of the pool's coin-0 balance: large enough to be meaningful, small enough to
    // stay inside every variant's solvable range.
    let dx = readings
        .balances
        .first()?
        .checked_div(U256::from(1000))?;
    if dx.is_zero() {
        return None;
    }
    let ours = built.get_amount_out(0, 1, dx)?;
    let theirs = call_opt(engine, pool, IPool::get_dyCall { i: 0, j: 1, dx }).or_else(|| {
        call_opt(engine, pool, ICryptoPool::get_dyCall { i: U256::ZERO, j: U256::from(1), dx })
    })?;
    Some((ours, theirs, dx))
}

/// Coin decimals in pool index order, read from the coins themselves. Native ETH is 18.
fn coin_decimals(engine: &Engine, component: &ProtocolComponent) -> Vec<u8> {
    sol! {
        #[allow(missing_docs)]
        interface IDecimals {
            function decimals() external view returns (uint8);
        }
    }
    component
        .tokens
        .iter()
        .map(|token| {
            if token.iter().all(|b| *b == 0) {
                return 18;
            }
            let address = AlloyAddress::from_slice(token.as_ref());
            call_opt(engine, &address, IDecimals::decimalsCall {}).unwrap_or(18)
        })
        .collect()
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
}
