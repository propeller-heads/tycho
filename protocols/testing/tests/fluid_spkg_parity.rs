//! Does the fluid substreams feed keep the resolver quotable?
//!
//! In production `SHARED_TYCHO_DB` is fed by the fluid substreams' contract changes, and
//! `FluidV1` quotes from a resolver call against that database. This test substitutes the spkg
//! for the chain as the source of incremental updates: it seeds the database from a
//! `debug_traceCall` prestate at the window's first block, then applies each streamed block's
//! contract changes and compares the resolver's answer against an `eth_call` at that block.
//! A slot the spkg fails to emit leaves a stale value behind, which shows up as drift in the
//! resolver's answer. So no guess about which slots matter is needed.
//!
//! `PreCachedDB` has no RPC fallback, so nothing the spkg omits can be silently read from the
//! chain: a missing account errors, and a missing slot on a known account keeps its last value.
//!
//! Scope: the spkg tracks the liquidity layer, the reserves resolver and the dex contracts. The
//! rest of the resolver's read set — the token contracts it reaches through `balanceOf`, Zircuit
//! staking on mainnet — is discovered by tycho-indexer's DCI, not by the spkg. Those accounts are
//! therefore refreshed from the chain at every block, as the DCI keeps them current in
//! production. Without that they freeze at their seed values and every pool drifts for reasons
//! the spkg never claimed to cover; with it, a mismatch can only come from state the spkg owns.
//!
//! Both tests need credentials and take minutes. From the repo
//! root, with `ETH_RPC_URL` and `STREAMINGFAST_KEY` in `.env`:
//!
//! ```text
//! LIBRARY_PATH=/opt/homebrew/opt/libpq/lib \
//!   cargo test -p protocol-testing --test fluid_spkg_parity -- --ignored --nocapture
//! ```
//!
//! `LIBRARY_PATH` points the linker at keg-only libpq, which the `protocol-testing` binary
//! needs. The result on ethereum-fluid v0.3.2, 2026-08-26:
//!
//! ```text
//! ─── Coverage ───────────────────────────────────────────────────────────────
//!   Window:                [25302880, 25303210] (331 blocks)
//!   Accounts seeded:       137
//!   Contract changes:      197
//!   DCI accounts kept:     28969
//!   Pools per block:       48
//!   Resolver comparisons:  15888
//!   Balance emissions at:  {25302900, 25302931, 25303200}
//!   Mismatches:            0
//! ────────────────────────────────────────────────────────────────────────────
//! ```

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::PathBuf,
    str::FromStr as _,
    sync::Arc,
};

use alloy::{
    primitives::{Address as AlloyAddress, U256},
    providers::{Provider, ProviderBuilder, RootProvider},
    sol,
    sol_types::SolCall,
};
use num_bigint::BigInt;
use prost::Message as _;
use serde_json::{json, Value};
use tokio_stream::StreamExt as _;
use tycho_indexer::{
    pb::sf::substreams::v1::Package,
    substreams::{
        stream::{BlockResponse, SubstreamsStream},
        SubstreamsEndpoint,
    },
};
use tycho_protobuf::pb::tycho::evm::v1::{BlockChanges, ChangeType as ProtoChangeType};
use tycho_simulation::{
    evm::{
        engine_db::{create_engine, tycho_db::PreCachedDB, SHARED_TYCHO_DB},
        protocol::fluid::{call_resolver, ResolverOverrides},
        simulation::SimulationEngine,
        tycho_models::{AccountUpdate, ChangeType as VmChangeType},
    },
    tycho_client::feed::BlockHeader,
    tycho_common::{models::Chain, Bytes},
};

const ENDPOINT: &str = "https://mainnet.eth.streamingfast.io:443";
const AUTH_URL: &str = "https://auth.streamingfast.io/v1/auth/issue";
const OUTPUT_MODULE: &str = "map_protocol_changes";

const LIQUIDITY_LAYER: &str = "0x52aa899454998be5b000ad077a46bbe360f4e497";
/// The reserves resolver deployed at block 22,487,434; every window here is later than that.
const RESOLVER: &str = "0xc93876c0eed99645dd53937b25433e311881a27c";
const NATIVE_TOKEN: &str = "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

/// `get_block_storage_changes` emitting native balance changes, and `TransactionChangesBuilder`
/// keeping balance-only contract changes, both landed in `tycho-substreams` 0.8.x — which the
/// fluid package picked up in v0.3.2. An older package fails this test on bugs already fixed.
const MIN_PACKAGE_VERSION: (u64, u64, u64) = (0, 3, 2);
const EXPECTED_PARAMS: &str = "liquidity_contract=0x52aa899454998be5b000ad077a46bbe360f4e497&\
factory_address=0x91716c4eda1fb55e84bf8b4c7085f84285c19085&resolvers[0][0]=22487434&\
resolvers[0][1]=0xc93876c0eed99645dd53937b25433e311881a27c&resolvers[1][0]=0&\
resolvers[1][1]=0xb387f9c2092cf7c4943f97842887ebff7ae96eb3&tvl_query_frequency=300&\
tvl_query_start_block=24016914";

