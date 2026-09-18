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
        fee_attributes: [
            format!("pair_fee_{}", hex::encode([1; 20])),
            format!("taker_fee_{}", hex::encode([1; 20])),
        ],
        fees: [None; 2],
        paused: false,
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
            chain: tycho_common::dto::Chain::Base,
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
                .chain([(String::from("fees_indexed"), Bytes::from([1u8]))])
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
        &DecoderContext::new().caller(Bytes::from([1u8; 20])),
    )
    .await
    .unwrap();
    assert_eq!(decoded, expected);
    let mut with_fee = snapshot.clone();
    with_fee.state.attributes.insert(
        "taker_fee_aba5b53b03eafad1c5fc8bd5fc765fc85bb3de67".into(),
        Bytes::from([1, 0, 25]),
    );
    let fallback = BaibaiState::try_from_with_header(
        with_fee,
        header.clone(),
        &HashMap::new(),
        &all_tokens,
        &DecoderContext::new(),
    )
    .await
    .unwrap();
    assert_eq!(fallback.fee(), 0.0025);
    let mut wrong_chain = snapshot.clone();
    wrong_chain.component.chain = Chain::Ethereum;
    assert!(BaibaiState::try_from_with_header(
        wrong_chain,
        header.clone(),
        &HashMap::new(),
        &all_tokens,
        &DecoderContext::new(),
    )
    .await
    .is_err());
    assert!(BaibaiState::try_from_with_header(
        snapshot.clone(),
        header.clone(),
        &HashMap::new(),
        &all_tokens,
        &DecoderContext::new().caller(Bytes::from([1u8; 19])),
    )
    .await
    .is_err());
    let mut without_fees = snapshot.clone();
    without_fees
        .state
        .attributes
        .remove("fees_indexed");
    assert!(BaibaiState::try_from_with_header(
        without_fees,
        header.clone(),
        &HashMap::new(),
        &all_tokens,
        &DecoderContext::new(),
    )
    .await
    .is_err());
    let mut paused = snapshot.clone();
    paused
        .state
        .attributes
        .insert("paused".into(), Bytes::from([1u8]));
    assert!(BaibaiState::try_from_with_header(
        paused,
        header.clone(),
        &HashMap::new(),
        &all_tokens,
        &DecoderContext::new(),
    )
    .await
    .is_err());
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
        &DecoderContext::new().caller(Bytes::from([1u8; 20]))
    )
    .await
    .is_err());
}

#[test]
fn bid_limits_stop_before_decreasing_proceeds_and_respect_custody() {
    let fixture = fixture();
    let mut state = state(&fixture, &fixture.scenarios[0]);
    let wad = U256::from(10u64.pow(18));
    state.words[2] =
        U256::from(100_000_000u64) | (U256::from(10_000) << 128usize) | (U256::from(2) << 152usize);
    state.words[3] = wad;
    state.words[5] = U256::ZERO;
    state.words[18] = (U256::from(1) << 214usize) |
        (((U256::from(2) << 42usize) | U256::from(250_000_000)) << 88usize);
    let tokens = tokens(&state);
    for (filled, depth, custody, expected_output) in [
        (0, 10_000, 1_000_000_000, 100_000_000),
        (0, 10_000, 50_000_000, 50_000_000),
        (1, 10_000, 1_000_000_000, 50_000_000),
        (0, 2_500, 1_000_000_000, 50_000_000),
        (2, 10_000, 1_000_000_000, 0),
    ] {
        state.words[5] = wad * U256::from(filled) / U256::from(2);
        state.words[2] =
            (state.words[2] & !(U256::from(65_535) << 128usize)) | (U256::from(depth) << 128usize);
        state.words[31] = state.balances[1] - U256::from(custody);
        let (limit, output) = state
            .get_limits(state.tokens[0].clone(), state.tokens[1].clone())
            .unwrap();
        assert_eq!(output, BigUint::from(expected_output as u64));
        if output != BigUint::ZERO {
            assert!(limit <= big(wad - state.words[5]));
            assert_eq!(
                state
                    .get_amount_out(limit, &tokens[0], &tokens[1])
                    .unwrap()
                    .amount,
                output
            );
        } else {
            assert_eq!(limit, BigUint::ZERO);
        }
    }
}

#[test]
fn bid_limits_bound_rounded_proceeds_within_each_segment() {
    let fixture = fixture();
    let mut state = state(&fixture, &fixture.scenarios[0]);
    // floor(0.6 * input) - ceil(0.5 * input) can decrease by one atom,
    // even though the underlying marginal price is positive.
    for knots in [vec![(100u64, 50u64)], vec![(37, 18), (100, 50)]] {
        state.words[2] = U256::from(600_000_000_000_000_000u64) |
            (U256::from(10_000) << 128usize) |
            (U256::from(knots.len()) << 152usize);
        state.words[3] = U256::from(1);
        state.words[18] = knots
            .iter()
            .enumerate()
            .fold(U256::ZERO, |packed, (i, &(q, c))| {
                packed | (((U256::from(q) << 42usize) | U256::from(c)) << (172 - 84 * i))
            });
        for filled in 0..40 {
            state.words[5] = U256::from(filled);
            for custody in 1..10 {
                state.words[31] = state.balances[1] - U256::from(custody);
                let (limit, output) = state
                    .get_limits(state.tokens[0].clone(), state.tokens[1].clone())
                    .unwrap();
                let side = state.side(false).unwrap();
                let limit = super::math::uint(&limit)
                    .unwrap()
                    .to::<u64>();
                for input in 0..=limit {
                    assert!(
                        side.quote(U256::from(input)).unwrap().0 <= U256::from(custody),
                        "knots={knots:?}, filled={filled}, custody={custody}, limit={limit}, input={input}"
                    );
                }
                assert_eq!(output, big(side.quote(U256::from(limit)).unwrap().0));
            }
        }
    }
}

