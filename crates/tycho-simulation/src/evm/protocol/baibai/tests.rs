use std::{collections::HashMap, str::FromStr};

use alloy::primitives::U256;
use num_bigint::BigUint;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{token::Token, Chain},
    simulation::protocol_sim::{Balances, BlockContext, ProtocolSim},
    Bytes,
};

use super::{math::big, BaibaiState};

#[derive(serde::Deserialize)]
struct Fixture {
    timestamp: u64,
    base: Bytes,
    quote: Bytes,
    scenarios: Vec<Scenario>,
}
#[derive(serde::Deserialize)]
struct Scenario {
    name: String,
    words: [U256; 32],
    cases: Vec<Case>,
}
#[derive(serde::Deserialize)]
struct Case {
    sell_base: bool,
    input: String,
    output: String,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/assets/baibai-quotes.json"
    )))
    .unwrap()
}

fn state(fixture: &Fixture, scenario: &Scenario) -> BaibaiState {
    BaibaiState {
        id: "pair".into(),
        tokens: [fixture.base.clone(), fixture.quote.clone()],
        words: scenario.words,
        balances: [U256::from(10).pow(U256::from(24)); 2],
        c_unit: U256::from(1),
        timestamp: fixture.timestamp,
    }
}

fn tokens(state: &BaibaiState) -> [Token; 2] {
    [
        Token::new(&state.tokens[0], "WETH", 18, 100, &[], Chain::Base, 0),
        Token::new(&state.tokens[1], "USDC", 6, 100, &[], Chain::Base, 0),
    ]
}

/// Expected values come from eth_call against deployed CurveBook v3 bytecode at
/// the fixture's pinned block, with only the listed curve storage overridden.
#[test]
fn quotes_match_deployed_bytecode() {
    let fixture = fixture();
    for scenario in &fixture.scenarios {
        let state = state(&fixture, scenario);
        let tokens = tokens(&state);
        for case in &scenario.cases {
            let input = BigUint::from_str(&case.input).unwrap();
            let expected = BigUint::from_str(&case.output).unwrap();
            let direction = usize::from(!case.sell_base);
            let actual =
                state.get_amount_out(input.clone(), &tokens[direction], &tokens[1 - direction]);
            if expected == BigUint::ZERO {
                assert!(actual.is_err(), "{} {} {}", scenario.name, direction, input);
            } else {
                let actual = actual
                    .unwrap_or_else(|e| panic!("{} {} {}: {e}", scenario.name, direction, input));
                assert_eq!(actual.amount, expected, "{} {} {}", scenario.name, direction, input);
                let next = actual
                    .new_state
                    .as_any()
                    .downcast_ref::<BaibaiState>()
                    .unwrap();
                assert_eq!(big(next.balances[direction]), big(state.balances[direction]) + input);
                assert_eq!(
                    big(next.balances[1 - direction]),
                    big(state.balances[1 - direction]) - expected.clone()
                );
                let cursor = if case.sell_base { 5 } else { 4 };
                assert_eq!(
                    big(next.words[cursor] - state.words[cursor]),
                    if case.sell_base { BigUint::from_str(&case.input).unwrap() } else { expected }
                );
            }
        }
    }
}

#[test]
fn reservations_limit_output_and_sequential_swaps_consume_inventory() {
    let fixture = fixture();
    let mut state = state(&fixture, &fixture.scenarios[0]);
    let tokens = tokens(&state);
    state.words[31] = state.balances[1] - U256::from(50_000_000);
    let (limit, output) = state
        .get_limits(state.tokens[0].clone(), state.tokens[1].clone())
        .unwrap();
    assert!(output <= BigUint::from(50_000_000u64));
    assert!(state
        .get_amount_out(limit.clone() + BigUint::from(10_000_000_000u64), &tokens[0], &tokens[1])
        .is_err());
    let fill = state
        .get_amount_out(limit, &tokens[0], &tokens[1])
        .unwrap();
    assert_eq!(fill.amount, output);
    assert!(fill
        .new_state
        .get_amount_out(BigUint::from(10_000_000_000u64), &tokens[0], &tokens[1])
        .is_err());
}