const TVL_QUERY_FREQUENCY: u64 = 300;
const TVL_QUERY_START_BLOCK: u64 = 24_016_914;

/// Three dexes deployed at 25,268,517 / 25,268,534 / 25,268,589 all initialize at 25,302,931,
/// which sits between the cadence blocks 25,302,900 and 25,303,200. Deployment and
/// initialization are 34k blocks apart on mainnet, so no window holds both — the component is
/// buffered in `store_components` and emitted at initialization, which is what this asserts.
const DEFAULT_START_BLOCK: u64 = 25_302_880;
const DEFAULT_STOP_BLOCK: u64 = 25_303_210;
const INITIALIZATION_BLOCK: u64 = 25_302_931;
const INITIALIZED_DEXES: [&str; 3] = [
    "0xa2e3a4e2a08b5714fa974ce88466d736bd8b39d9",
    "0x4653583be64eb008d7f34cc6023a81c5033e6f70",
    "0xb9b87a1b79891a8c9251f501b1b5d71bc7c8aa24",
];

/// The only dex ever paused and later unpaused: `LogPauseSwapAndArbitrage` at 24,908,732 and
/// `LogUnpauseSwapAndArbitrage` at 25,188,453, 280k blocks apart.
const PAUSED_DEX: &str = "0x276084527b801e00db8e4410504f9baf93f72c67";
const PAUSE_BLOCK: u64 = 24_908_732;
const UNPAUSE_BLOCK: u64 = 25_188_453;

sol! {
    struct CollateralReserves {
        uint token0RealReserves;
        uint token1RealReserves;
        uint token0ImaginaryReserves;
        uint token1ImaginaryReserves;
    }

    struct DebtReserves {
        uint token0Debt;
        uint token1Debt;
        uint token0RealReserves;
        uint token1RealReserves;
        uint token0ImaginaryReserves;
        uint token1ImaginaryReserves;
    }

    struct TokenLimit {
        uint256 available;
        uint256 expandsTo;
        uint256 expandDuration;
    }

    struct DexLimits {
        TokenLimit withdrawableToken0;
        TokenLimit withdrawableToken1;
        TokenLimit borrowableToken0;
        TokenLimit borrowableToken1;
    }

    struct PoolWithReserves {
        address pool;
        address token0;
        address token1;
        uint256 fee;
        uint256 centerPrice;
        CollateralReserves collateralReserves;
        DebtReserves debtReserves;
        DexLimits limits;
    }

    function getPoolReservesAdjusted(address pool_) public returns (PoolWithReserves memory);
    function getAllPoolsReservesAdjusted() public returns (PoolWithReserves[] memory);
    function decimals() public view returns (uint8);
}

// ─── The spkg ────────────────────────────────────────────────────────────────

fn spkg_path() -> PathBuf {
    std::env::var("FLUID_SPKG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/spkg/ethereum-fluid.spkg")
        })
}

/// Reads the packed spkg and fails unless it is at least [`MIN_PACKAGE_VERSION`] and carries the
/// mainnet params from `substreams.yaml`, so a version or params drift fails loudly rather than
/// quietly running the stream with a different configuration.
fn load_package(path: &PathBuf) -> Package {
    let content = std::fs::read(path).unwrap_or_else(|e| {
        panic!(
            "Failed to read spkg at {}: {e}. Build it from the working tree with \
             `cargo build --target wasm32-unknown-unknown --release` in \
             protocols/substreams/ethereum-fluid, then `substreams pack substreams.yaml -o \
             target/spkg/ethereum-fluid.spkg`.",
            path.display()
        )
    });
    let package = Package::decode(content.as_slice())
        .unwrap_or_else(|e| panic!("Failed to decode spkg: {e}"));

    let version = package
        .package_meta
        .first()
        .map(|meta| meta.version.clone())
        .expect("spkg carries package metadata");
    assert!(
        parse_version(&version) >= MIN_PACKAGE_VERSION,
        "spkg is {version}, below the v{}.{}.{} that carries the tycho-substreams 0.8.x native \
         balance and balance-only contract change fixes this test depends on",
        MIN_PACKAGE_VERSION.0,
        MIN_PACKAGE_VERSION.1,
        MIN_PACKAGE_VERSION.2,
    );

    let params = package
        .networks
        .get(&package.network)
        .and_then(|network| network.params.get(OUTPUT_MODULE))
        .unwrap_or_else(|| {
            panic!("spkg has no {OUTPUT_MODULE} params for network {}", package.network)
        });
    assert_eq!(
        params, EXPECTED_PARAMS,
        "packed {OUTPUT_MODULE} params drifted from substreams.yaml's mainnet params"
    );
    package
}

