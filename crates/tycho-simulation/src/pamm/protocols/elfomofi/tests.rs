use std::time::{SystemTime, UNIX_EPOCH};

use num_bigint::BigUint;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    Bytes,
};

use super::{
    models::{Book, Level, Snapshot},
    state::ElfomofiState,
};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn token(address: Bytes, decimals: u32) -> Token {
    token_on(Chain::Base, address, decimals)
}
fn token_on(chain: Chain, address: Bytes, decimals: u32) -> Token {
    Token::new(&address, "TEST", decimals, 0, &[], chain, 100)
}
fn snapshot(balance: u64) -> Snapshot {
    let levels = |points: &[(u64, u64)]| {
        points
            .iter()
            .map(|&(input, output)| Level { amount_in: input.into(), amount_out: output.into() })
            .collect()
    };
    Snapshot {
        chain: Chain::Base,
        block_number: 100,
        block_hash: Bytes::from(vec![3; 32]),
        block_timestamp: now(),
        observed_at: now(),
        max_age_seconds: 30,
        book: Book {
            base_token: Bytes::from(vec![1; 20]),
            quote_token: Bytes::from(vec![2; 20]),
            asks: levels(&[(100, 40), (300, 100)]),
            bids: levels(&[(100, 200), (300, 500)]),
            base_balance: balance.into(),
            quote_balance: balance.into(),
        },
    }
}
fn state() -> ElfomofiState {
    ElfomofiState::from_snapshot(snapshot(10_000)).unwrap()
}
fn quote(
    state: &ElfomofiState,
    input: u64,
    reverse: bool,
) -> Result<BigUint, tycho_common::simulation::errors::SimulationError> {
    let book = &state.snapshot().book;
    let (sell, buy) = if reverse {
        (&book.quote_token, &book.base_token)
    } else {
        (&book.base_token, &book.quote_token)
    };
    state
        .get_amount_out(input.into(), &token(sell.clone(), 0), &token(buy.clone(), 0))
        .map(|result| result.amount)
}

#[test]
fn integer_interpolation_rounds_down_without_extrapolation() {
    let state = state();
    for (input, output) in [(0, 0), (50, 100), (100, 200), (101, 201), (200, 350), (300, 500)] {
        assert_eq!(quote(&state, input, false).unwrap(), BigUint::from(output as u64));
    }
    assert_eq!(quote(&state, 200, true).unwrap(), BigUint::from(70u64));
    assert_eq!(quote(&state, 1, true).unwrap(), BigUint::ZERO);
    assert!(quote(&state, 301, false).is_err());
}

#[test]
fn inventory_caps_every_advertised_input_in_both_directions() {
    for balance in [0, 1, 100, 200, 201, 349, 500, 10_000] {
        let state = ElfomofiState::from_snapshot(snapshot(balance)).unwrap();
        let book = &state.snapshot().book;
        for (sell, buy) in
            [(&book.base_token, &book.quote_token), (&book.quote_token, &book.base_token)]
        {
            let (input, output) = state
                .get_limits(sell.clone(), buy.clone())
                .unwrap();
            assert!(output <= balance.into());
            assert_eq!(
                state
                    .get_amount_out(input.clone(), &token(sell.clone(), 0), &token(buy.clone(), 0))
                    .unwrap()
                    .amount,
                output
            );
            assert!(state
                .get_amount_out(input + 1u64, &token(sell.clone(), 0), &token(buy.clone(), 0))
                .is_err());
        }
    }
}

#[test]
fn spot_price_uses_buy_direction_and_decimals() {
    let state = state();
    let book = &state.snapshot().book;
    assert_eq!(
        state
            .spot_price(&token(book.base_token.clone(), 18), &token(book.quote_token.clone(), 6))
            .unwrap(),
        2.5e12
    );
    assert_eq!(
        state
            .spot_price(&token(book.quote_token.clone(), 6), &token(book.base_token.clone(), 18))
            .unwrap(),
        0.5e-12
    );
}