#[test]
fn expiry_advances_without_pool_updates_and_deadline_is_inclusive() {
    let fixture = fixture();
    let mut state = state(&fixture, &fixture.scenarios[0]);
    assert!(!state.apply_block(&BlockContext::new(1, fixture.timestamp + 30)));
    assert!(state.fresh());
    assert!(state.apply_block(&BlockContext::new(2, fixture.timestamp + 31)));
    assert!(!state.fresh());
    state.words[0] = U256::ZERO;
    assert!(!state.fresh(), "TTL=0 must still enforce validUntil");
}

#[test]
fn invalid_delta_is_atomic_and_balances_update_independently() {
    let fixture = fixture();
    let mut state = state(&fixture, &fixture.scenarios[0]);
    let original = state.clone();
    let delta = ProtocolStateDelta {
        component_id: state.id.clone(),
        updated_attributes: HashMap::from([
            ("word_0".into(), Bytes::from([0])),
            ("word_1".into(), Bytes::from(vec![1; 33])),
        ]),
        deleted_attributes: Default::default(),
    };
    assert!(state
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .is_err());
    assert_eq!(state, original);
    let balances = Balances {
        component_balances: HashMap::from([(
            state.id.clone(),
            HashMap::from([(state.tokens[0].clone(), Bytes::from([7]))]),
        )]),
        ..Default::default()
    };
    state
        .delta_transition(
            ProtocolStateDelta {
                component_id: state.id.clone(),
                updated_attributes: HashMap::new(),
                deleted_attributes: Default::default(),
            },
            &HashMap::new(),
            &balances,
        )
        .unwrap();
    assert_eq!(state.balances[0], U256::from(7));
    assert_eq!(state.balances[1], original.balances[1]);
}

#[test]
fn rejects_unknown_tokens_and_oversized_amounts() {
    let fixture = fixture();
    let state = state(&fixture, &fixture.scenarios[0]);
    let tokens = tokens(&state);
    assert!(state
        .get_amount_out(BigUint::from(1u8), &tokens[0], &tokens[0])
        .is_err());
    assert!(state
        .get_amount_out(BigUint::from(1u8) << 256, &tokens[0], &tokens[1])
        .is_err());
}

#[tokio::test]
async fn snapshot_decodes_roles_independently_of_token_order_and_requires_full_state() {
    use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
    use tycho_common::dto::{ProtocolComponent, ResponseProtocolState};

    use crate::protocol::models::{DecoderContext, TryFromWithBlock};
    let fixture = fixture();
    let expected = state(&fixture, &fixture.scenarios[0]);
    let all_tokens = tokens(&expected)
        .into_iter()
        .map(|token| (token.address.clone(), token))
        .collect();
    let snapshot = ComponentWithState {
        component: ProtocolComponent {
            id: expected.id.clone(),
            protocol_system: "baibai".into(),
            protocol_type_name: "baibai_pool".into(),
            tokens: vec![expected.tokens[1].clone(), expected.tokens[0].clone()],
            static_attributes: HashMap::from([
                ("base".into(), expected.tokens[0].clone()),
                ("quote".into(), expected.tokens[1].clone()),
            ]),
            ..Default::default()
        }
        .into(),
        state: ResponseProtocolState {
            component_id: expected.id.clone(),
            attributes: expected
                .words
                .iter()
                .enumerate()
                .map(|(i, word)| (format!("word_{i}"), Bytes::from(word.to_be_bytes::<32>())))
                .collect(),
            balances: expected
                .tokens
                .iter()
                .enumerate()
                .map(|(i, token)| {
                    (token.clone(), Bytes::from(expected.balances[i].to_be_bytes::<32>()))
                })
                .collect(),
        }
        .into(),
        component_tvl: None,
        entrypoints: vec![],
    };
    let header = BlockHeader { timestamp: expected.timestamp, ..Default::default() };
    let decoded = BaibaiState::try_from_with_header(
        snapshot.clone(),
        header.clone(),
        &HashMap::new(),
        &all_tokens,
        &DecoderContext::new(),
    )
    .await
    .unwrap();
    assert_eq!(decoded, expected);
    let mut incomplete = snapshot;
    incomplete
        .state
        .attributes
        .remove("word_31");
    assert!(BaibaiState::try_from_with_header(
        incomplete,
        header,
        &HashMap::new(),
        &all_tokens,
        &DecoderContext::new()
    )
    .await
    .is_err());
}