fn parse_version(version: &str) -> (u64, u64, u64) {
    let mut parts = version
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.parse().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

/// `SubstreamsEndpoint` sends the token raw, so a `server_…` API key must be exchanged for a JWT
/// first. A key that already is a JWT is passed through.
async fn resolve_jwt(api_key: &str) -> String {
    if api_key.starts_with("ey") {
        return api_key.to_string();
    }
    let response: Value = reqwest::Client::new()
        .post(AUTH_URL)
        .json(&json!({ "api_key": api_key }))
        .send()
        .await
        .expect("StreamingFast auth request failed")
        .json()
        .await
        .expect("StreamingFast auth returned no JSON");
    response
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("StreamingFast auth returned no token: {response}"))
        .to_string()
}

/// Streams `[start, stop]` inclusive from the endpoint and returns the decoded `BlockChanges` per
/// block in ascending block order. Substreams' own stop block is exclusive, so it gets `stop + 1`.
async fn stream_block_range(
    jwt: &str,
    package: Package,
    start: u64,
    stop: u64,
) -> Vec<BlockChanges> {
    let endpoint = Arc::new(
        SubstreamsEndpoint::new(ENDPOINT, Some(jwt.to_string()))
            .await
            .expect("Failed to create substreams endpoint"),
    );
    let mut stream = SubstreamsStream::new(
        endpoint,
        None,
        Some(package),
        OUTPUT_MODULE.to_string(),
        start as i64,
        stop + 1,
        true,
        "fluid-spkg-parity".to_string(),
        false,
    );

    let mut blocks = Vec::new();
    loop {
        match stream.next().await {
            Some(Ok(BlockResponse::New(data))) => {
                let output = data
                    .output
                    .as_ref()
                    .and_then(|output| output.map_output.as_ref())
                    .expect("BlockScopedData has no map_output");
                blocks.push(
                    BlockChanges::decode(output.value.as_slice())
                        .expect("Failed to decode BlockChanges"),
                );
            }
            Some(Ok(BlockResponse::Ended)) | None => break,
            Some(Ok(BlockResponse::Undo(_))) => {
                panic!("Unexpected undo signal in range [{start}, {stop}] with final blocks only")
            }
            Some(Err(e)) => panic!("Substreams stream error: {e}"),
        }
    }
    blocks.sort_unstable_by_key(block_number);
    blocks
}

fn block_number(changes: &BlockChanges) -> u64 {
    changes
        .block
        .as_ref()
        .expect("BlockChanges carries a block")
        .number
}

fn block_header(changes: &BlockChanges) -> BlockHeader {
    let block = changes
        .block
        .as_ref()
        .expect("BlockChanges carries a block");
    BlockHeader {
        number: block.number,
        hash: Bytes::from(block.hash.clone()),
        timestamp: block.ts,
        ..Default::default()
    }
}

// ─── The chain ───────────────────────────────────────────────────────────────

fn provider(rpc_url: &str) -> RootProvider {
    ProviderBuilder::new()
        .connect_http(
            rpc_url
                .parse()
                .expect("ETH_RPC_URL is a valid URL"),
        )
        .root()
        .clone()
}

async fn eth_call<P: Provider>(provider: &P, to: &str, data: Vec<u8>, block: u64) -> Vec<u8> {
    let call = json!({ "to": to, "data": format!("0x{}", hex::encode(&data)) });
    let result: Value = provider
        .raw_request("eth_call".into(), (call, format!("0x{block:x}")))
        .await
        .unwrap_or_else(|e| panic!("eth_call to {to} at block {block} failed: {e}"));
    hex::decode(
        result
            .as_str()
            .expect("eth_call returns hex")
            .trim_start_matches("0x"),
    )
    .expect("eth_call returns hex return data")
}

async fn all_pools<P: Provider>(provider: &P, block: u64) -> Vec<PoolWithReserves> {
    let data =
        eth_call(provider, RESOLVER, getAllPoolsReservesAdjustedCall {}.abi_encode(), block).await;
    getAllPoolsReservesAdjustedCall::abi_decode_returns(&data)
        .unwrap_or_else(|e| panic!("Failed to decode getAllPoolsReservesAdjusted at {block}: {e}"))
}

async fn chain_reserves<P: Provider>(provider: &P, pool: AlloyAddress, block: u64) -> Vec<u8> {
    let call = getPoolReservesAdjustedCall { pool_: pool };
    eth_call(provider, RESOLVER, call.abi_encode(), block).await
}