#[test]
fn returned_and_serialized_state_support_repeated_bid_and_ask_quotes() {
    let initial = state();
    let book = &initial.snapshot().book;
    let base = token(book.base_token.clone(), 0);
    let quote = token(book.quote_token.clone(), 0);
    let bid_limits = initial
        .get_limits(base.address.clone(), quote.address.clone())
        .unwrap();
    let ask_limits = initial
        .get_limits(quote.address.clone(), base.address.clone())
        .unwrap();
    let mut current = initial.clone();
    for _ in 0..2 {
        for (input, reverse, output) in
            [(100u64, false, 200u64), (100, true, 40), (200, false, 350), (200, true, 70)]
        {
            let (sell, buy) = if reverse { (&quote, &base) } else { (&base, &quote) };
            let result = current
                .get_amount_out(input.into(), sell, buy)
                .unwrap();
            assert_eq!(result.amount, BigUint::from(output));
            let next = result
                .new_state
                .as_any()
                .downcast_ref::<ElfomofiState>()
                .unwrap();
            assert_eq!(next.snapshot(), initial.snapshot());
            assert_eq!(
                next.get_limits(base.address.clone(), quote.address.clone())
                    .unwrap(),
                bid_limits
            );
            assert_eq!(
                next.get_limits(quote.address.clone(), base.address.clone())
                    .unwrap(),
                ask_limits
            );
            current = serde_json::from_str(&serde_json::to_string(next).unwrap()).unwrap();
            assert_eq!(current.snapshot(), initial.snapshot());
        }
    }
}

#[test]
fn wall_clock_expiry_also_checks_chain_timestamp_and_future_observations() {
    let mut value = snapshot(10_000);
    value.observed_at = 100;
    value.block_timestamp = 98;
    value.max_age_seconds = 10;
    let state = ElfomofiState::from_snapshot(value).unwrap();
    assert!(!state.is_fresh_at(99));
    assert!(state.is_fresh_at(107));
    assert!(!state.is_fresh_at(108));
    assert!(quote(&state, 100, false).is_err());
    let book = &state.snapshot().book;
    assert_eq!(
        state
            .get_limits(book.base_token.clone(), book.quote_token.clone())
            .unwrap(),
        (BigUint::ZERO, BigUint::ZERO)
    );
}

#[test]
fn malformed_and_nonmonotone_snapshots_are_rejected() {
    let original = snapshot(10_000);
    let mut value = original.clone();
    value.book.bids[1].amount_in = 100u64.into();
    assert!(ElfomofiState::from_snapshot(value).is_err());
    let mut value = original.clone();
    value.book.asks[0].amount_out = BigUint::ZERO;
    assert!(ElfomofiState::from_snapshot(value).is_err());
    let mut value = original.clone();
    value.book.quote_token = value.book.base_token.clone();
    assert!(ElfomofiState::from_snapshot(value).is_err());
    let mut value = original.clone();
    value.block_hash = Bytes::zero(31);
    assert!(ElfomofiState::from_snapshot(value).is_err());
    let mut value = original;
    value.book.base_balance = BigUint::from(1u8) << 256;
    assert!(ElfomofiState::from_snapshot(value).is_err());
}

#[test]
fn amounts_keep_precision_above_u128_and_use_full_uint256_input_domain() {
    let mut value = snapshot(10_000);
    let scale: BigUint = BigUint::from(1u8) << 200;
    value.book.bids = vec![
        Level { amount_in: scale.clone(), amount_out: &scale * 2u64 },
        Level { amount_in: &scale * 2u64, amount_out: &scale * 3u64 },
    ];
    value.book.quote_balance = BigUint::from(1u8) << 255;
    let state = ElfomofiState::from_snapshot(value.clone()).unwrap();
    let book = &state.snapshot().book;
    assert_eq!(
        state
            .get_amount_out(
                &scale + 1u64,
                &token(book.base_token.clone(), 0),
                &token(book.quote_token.clone(), 0)
            )
            .unwrap()
            .amount,
        &scale * 2u64 + 1u64
    );
    let max_input: BigUint = (BigUint::from(1u8) << 256) - 1u64;
    value.book.bids = vec![Level { amount_in: max_input.clone(), amount_out: max_input.clone() }];
    value.book.quote_balance = max_input.clone();
    let state = ElfomofiState::from_snapshot(value.clone()).unwrap();
    let book = &state.snapshot().book;
    assert_eq!(
        state
            .get_limits(book.base_token.clone(), book.quote_token.clone())
            .unwrap()
            .0,
        max_input
    );
    for amount in [BigUint::from(1u8) << 255, max_input.clone()] {
        assert_eq!(
            state
                .get_amount_out(
                    amount.clone(),
                    &token(book.base_token.clone(), 0),
                    &token(book.quote_token.clone(), 0)
                )
                .unwrap()
                .amount,
            amount
        );
    }
    assert!(state
        .get_amount_out(
            &max_input + 1u64,
            &token(book.base_token.clone(), 0),
            &token(book.quote_token.clone(), 0)
        )
        .is_err());
    value.book.bids[0].amount_in = &max_input + 1u64;
    assert!(ElfomofiState::from_snapshot(value).is_err());
}

