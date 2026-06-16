use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use num_bigint::BigUint;
use tycho_client::feed::{dto, BlockHeader, FeedMessage};
use tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim, Bytes};
use tycho_simulation::evm::{
    decoder::TychoStreamDecoder,
    engine_db::tycho_db::PreCachedDB,
    protocol::vm::state::EVMPoolState,
};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("fixtures")
}

fn load_tokens() -> HashMap<Bytes, Token> {
    let path = fixtures_dir().join("tokens.json");
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read tokens.json at {}: {e}", path.display()));
    let tokens: Vec<Token> = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("Failed to deserialize tokens.json: {e}"));
    tokens
        .into_iter()
        .map(|t| (t.address.clone(), t))
        .collect()
}

fn load_feed_message(fixture: &str) -> FeedMessage<BlockHeader> {
    let path = fixtures_dir().join(format!("{fixture}.json"));
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("Failed to read fixture {fixture}.json at {}: {e}", path.display())
    });
    let dto_msg: dto::FeedMessage<BlockHeader> = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("Failed to deserialize fixture {fixture}.json: {e}"));
    dto_msg.into()
}

/// Loads and decodes all pools from a fixture file.
///
/// Deserializes the fixture as a `dto::FeedMessage`, converts it to the runtime `FeedMessage`,
/// registers decoders for `vm:balancer_v2` and `vm:curve`, sets the token map from
/// `tokens.json`, and returns the decoded pool states keyed by component ID.
pub fn load_pools(fixture: &str) -> HashMap<String, Box<dyn ProtocolSim>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to build Tokio runtime");

    rt.block_on(async move {
        let mut decoder = TychoStreamDecoder::<BlockHeader>::new();
        decoder.register_decoder::<EVMPoolState<PreCachedDB>>("vm:balancer_v2");
        decoder.register_decoder::<EVMPoolState<PreCachedDB>>("vm:curve");

        let tokens = load_tokens();
        decoder.set_tokens(tokens).await;

        let msg = load_feed_message(fixture);
        decoder
            .decode(&msg)
            .await
            .unwrap_or_else(|e| panic!("Failed to decode fixture {fixture}: {e}"))
            .states
    })
}

/// Returns the first two tokens of the first component in a fixture, in the order they appear
/// in the component's token list.
///
/// These are used as `(token_in, token_out)` in benchmarks and smoke tests.
pub fn pool_tokens(fixture: &str) -> (Token, Token) {
    let path = fixtures_dir().join(format!("{fixture}.json"));
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("Failed to read fixture {fixture}.json at {}: {e}", path.display())
    });
    let dto_msg: dto::FeedMessage<BlockHeader> = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("Failed to deserialize fixture {fixture}.json: {e}"));

    let tokens_map = load_tokens();

    let state_msg = dto_msg
        .state_msgs
        .into_values()
        .next()
        .expect("Fixture has no protocol state messages");

    let component = state_msg
        .snapshots
        .states
        .into_values()
        .next()
        .expect("Fixture has no snapshot states");

    let token_addrs = &component.component.tokens;
    assert!(
        token_addrs.len() >= 2,
        "Component has fewer than 2 tokens; cannot form a (token_in, token_out) pair"
    );

    let lookup = |addr: &Bytes| -> Token {
        tokens_map
            .get(addr)
            .cloned()
            .unwrap_or_else(|| panic!("Token {addr} not found in tokens.json"))
    };

    (lookup(&token_addrs[0]), lookup(&token_addrs[1]))
}

/// Convenience wrapper: call `get_amount_out` on a pool state.
///
/// Returns the `Ok` result or panics with a descriptive message.
#[allow(dead_code)]
pub fn get_amount_out_or_panic(
    state: &dyn ProtocolSim,
    amount: BigUint,
    token_in: &Token,
    token_out: &Token,
) -> tycho_common::simulation::protocol_sim::GetAmountOutResult {
    state
        .get_amount_out(amount, token_in, token_out)
        .unwrap_or_else(|e| panic!("get_amount_out failed: {e:?}"))
}