/// Loads every account the resolver reads into `SHARED_TYCHO_DB` as `Creation` updates, from
/// prestate traces of each pool's own resolver call at `block` plus the all-pools call.
///
/// Per-pool traces are what make the seed complete: `PreCachedDB` returns zero for an unseeded
/// slot on a known account, so a slot only the single-pool code path reads would otherwise read
/// as zero and send the resolver down the wrong branch.
///
/// The seed is chain-derived on purpose: production's initial snapshot is chain-derived too, and
/// seeding the starting state is what leaves the *incremental* updates as the subject of the test.
async fn bootstrap_db<P: Provider>(provider: &P, block: u64, pools: &[AlloyAddress]) -> usize {
    let mut calls = vec![getAllPoolsReservesAdjustedCall {}.abi_encode()];
    calls.extend(
        pools
            .iter()
            .map(|pool| getPoolReservesAdjustedCall { pool_: *pool }.abi_encode()),
    );
    let traces = futures::future::join_all(
        calls
            .iter()
            .map(|data| prestate(provider, data, block)),
    )
    .await;

    let mut accounts: BTreeMap<AlloyAddress, PrestateAccount> = BTreeMap::new();
    for trace in &traces {
        for (address, account) in trace
            .as_object()
            .expect("prestate is an object")
        {
            let address = AlloyAddress::from_str(address).expect("prestate key is an address");
            let entry = accounts.entry(address).or_default();
            entry
                .slots
                .extend(prestate_slots(account));
            if let Some(balance) = account
                .get("balance")
                .and_then(Value::as_str)
            {
                entry.balance = Some(U256::from_str(balance).expect("hex balance"));
            }
            if let Some(code) = account
                .get("code")
                .and_then(Value::as_str)
            {
                entry.code = hex::decode(code.trim_start_matches("0x")).expect("hex code");
            }
        }
    }

    let count = accounts.len();
    let updates = accounts
        .into_iter()
        .map(|(address, account)| {
            AccountUpdate::new(
                address,
                Chain::Ethereum,
                account.slots,
                account.balance,
                Some(account.code),
                VmChangeType::Creation,
            )
        })
        .collect();
    SHARED_TYCHO_DB
        .update(updates, Some(BlockHeader { number: block, ..Default::default() }))
        .expect("shared db bootstrap");
    count
}

#[derive(Default)]
struct PrestateAccount {
    slots: HashMap<U256, U256>,
    balance: Option<U256>,
    code: Vec<u8>,
}

async fn prestate<P: Provider>(provider: &P, data: &[u8], block: u64) -> Value {
    let call = json!({ "to": RESOLVER, "data": format!("0x{}", hex::encode(data)) });
    provider
        .raw_request(
            "debug_traceCall".into(),
            (call, format!("0x{block:x}"), json!({ "tracer": "prestateTracer" })),
        )
        .await
        .expect("debug_traceCall failed — does ETH_RPC_URL point at a tracing archive node?")
}

fn prestate_slots(account: &Value) -> HashMap<U256, U256> {
    let Some(storage) = account
        .get("storage")
        .and_then(Value::as_object)
    else {
        return HashMap::new();
    };
    storage
        .iter()
        .map(|(slot, value)| {
            (
                U256::from_str(slot).expect("hex slot"),
                U256::from_str(value.as_str().expect("hex value")).expect("hex value"),
            )
        })
        .collect()
}

async fn token_decimals<P: Provider>(provider: &P, token: AlloyAddress, block: u64) -> i32 {
    if token == AlloyAddress::from_str(NATIVE_TOKEN).expect("native token") {
        return 18;
    }
    let data = eth_call(provider, &token.to_string(), decimalsCall {}.abi_encode(), block).await;
    i32::from(
        decimalsCall::abi_decode_returns(&data)
            .unwrap_or_else(|e| panic!("Failed to decode decimals of {token}: {e}")),
    )
}

// ─── The VM ──────────────────────────────────────────────────────────────────

/// The contracts the spkg is responsible for: the liquidity layer, the reserves resolver and
/// every dex. Everything else the resolver reads is discovered by the DCI.
fn spkg_tracked(pools: &[AlloyAddress]) -> HashSet<AlloyAddress> {
    let mut tracked: HashSet<AlloyAddress> = pools.iter().copied().collect();
    tracked.insert(AlloyAddress::from_str(LIQUIDITY_LAYER).expect("liquidity layer"));
    tracked.insert(AlloyAddress::from_str(RESOLVER).expect("resolver"));
    tracked
}

