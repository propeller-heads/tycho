//! One-shot tool that streams a single startup snapshot from Tycho and writes per-pool fixture
//! JSON files under `crates/tycho-simulation/benches/fixtures/`.
//!
//! Target pools (by token count):
//!   - 2-token Balancer V2 pool  → balancer_v2_2token.json
//!   - 3-token Curve pool        → curve_3token.json
//!   - 4-token Curve/Balancer V4 → curve_4token.json
//!
//! Each fixture is a `dto::FeedMessage` (serialisable) filtered to the one chosen component plus
//! all vm_storage for that component's contract addresses. A companion `tokens.json` holds the
//! `Token` metadata for every token referenced by the three pools.
//!
//! Run:
//!   cargo run --release --example capture_vm_fixtures
//!
//! Requires `TYCHO_API_KEY` in the environment.

use std::{
    collections::{HashMap, HashSet},
    env,
    fs,
    path::PathBuf,
    time::Duration,
};

use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use tycho_client::{
    feed::{
        component_tracker::ComponentFilter,
        dto,
        BlockHeader,
        FeedMessage,
    },
    stream::TychoStreamBuilder,
};
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};
use tycho_simulation::utils::load_all_tokens;

const TYCHO_ENDPOINT: &str = "tycho-beta.propellerheads.xyz";
/// TVL threshold: capture any pool with TVL >= 10 ETH to find a variety of token-count sizes.
const TVL_THRESHOLD: f64 = 10.0;
/// Timeout for receiving the first snapshot.
const SNAPSHOT_TIMEOUT_SECS: u64 = 180;

/// Describes a pool fixture to capture.
struct FixtureTarget {
    /// Human-readable label for logging.
    label: &'static str,
    /// Output file name (inside benches/fixtures/).
    filename: &'static str,
    /// Protocol system the pool belongs to (key in `state_msgs`).
    protocol: &'static str,
    /// Minimum number of tokens in the component.
    min_tokens: usize,
    /// Maximum number of tokens in the component.
    max_tokens: usize,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let api_key = env::var("TYCHO_API_KEY")
        .expect("TYCHO_API_KEY environment variable must be set");

    let fixtures_dir = fixtures_path();
    fs::create_dir_all(&fixtures_dir).expect("Failed to create fixtures directory");

    info!("Connecting to Tycho at {TYCHO_ENDPOINT}...");

    let tvl_filter = ComponentFilter::with_tvl_range(TVL_THRESHOLD, TVL_THRESHOLD);

    let (_handle, mut rx) = TychoStreamBuilder::new(TYCHO_ENDPOINT, Chain::Ethereum)
        .exchange("vm:balancer_v2", tvl_filter.clone())
        .exchange("vm:curve", tvl_filter.clone())
        .auth_key(Some(api_key.clone()))
        .max_messages(1)
        .startup_timeout(Duration::from_secs(SNAPSHOT_TIMEOUT_SECS))
        .build()
        .await
        .expect("Failed to build Tycho stream");

    info!("Waiting for first snapshot (up to {SNAPSHOT_TIMEOUT_SECS}s)...");

    let feed_msg = tokio::time::timeout(
        Duration::from_secs(SNAPSHOT_TIMEOUT_SECS),
        rx.recv(),
    )
    .await
    .expect("Timed out waiting for first snapshot")
    .expect("Stream channel closed before first message")
    .expect("Stream returned an error on first message");

    info!(
        "Received snapshot with {} protocol state messages",
        feed_msg.state_msgs.len()
    );
    for (name, msg) in &feed_msg.state_msgs {
        info!(
            "  protocol={name} components={} vm_storage_accounts={}",
            msg.snapshots.states.len(),
            msg.snapshots.vm_storage.len()
        );
    }

    // Convert the runtime FeedMessage to the dto form (serde-capable).
    let dto_feed: dto::FeedMessage<BlockHeader> = feed_msg.into();

    let targets: Vec<FixtureTarget> = vec![
        FixtureTarget {
            label: "Balancer V2 2-token",
            filename: "balancer_v2_2token.json",
            protocol: "vm:balancer_v2",
            min_tokens: 2,
            max_tokens: 2,
        },
        FixtureTarget {
            label: "Curve 3-token",
            filename: "curve_3token.json",
            protocol: "vm:curve",
            min_tokens: 3,
            max_tokens: 3,
        },
        FixtureTarget {
            label: "Curve 4-token",
            filename: "curve_4token.json",
            protocol: "vm:curve",
            min_tokens: 4,
            max_tokens: 4,
        },
    ];

    // Collect all token addresses referenced by captured components.
    let mut all_referenced_tokens: HashSet<Bytes> = HashSet::new();
    let mut captured_count = 0;