#[test]
fn fees_follow_override_precedence_and_updates_are_atomic() {
    let fixture = fixture();
    let mut state = state(&fixture, &fixture.scenarios[0]);
    let mut update = |attrs: Vec<(String, Bytes)>, expected: f64| {
        state
            .delta_transition(
                ProtocolStateDelta {
                    component_id: state.id.clone(),
                    updated_attributes: attrs.into_iter().collect(),
                    deleted_attributes: Default::default(),
                },
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();
        assert_eq!(state.fee(), expected);
    };
    let pair = format!("pair_fee_{}", hex::encode([1; 20]));
    let router = format!("taker_fee_{}", hex::encode([1; 20]));
    update(vec![(router.clone(), Bytes::from([1, 0, 10]))], 0.001);
    update(vec![(pair.clone(), Bytes::from([1, 0, 0]))], 0.0);
    update(vec![(pair, Bytes::from([0, 0, 0]))], 0.001);
    update(vec![(router, Bytes::from([0, 0, 0]))], 0.0);
    update(vec![(format!("pair_fee_{}", hex::encode([2; 20])), Bytes::from([1, 3, 232]))], 0.0);
    let before = state.clone();
    assert!(state
        .delta_transition(
            ProtocolStateDelta {
                component_id: state.id.clone(),
                updated_attributes: HashMap::from([(
                    format!("taker_fee_{}", hex::encode([1; 20])),
                    Bytes::from([1, 3, 233])
                )]),
                deleted_attributes: Default::default(),
            },
            &HashMap::new(),
            &Balances::default()
        )
        .is_err());
    assert_eq!(state, before);
}

#[test]
fn fees_round_up_retain_custody_and_limits_bound_net_output() {
    let fixture = fixture();
    for scenario in fixture.scenarios.iter().take(5) {
        let gross = state(&fixture, scenario);
        let tokens = tokens(&gross);
        for bps in [1u16, 25, 1000] {
            let mut net = gross.clone();
            net.fees[1] = Some(bps);
            for case in &scenario.cases {
                let input = BigUint::from_str(&case.input).unwrap();
                let expected =
                    BigUint::from_str(&case.output).unwrap() * (10_000 - bps) / 10_000u16;
                let direction = usize::from(!case.sell_base);
                let result =
                    net.get_amount_out(input.clone(), &tokens[direction], &tokens[1 - direction]);
                if expected == BigUint::ZERO {
                    assert!(result.is_err());
                    continue;
                }
                let result = result.unwrap();
                assert_eq!(result.amount, expected);
                let next = result
                    .new_state
                    .as_any()
                    .downcast_ref::<BaibaiState>()
                    .unwrap();
                let gross_result = gross
                    .get_amount_out(input.clone(), &tokens[direction], &tokens[1 - direction])
                    .unwrap();
                let gross_next = gross_result
                    .new_state
                    .as_any()
                    .downcast_ref::<BaibaiState>()
                    .unwrap();
                assert_eq!(next.words, gross_next.words);
                assert_eq!(
                    big(next.balances[1 - direction]),
                    big(net.balances[1 - direction]) - &expected
                );
                // Only net output must fit custody, even when the curve's gross output does not.
                let mut limited = net.clone();
                limited.balances[1 - direction] = super::math::uint(&expected).unwrap();
                assert_eq!(
                    limited
                        .get_amount_out(input, &tokens[direction], &tokens[1 - direction])
                        .unwrap()
                        .amount,
                    expected
                );
                let (limit, output) = limited
                    .get_limits(
                        tokens[direction].address.clone(),
                        tokens[1 - direction].address.clone(),
                    )
                    .unwrap();
                assert!(output <= expected);
                if limit != BigUint::ZERO {
                    assert_eq!(
                        limited
                            .get_amount_out(limit, &tokens[direction], &tokens[1 - direction])
                            .unwrap()
                            .amount,
                        output
                    );
                }
            }
        }
    }
}

#[test]
fn upgrade_pause_stays_closed_across_subsequent_updates() {
    let fixture = fixture();
    let mut state = state(&fixture, &fixture.scenarios[0]);
    let tokens = tokens(&state);
    assert!(state.fresh());
    for attrs in [
        HashMap::from([("paused".into(), Bytes::from([1u8]))]),
        // A new implementation's storage is not safe to interpret, even when malformed.
        HashMap::from([("word_0".into(), Bytes::from([255u8; 33]))]),
    ] {
        state
            .delta_transition(
                ProtocolStateDelta {
                    component_id: state.id.clone(),
                    updated_attributes: attrs,
                    deleted_attributes: Default::default(),
                },
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();
        assert!(!state.fresh());
        for direction in 0..2 {
            let (input, output) = (&tokens[direction], &tokens[1 - direction]);
            assert!(state
                .get_amount_out(BigUint::from(1u8), input, output)
                .is_err());
            assert!(state.spot_price(input, output).is_err());
            assert_eq!(
                state
                    .get_limits(input.address.clone(), output.address.clone())
                    .unwrap(),
                (BigUint::ZERO, BigUint::ZERO)
            );
        }
    }
}