/// Refreshes the accounts the spkg does not track from the chain at `block`, the way
/// tycho-indexer's DCI keeps them current in production.
///
/// Without this the token contracts the resolver reads through `balanceOf` freeze at their seed
/// values and every pool drifts for reasons the spkg never claimed to cover. Holding them current
/// leaves fluid's own contracts as the only spkg-fed state, so a surviving mismatch can only come
/// from state the spkg owns.
async fn refresh_dci_accounts<P: Provider>(
    provider: &P,
    block: u64,
    header: &BlockHeader,
    tracked: &HashSet<AlloyAddress>,
) -> usize {
    let trace = prestate(provider, &getAllPoolsReservesAdjustedCall {}.abi_encode(), block).await;
    let updates: Vec<AccountUpdate> = trace
        .as_object()
        .expect("prestate is an object")
        .iter()
        .filter_map(|(address, account)| {
            let address = AlloyAddress::from_str(address).expect("prestate key is an address");
            if tracked.contains(&address) {
                return None;
            }
            Some(AccountUpdate::new(
                address,
                Chain::Ethereum,
                prestate_slots(account),
                account
                    .get("balance")
                    .and_then(Value::as_str)
                    .map(|balance| U256::from_str(balance).expect("hex balance")),
                // A `Creation` update is rejected without code, and the prestate omits it for
                // pure-balance accounts such as the zero address.
                Some(
                    account
                        .get("code")
                        .and_then(Value::as_str)
                        .map(|code| hex::decode(code.trim_start_matches("0x")).expect("hex code"))
                        .unwrap_or_default(),
                ),
                VmChangeType::Creation,
            ))
        })
        .collect();

    let count = updates.len();
    SHARED_TYCHO_DB
        .update(updates, Some(header.clone()))
        .expect("dci refresh");
    count
}

/// Applies a block's contract changes to `SHARED_TYCHO_DB` and moves the database header to that
/// block, exactly as the indexer would from the same substreams output.
fn apply_contract_changes(changes: &BlockChanges) -> usize {
    let mut updates = Vec::new();
    for tx_changes in &changes.changes {
        for contract in &tx_changes.contract_changes {
            let slots = contract
                .slots
                .iter()
                .map(|slot| (U256::from_be_slice(&slot.slot), U256::from_be_slice(&slot.value)))
                .collect();
            updates.push(AccountUpdate::new(
                AlloyAddress::from_slice(&contract.address),
                Chain::Ethereum,
                slots,
                (!contract.balance.is_empty()).then(|| U256::from_be_slice(&contract.balance)),
                (!contract.code.is_empty()).then(|| contract.code.clone()),
                VmChangeType::Update,
            ));
        }
    }

    let count = updates.len();
    SHARED_TYCHO_DB
        .update(updates, Some(block_header(changes)))
        .expect("applying contract changes");
    count
}

fn vm_reserves(
    engine: &SimulationEngine<PreCachedDB>,
    pool: AlloyAddress,
) -> Result<Vec<u8>, String> {
    call_resolver(
        &Bytes::from(pool.as_slice().to_vec()),
        &Bytes::from_str(RESOLVER).expect("resolver address"),
        engine,
        ResolverOverrides::default(),
    )
    .map_err(|e| format!("{e}"))
}

// ─── Substreams output views ─────────────────────────────────────────────────

/// The balance changes a block emits, as `pool → token → balance`.
fn emitted_balances(changes: &BlockChanges) -> BTreeMap<String, BTreeMap<String, Vec<u8>>> {
    let mut balances: BTreeMap<String, BTreeMap<String, Vec<u8>>> = BTreeMap::new();
    for tx_changes in &changes.changes {
        for balance in &tx_changes.balance_changes {
            let component = String::from_utf8(balance.component_id.clone())
                .expect("component id is utf8")
                .to_lowercase();
            balances
                .entry(component)
                .or_default()
                .insert(format!("0x{}", hex::encode(&balance.token)), balance.balance.clone());
        }
    }
    balances
}

/// The `paused` attribute changes a block emits, as `component → (value, change type)`.
fn emitted_paused(changes: &BlockChanges) -> BTreeMap<String, (Vec<u8>, i32)> {
    let mut paused = BTreeMap::new();
    for tx_changes in &changes.changes {
        for entity in &tx_changes.entity_changes {
            for attribute in &entity.attributes {
                if attribute.name == "paused" {
                    paused.insert(
                        entity.component_id.to_lowercase(),
                        (attribute.value.clone(), attribute.change),
                    );
                }
            }
        }
    }
    paused
}

/// The substreams' `from_adjusted_amount`: adjusted amounts are 1e12-based, so shift by
/// `decimals - 12`.
fn from_adjusted_amount(adjusted: BigInt, decimals: i32) -> BigInt {
    let diff = decimals - 12;
    if diff < 0 {
        adjusted / BigInt::from(10u64).pow(diff.unsigned_abs())
    } else {
        adjusted * BigInt::from(10u64).pow(diff as u32)
    }
}