    for target in &targets {
        let Some(state_msg) = dto_feed.state_msgs.get(target.protocol) else {
            warn!("Protocol '{}' not found in snapshot — skipping {}", target.protocol, target.label);
            continue;
        };

        let chosen = choose_component(state_msg, target.min_tokens, target.max_tokens);
        let Some((component_id, component_with_state)) = chosen else {
            warn!(
                "No component with {}-{} tokens found in '{}' — skipping {}",
                target.min_tokens, target.max_tokens, target.protocol, target.label
            );
            continue;
        };

        info!(
            "Selected component '{}' (tokens={}) for {}",
            component_id,
            component_with_state.component.tokens.len(),
            target.label
        );

        // Collect token addresses from this component.
        for token in &component_with_state.component.tokens {
            all_referenced_tokens.insert(token.clone());
        }

        // Determine which contract addresses belong to this component.
        let component_contracts: HashSet<Bytes> = component_with_state
            .component
            .contract_ids
            .iter()
            .cloned()
            .collect();

        // Build a filtered snapshot: just this one component + its vm_storage.
        let filtered_vm_storage: HashMap<Bytes, _> = state_msg
            .snapshots
            .vm_storage
            .iter()
            .filter(|(addr, _)| component_contracts.contains(*addr))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        info!(
            "  vm_storage entries for component: {} (out of {} total)",
            filtered_vm_storage.len(),
            state_msg.snapshots.vm_storage.len()
        );

        let mut filtered_states = HashMap::new();
        filtered_states.insert(component_id.clone(), component_with_state.clone());

        let filtered_snapshot = dto::Snapshot {
            states: filtered_states,
            vm_storage: filtered_vm_storage,
        };

        let filtered_state_msg = dto::StateSyncMessage {
            header: state_msg.header.clone(),
            snapshots: filtered_snapshot,
            deltas: None,
            removed_components: HashMap::new(),
        };

        let mut filtered_state_msgs = HashMap::new();
        filtered_state_msgs.insert(target.protocol.to_string(), filtered_state_msg);

        let filtered_feed = dto::FeedMessage {
            state_msgs: filtered_state_msgs,
            sync_states: dto_feed
                .sync_states
                .iter()
                .filter(|(k, _)| k.as_str() == target.protocol)
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        };

        let json = serde_json::to_string_pretty(&filtered_feed)
            .expect("Failed to serialize fixture to JSON");

        let output_path = fixtures_dir.join(target.filename);
        fs::write(&output_path, &json).expect("Failed to write fixture file");

        info!(
            "Wrote {} ({} bytes) to {:?}",
            target.filename,
            json.len(),
            output_path
        );

        // Verify round-trip: the dto JSON must deserialise back into the runtime FeedMessage type.
        verify_round_trip(&json, target.filename);

        captured_count += 1;
    }

    if captured_count == 0 {
        error!("No fixtures were captured. Check TVL threshold or exchange names.");
        std::process::exit(1);
    }

    // Write tokens.json for all tokens referenced by the captured pools.
    write_tokens_fixture(&fixtures_dir, &api_key, &all_referenced_tokens).await;

    info!("Done. Captured {}/{} fixtures.", captured_count, targets.len());
}

/// Chooses the first component whose token count is in `[min_tokens, max_tokens]`.
fn choose_component<'a>(
    state_msg: &'a dto::StateSyncMessage<BlockHeader>,
    min_tokens: usize,
    max_tokens: usize,
) -> Option<(String, &'a dto::ComponentWithState)> {
    state_msg
        .snapshots
        .states
        .iter()
        .find(|(_, cws)| {
            let n = cws.component.tokens.len();
            n >= min_tokens && n <= max_tokens
        })
        .map(|(id, cws)| (id.clone(), cws))
}

/// Verifies that the JSON fixture round-trips: JSON → dto::FeedMessage → FeedMessage (runtime).
///
/// The runtime `FeedMessage` does not implement `Deserialize` directly; callers deserialise via
/// the dto form and convert. This mirrors exactly what later benchmark tasks will do.
fn verify_round_trip(json: &str, filename: &str) {
    let dto_result: Result<dto::FeedMessage<BlockHeader>, _> = serde_json::from_str(json);
    match dto_result {
        Ok(dto_msg) => {
            let _runtime_msg: FeedMessage<BlockHeader> = dto_msg.into();
            info!("Round-trip OK for {filename}");
        }
        Err(e) => {
            error!("Round-trip FAILED for {filename}: {e}");
            std::process::exit(1);
        }
    }
}

/// Fetches token metadata for the referenced addresses and writes `tokens.json`.
async fn write_tokens_fixture(
    fixtures_dir: &PathBuf,
    api_key: &str,
    referenced_addresses: &HashSet<Bytes>,
) {
    info!(
        "Fetching token metadata for {} token addresses...",
        referenced_addresses.len()
    );

    let all_tokens = load_all_tokens(
        TYCHO_ENDPOINT,
        false,
        Some(api_key),
        true,
        Chain::Ethereum,
        None,
        None,
    )
    .await
    .expect("Failed to load token metadata from Tycho");

    // Keep only tokens whose address appears in one of our captured components.
    let relevant: Vec<&Token> = all_tokens
        .values()
        .filter(|t| referenced_addresses.contains(&t.address))
        .collect();

    info!("Found {} relevant tokens (out of {} total)", relevant.len(), all_tokens.len());

    let json = serde_json::to_string_pretty(&relevant)
        .expect("Failed to serialize tokens to JSON");

    let output_path = fixtures_dir.join("tokens.json");
    fs::write(&output_path, &json).expect("Failed to write tokens.json");

    info!("Wrote tokens.json ({} bytes) to {:?}", json.len(), output_path);
}

/// Returns the path to the `benches/fixtures/` directory, resolved relative to this file's
/// crate root (i.e., the `CARGO_MANIFEST_DIR` environment variable set at compile time).
fn fixtures_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("fixtures")
}