#[test]
fn empty_books_and_wrong_pairs_have_no_liquidity() {
    let mut value = snapshot(10_000);
    value.book.bids.clear();
    value.book.asks.clear();
    let state = ElfomofiState::from_snapshot(value).unwrap();
    assert!(quote(&state, 1, false).is_err());
    assert!(state
        .get_amount_out(
            1u64.into(),
            &token(Bytes::zero(20), 0),
            &token(
                state
                    .snapshot()
                    .book
                    .quote_token
                    .clone(),
                0
            )
        )
        .is_err());
}

#[test]
fn serialized_state_quotes_locally_without_rpc_configuration() {
    let state = state();
    let json = serde_json::to_string(&state).unwrap();
    assert!(!json.contains("rpc_url"));
    let state: ElfomofiState = serde_json::from_str(&json).unwrap();
    assert!(quote(&state, 100, false).is_ok());
    assert!(!json.contains("client"));
}

#[test]
fn deserialization_rejects_invalid_levels_before_quoting() {
    let mut json = serde_json::to_value(state()).unwrap();
    json["snapshot"]["book"]["bids"][0]["amount_in"] = serde_json::to_value(BigUint::ZERO).unwrap();
    assert!(serde_json::from_value::<ElfomofiState>(json).is_err());
}

#[test]
fn chain_identity_survives_serialization_and_repeated_native_quotes() {
    for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
        let mut snapshot = snapshot(10_000);
        snapshot.chain = chain;
        let state = ElfomofiState::from_snapshot(snapshot).unwrap();
        let book = &state.snapshot().book;
        let base = token_on(chain, book.base_token.clone(), 0);
        let quote = token_on(chain, book.quote_token.clone(), 0);
        let restored: ElfomofiState =
            serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        assert_eq!(restored.snapshot(), state.snapshot());
        for (input, output, sell, buy) in
            [(100u64, 200u64, &base, &quote), (100, 40, &quote, &base)]
        {
            let result = restored
                .get_amount_out(input.into(), sell, buy)
                .unwrap();
            assert_eq!(result.amount, BigUint::from(output));
            let next = result
                .new_state
                .as_any()
                .downcast_ref::<ElfomofiState>()
                .unwrap();
            assert_eq!(next.snapshot(), state.snapshot());
        }
        let wrong = token_on(Chain::Ethereum, base.address.clone(), 0);
        assert!(restored
            .get_amount_out(100u64.into(), &wrong, &quote)
            .is_err());
        assert!(restored
            .spot_price(&wrong, &quote)
            .is_err());
    }
}

#[test]
fn old_snapshots_default_to_base_and_unsupported_chains_are_rejected() {
    let mut json = serde_json::to_value(state()).unwrap();
    json["snapshot"]
        .as_object_mut()
        .unwrap()
        .remove("chain");
    let restored: ElfomofiState = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(restored.snapshot().chain, Chain::Base);
    json["snapshot"]["chain"] = serde_json::to_value(Chain::Ethereum).unwrap();
    assert!(serde_json::from_value::<ElfomofiState>(json).is_err());
}