fn coerce_native(token: AlloyAddress) -> String {
    if token == AlloyAddress::from_str(NATIVE_TOKEN).expect("native token") {
        ZERO_ADDRESS.to_string()
    } else {
        format!("0x{}", hex::encode(token.as_slice()))
    }
}

fn to_big_int(value: U256) -> BigInt {
    BigInt::from_bytes_be(num_bigint::Sign::Plus, &value.to_be_bytes::<32>())
}

// ─── Assertions on the spkg output alone ─────────────────────────────────────

/// Asserts the three dexes appear only at their initialization block, and that each carries the
/// contracts, static attributes, default `paused` entity change and resolver entrypoint that
/// tycho needs to quote it.
fn assert_component_creation(blocks: &[BlockChanges]) {
    let mut created: BTreeMap<u64, BTreeSet<String>> = BTreeMap::new();
    for changes in blocks {
        for tx_changes in &changes.changes {
            for component in &tx_changes.component_changes {
                created
                    .entry(block_number(changes))
                    .or_default()
                    .insert(component.id.to_lowercase());
                assert_component_shape(component);
            }
        }
    }

    let expected: BTreeSet<String> = INITIALIZED_DEXES
        .iter()
        .map(|dex| dex.to_string())
        .collect();
    assert_eq!(
        created,
        BTreeMap::from([(INITIALIZATION_BLOCK, expected)]),
        "components must be emitted only at initialization, and only the expected dexes"
    );

    let paused_at_init = blocks
        .iter()
        .find(|changes| block_number(changes) == INITIALIZATION_BLOCK)
        .map(emitted_paused)
        .expect("initialization block streamed");
    for dex in INITIALIZED_DEXES {
        assert_eq!(
            paused_at_init.get(dex),
            Some(&(vec![0u8], i32::from(ProtoChangeType::Creation))),
            "{dex} must be created with a default paused = [0]"
        );
    }
}

fn assert_component_shape(component: &tycho_protobuf::pb::tycho::evm::v1::ProtocolComponent) {
    let dex = hex::decode(component.id.trim_start_matches("0x")).expect("component id is hex");
    let contracts: Vec<String> = component
        .contracts
        .iter()
        .map(|contract| format!("0x{}", hex::encode(contract)))
        .collect();
    assert_eq!(
        contracts,
        vec![LIQUIDITY_LAYER.to_string(), RESOLVER.to_string(), component.id.clone()],
        "{} must carry the liquidity layer, resolver and dex contracts",
        component.id
    );

    let attribute = |name: &str| {
        component
            .static_att
            .iter()
            .find(|attribute| attribute.name == name)
            .unwrap_or_else(|| panic!("{} has no {name} static attribute", component.id))
            .value
            .clone()
    };
    assert_eq!(
        format!("0x{}", hex::encode(attribute("reserves_resolver_address"))),
        RESOLVER,
        "{} must point at the post-22,487,434 reserves resolver",
        component.id
    );
    assert_eq!(attribute("deploy_tx").len(), 32, "{} needs a deploy tx hash", component.id);
    assert!(!attribute("t0_decimals").is_empty(), "{} needs t0_decimals", component.id);
    assert!(!attribute("t1_decimals").is_empty(), "{} needs t1_decimals", component.id);
    assert_eq!(
        component
            .contracts
            .get(2)
            .map(Vec::as_slice),
        Some(dex.as_slice()),
        "{} must list its own address last, which the entrypoint calldata is built from",
        component.id
    );
}

fn assert_entrypoints(blocks: &[BlockChanges]) {
    let mut entrypoints: BTreeMap<String, String> = BTreeMap::new();
    for changes in blocks {
        for tx_changes in &changes.changes {
            for entrypoint in &tx_changes.entrypoints {
                assert_eq!(
                    format!("0x{}", hex::encode(&entrypoint.target)),
                    RESOLVER,
                    "entrypoint {} must target the reserves resolver",
                    entrypoint.id
                );
                entrypoints
                    .insert(entrypoint.component_id.to_lowercase(), entrypoint.signature.clone());
            }
        }
    }
    for dex in INITIALIZED_DEXES {
        assert_eq!(
            entrypoints.get(dex).map(String::as_str),
            Some("getPoolReservesAdjusted(address)"),
            "{dex} must get a resolver entrypoint so the DCI can trace it"
        );
    }
}

/// Asserts balances are emitted at every multiple of `tvl_query_frequency` and at component
/// initialization, at no other block, and always for the same set of initialized pools.
fn assert_balance_cadence(blocks: &[BlockChanges]) -> BTreeSet<u64> {
    let mut emitting = BTreeSet::new();
    let mut pool_sets: Vec<(u64, BTreeSet<String>)> = Vec::new();
    for changes in blocks {
        let number = block_number(changes);
        let balances = emitted_balances(changes);
        let expected = number >= TVL_QUERY_START_BLOCK &&
            (number.is_multiple_of(TVL_QUERY_FREQUENCY) || number == INITIALIZATION_BLOCK);
        assert_eq!(
            !balances.is_empty(),
            expected,
            "block {number} {} emit balances",
            if expected { "must" } else { "must not" }
        );
        if expected {
            emitting.insert(number);
            pool_sets.push((number, balances.keys().cloned().collect()));
        }
    }

    assert!(
        emitting.len() >= 2,
        "window must span at least two balance emissions to test the cadence, got {emitting:?}"
    );
    let (_, first) = pool_sets
        .first()
        .expect("an emission")
        .clone();
    for (number, pools) in &pool_sets {
        let newly_initialized = *number >= INITIALIZATION_BLOCK;
        let expected: BTreeSet<String> = if newly_initialized {
            first
                .union(
                    &INITIALIZED_DEXES
                        .iter()
                        .map(|d| d.to_string())
                        .collect(),
                )
                .cloned()
                .collect()
        } else {
            first.clone()
        };
        assert_eq!(
            pools, &expected,
            "block {number} must emit balances for every initialized pool"
        );
    }
    emitting
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Streams the fluid spkg over the window, feeds only its contract changes into the VM database,
/// and asserts the resolver still answers byte-identically to the chain at every block.
///
/// Required env vars (repo-root `.env`):
///   ETH_RPC_URL       — mainnet archive node with `debug_traceCall`; plain archive is not enough
///   STREAMINGFAST_KEY — StreamingFast API key or JWT
///   FLUID_SPKG_PATH   — (optional) spkg override; defaults to target/spkg/ethereum-fluid.spkg
///   FLUID_START_BLOCK / FLUID_STOP_BLOCK — (optional) window overrides
#[tokio::test]
#[ignore = "requires ETH_RPC_URL (tracing archive node) and STREAMINGFAST_KEY"]
async fn spkg_keeps_the_resolver_quotable() {
    dotenv::dotenv().ok();
    let rpc_url = std::env::var("ETH_RPC_URL").expect("ETH_RPC_URL must be set");
    let api_key = std::env::var("STREAMINGFAST_KEY").expect("STREAMINGFAST_KEY must be set");
    let start = env_block("FLUID_START_BLOCK", DEFAULT_START_BLOCK);
    let stop = env_block("FLUID_STOP_BLOCK", DEFAULT_STOP_BLOCK);
    assert!(
        start >= TVL_QUERY_START_BLOCK,
        "window must start at or after tvl_query_start_block {TVL_QUERY_START_BLOCK}"
    );

    let package = load_package(&spkg_path());
    let jwt = resolve_jwt(&api_key).await;
    let provider = provider(&rpc_url);

    let blocks = stream_block_range(&jwt, package, start, stop).await;
    assert_eq!(
        blocks.len() as u64,
        stop - start + 1,
        "substreams must deliver every block in [{start}, {stop}]"
    );

    let pools: Vec<AlloyAddress> = all_pools(&provider, start)
        .await
        .iter()
        .map(|pool| pool.pool)
        .collect();
    let seeded = bootstrap_db(&provider, start, &pools).await;
    let tracked = spkg_tracked(&pools);
    let engine = create_engine(SHARED_TYCHO_DB.clone(), false).expect("engine");

    let mut applied = 0;
    let mut refreshed = 0;
    let mut compared = 0;
    let mut mismatches: Vec<String> = Vec::new();
    for changes in &blocks {
        let number = block_number(changes);
        refreshed +=
            refresh_dci_accounts(&provider, number, &block_header(changes), &tracked).await;
        applied += apply_contract_changes(changes);

        let expected = futures::future::join_all(
            pools
                .iter()
                .map(|pool| chain_reserves(&provider, *pool, number)),
        )
        .await;
        for (pool, on_chain) in pools.iter().zip(expected) {
            compared += 1;
            match vm_reserves(&engine, *pool) {
                Ok(simulated) if simulated == on_chain => {}
                Ok(simulated) => {
                    mismatches.push(describe_mismatch(number, *pool, &simulated, &on_chain))
                }
                Err(e) => mismatches
                    .push(format!("block {number} pool {pool}: resolver call failed: {e}")),
            }
        }
    }

    assert_component_creation(&blocks);
    assert_entrypoints(&blocks);
    let emissions = assert_balance_cadence(&blocks);
    assert_balance_values(&provider, &blocks, &emissions).await;

    println!("\n─── Coverage ───────────────────────────────────────────────────────────────");
    println!("  Window:                [{start}, {stop}] ({} blocks)", blocks.len());
    println!("  Accounts seeded:       {seeded}");
    println!("  Contract changes:      {applied}");
    println!("  DCI accounts kept:     {refreshed}");
    println!("  Pools per block:       {}", pools.len());
    println!("  Resolver comparisons:  {compared}");
    println!("  Balance emissions at:  {emissions:?}");
    println!("  Mismatches:            {}", mismatches.len());
    println!("────────────────────────────────────────────────────────────────────────────");

    assert!(
        mismatches.is_empty(),
        "the spkg failed to deliver state the resolver reads ({} mismatches):\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

fn env_block(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Names the first differing 32-byte word, so a mismatch points at the field that drifted.
fn describe_mismatch(block: u64, pool: AlloyAddress, simulated: &[u8], on_chain: &[u8]) -> String {
    let first_diff = simulated
        .chunks(32)
        .zip(on_chain.chunks(32))
        .position(|(left, right)| left != right);
    match first_diff {
        Some(word) => format!(
            "block {block} pool {pool}: word {word} is 0x{} on the spkg-fed VM, 0x{} on chain",
            hex::encode(&simulated[word * 32..(word + 1) * 32]),
            hex::encode(&on_chain[word * 32..(word + 1) * 32]),
        ),
        None => format!(
            "block {block} pool {pool}: {} bytes from the spkg-fed VM, {} on chain",
            simulated.len(),
            on_chain.len()
        ),
    }
}

/// Checks the emitted balances against `getAllPoolsReservesAdjusted` at the same block, scaled
/// the way the substreams scale them and with the native token coerced to the zero address.
async fn assert_balance_values<P: Provider>(
    provider: &P,
    blocks: &[BlockChanges],
    emissions: &BTreeSet<u64>,
) {
    let mut decimals: HashMap<AlloyAddress, i32> = HashMap::new();
    for number in emissions {
        let emitted = blocks
            .iter()
            .find(|changes| block_number(changes) == *number)
            .map(emitted_balances)
            .expect("emitting block streamed");
        for pool in all_pools(provider, *number).await {
            let id = format!("0x{}", hex::encode(pool.pool.as_slice()));
            let Some(emitted) = emitted.get(&id) else { continue };
            for (token, adjusted) in [
                (
                    pool.token0,
                    pool.collateralReserves
                        .token0RealReserves +
                        pool.debtReserves.token0Debt,
                ),
                (
                    pool.token1,
                    pool.collateralReserves
                        .token1RealReserves +
                        pool.debtReserves.token1Debt,
                ),
            ] {
                let decimals = match decimals.get(&token) {
                    Some(decimals) => *decimals,
                    None => {
                        let fetched = token_decimals(provider, token, *number).await;
                        decimals.insert(token, fetched);
                        fetched
                    }
                };
                let expected = from_adjusted_amount(to_big_int(adjusted), decimals);
                assert_eq!(
                    emitted.get(&coerce_native(token)),
                    Some(&expected.to_signed_bytes_be()),
                    "block {number} pool {id} token {token}: emitted balance must equal the \
                     resolver's reserves scaled from the 1e12 adjusted amount"
                );
            }
        }
    }
}

/// Asserts the spkg turns `LogPauseSwapAndArbitrage` into `paused = [1]` and
/// `LogUnpauseSwapAndArbitrage` into a deletion of the attribute.
///
/// These are the only pause and unpause the dex has ever seen, 280k blocks apart, so each gets
/// its own short window.
#[tokio::test]
#[ignore = "requires ETH_RPC_URL (tracing archive node) and STREAMINGFAST_KEY"]
async fn spkg_emits_paused_transitions() {
    dotenv::dotenv().ok();
    let api_key = std::env::var("STREAMINGFAST_KEY").expect("STREAMINGFAST_KEY must be set");
    let jwt = resolve_jwt(&api_key).await;
    let package = load_package(&spkg_path());

    for (block, expected) in [
        (PAUSE_BLOCK, (vec![1u8], i32::from(ProtoChangeType::Creation))),
        (UNPAUSE_BLOCK, (Vec::new(), i32::from(ProtoChangeType::Deletion))),
    ] {
        let blocks = stream_block_range(&jwt, package.clone(), block - 1, block + 1).await;
        for changes in &blocks {
            let number = block_number(changes);
            let paused = emitted_paused(changes);
            if number == block {
                assert_eq!(
                    paused.get(PAUSED_DEX),
                    Some(&expected),
                    "block {number} must carry the pause transition for {PAUSED_DEX}"
                );
            } else {
                assert!(
                    !paused.contains_key(PAUSED_DEX),
                    "block {number} must not touch {PAUSED_DEX}'s paused attribute"
                );
            }
        }
    }
}
