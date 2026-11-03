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

fn token(address: &Bytes, decimals: u32) -> Token {
    token_on(Chain::Base, address, decimals)
}
fn token_on(chain: Chain, address: &Bytes, decimals: u32) -> Token {
    Token::new(address, "TEST", decimals, 0, &[], chain, 100)
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
        block_timestamp: 100,
        observed_at: 100,
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
fn from_snapshot(
    snapshot: Snapshot,
) -> Result<ElfomofiState, tycho_common::simulation::errors::SimulationError> {
    let base = token_on(snapshot.chain, &snapshot.book.base_token, 0);
    let quote = token_on(snapshot.chain, &snapshot.book.quote_token, 0);
    ElfomofiState::from_snapshot(snapshot, base, quote)
}
fn state() -> ElfomofiState {
    from_snapshot(snapshot(10_000)).unwrap()
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
        .get_amount_out(input.into(), &token(sell, 0), &token(buy, 0))
        .map(|result| result.amount)
}

#[test]
fn integer_interpolation_rounds_down_without_extrapolation() {
    let state = state();
    for (input, output) in [(0, 0u64), (50, 100), (100, 200), (101, 201), (200, 350), (300, 500)] {
        assert_eq!(quote(&state, input, false).unwrap(), BigUint::from(output));
    }
    assert_eq!(quote(&state, 200, true).unwrap(), BigUint::from(70u64));
    assert_eq!(quote(&state, 1, true).unwrap(), BigUint::ZERO);
    assert!(quote(&state, 301, false).is_err());
}

#[test]
fn inventory_caps_every_advertised_input_in_both_directions() {
    for balance in [0, 1, 100, 200, 201, 349, 500, 10_000] {
        let state = from_snapshot(snapshot(balance)).unwrap();
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
                    .get_amount_out(input.clone(), &token(sell, 0), &token(buy, 0))
                    .unwrap()
                    .amount,
                output
            );
            assert!(state
                .get_amount_out(input + 1u64, &token(sell, 0), &token(buy, 0))
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
            .spot_price(&token(&book.base_token, 18), &token(&book.quote_token, 6))
            .unwrap(),
        2.5e12
    );
    assert_eq!(
        state
            .spot_price(&token(&book.quote_token, 6), &token(&book.base_token, 18))
            .unwrap(),
        0.5e-12
    );
}

#[test]
fn a_fill_disables_the_pair_in_both_directions() {
    use crate::book::component::PairState;

    let initial = state();
    let book = &initial.snapshot().book;
    let base = token(&book.base_token, 0);
    let quote_token = token(&book.quote_token, 0);
    for (sell, buy, output) in [(&base, &quote_token, 200u64), (&quote_token, &base, 40)] {
        let result = initial
            .get_amount_out(100u64.into(), sell, buy)
            .unwrap();
        assert_eq!(result.amount, BigUint::from(output));
        assert!(!result.new_state.eq(&initial));
        let next = result
            .new_state
            .as_any()
            .downcast_ref::<ElfomofiState>()
            .unwrap();
        assert_eq!(next.snapshot(), initial.snapshot());
        assert!(next.directions().is_empty());
        let restored: ElfomofiState =
            serde_json::from_str(&serde_json::to_string(next).unwrap()).unwrap();
        assert!(restored.eq(next));
        for (sell, buy) in [(&base, &quote_token), (&quote_token, &base)] {
            assert!(restored
                .get_amount_out(1u64.into(), sell, buy)
                .is_err());
            assert!(restored.spot_price(sell, buy).is_err());
            assert_eq!(
                restored
                    .get_limits(sell.address.clone(), buy.address.clone())
                    .unwrap(),
                (BigUint::ZERO, BigUint::ZERO)
            );
            let zero = restored
                .get_amount_out(BigUint::ZERO, sell, buy)
                .unwrap();
            assert_eq!(zero.amount, BigUint::ZERO);
            assert!(zero.new_state.eq(&restored));
        }
    }
    assert_eq!(quote(&initial, 100, false).unwrap(), BigUint::from(200u64));
    assert_eq!(quote(&initial, 100, true).unwrap(), BigUint::from(40u64));
    assert!(from_snapshot(initial.snapshot().clone())
        .unwrap()
        .eq(&initial));
    let mut other_pair = initial.snapshot().clone();
    other_pair.book.quote_token = Bytes::from(vec![4; 20]);
    assert_eq!(
        quote(&from_snapshot(other_pair).unwrap(), 100, false).unwrap(),
        BigUint::from(200u64)
    );
}

#[test]
fn zero_input_and_failed_quotes_do_not_spend_a_pair() {
    let state = state();
    let book = &state.snapshot().book;
    let base = token(&book.base_token, 0);
    let quote = token(&book.quote_token, 0);
    for (sell, buy) in [(&base, &quote), (&quote, &base)] {
        let zero = state
            .get_amount_out(BigUint::ZERO, sell, buy)
            .unwrap();
        assert_eq!(zero.amount, BigUint::ZERO);
        assert!(zero.new_state.eq(&state));
        assert!(state
            .get_amount_out(301u64.into(), sell, buy)
            .is_err());
        assert!(state
            .get_amount_out(100u64.into(), sell, buy)
            .is_ok());
    }
}

#[test]
fn old_serialized_snapshots_start_with_an_unused_pair() {
    let initial = state();
    let mut json = serde_json::to_value(&initial).unwrap();
    json.as_object_mut()
        .unwrap()
        .remove("used");
    let restored: ElfomofiState = serde_json::from_value(json.clone()).unwrap();
    assert!(restored.eq(&initial));
    json["used"] = serde_json::json!("invalid");
    assert!(serde_json::from_value::<ElfomofiState>(json).is_err());
}

#[test]
fn snapshot_age_does_not_change_local_simulation() {
    let mut value = snapshot(10_000);
    value.observed_at = 100;
    value.block_timestamp = 98;
    value.max_age_seconds = 10;
    assert!(value.ensure_fresh(99).is_err());
    assert!(value.ensure_fresh(107).is_ok());
    assert!(value.ensure_fresh(108).is_err());
    let state = from_snapshot(value).unwrap();
    assert_eq!(quote(&state, 100, false).unwrap(), BigUint::from(200u64));
    let book = &state.snapshot().book;
    assert_eq!(
        state
            .get_limits(book.base_token.clone(), book.quote_token.clone())
            .unwrap(),
        (BigUint::from(300u64), BigUint::from(500u64))
    );
}

#[test]
fn real_serialized_states_encode_with_a_route_allocated_floor_on_every_chain() {
    use tycho_execution::encoding::{
        evm::swap_encoder::{swap_encoder_registry::SwapEncoderRegistry, ElfomofiSwapPolicy},
        models::{EncodingContext, Swap},
    };

    use super::all_pairs::ElfomofiAllPairsState;
    use crate::book::all_pairs::{all_pairs_book, SignedByVenue, Venue, VenueBook};

    for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
        let mut snapshot = snapshot(10_000);
        snapshot.chain = chain;
        let mut other_pair = snapshot.clone();
        other_pair.book.quote_token = Bytes::from(vec![4; 20]);
        let state = from_snapshot(snapshot).unwrap();
        let book = all_pairs_book::<ElfomofiState, SignedByVenue, ElfomofiAllPairsState>(
            super::PROTOCOL_SYSTEM,
            super::PROTOCOL_TYPE,
            chain,
            vec![
                VenueBook { signer: Venue, state, updated_at: None },
                VenueBook {
                    signer: Venue,
                    state: from_snapshot(other_pair).unwrap(),
                    updated_at: None,
                },
            ],
            (),
        )
        .unwrap();
        assert_eq!(book.component.tokens.len(), 3);
        let sell = book.component.tokens[0].clone();
        let buy = book.component.tokens[1].clone();
        let serialized = serde_json::to_vec(book.state.as_ref()).unwrap();
        let restored: Box<dyn ProtocolSim> = serde_json::from_slice(&serialized).unwrap();
        let quote = restored
            .get_amount_out(100u64.into(), &sell, &buy)
            .unwrap();
        assert_eq!(quote.amount, BigUint::from(200u64));
        let spent = serde_json::to_vec(quote.new_state.as_ref()).unwrap();
        let spent: Box<dyn ProtocolSim> = serde_json::from_slice(&spent).unwrap();
        let swap = Swap::new(book.component, sell.clone(), buy.clone(), BigUint::ZERO)
            .with_protocol_state(restored.into())
            .with_estimated_amount_in(100u64.into())
            .with_user_data(
                ElfomofiSwapPolicy::MinAmountOut(190u64.into())
                    .to_user_data()
                    .unwrap(),
            );
        let registry = SwapEncoderRegistry::new(chain).add_default_encoders(Some(
            serde_json::json!({chain.to_string(): {super::PROTOCOL_SYSTEM: Bytes::from(vec![9; 20]).to_string()}}).to_string()
        )).unwrap();
        let encoder = registry
            .get_encoder(super::PROTOCOL_SYSTEM)
            .unwrap();
        let context = EncodingContext {
            router_address: None,
            group_token_in: sell.address.clone(),
            group_token_out: buy.address.clone(),
            origin: None,
        };
        let data = encoder
            .encode_swap(&swap, &context)
            .unwrap();
        assert_eq!(data.len(), 104);
        assert_eq!(&data[..20], sell.address.as_ref());
        assert_eq!(&data[20..40], buy.address.as_ref());
        assert_eq!(BigUint::from_bytes_be(&data[40..72]), BigUint::from(100u64));
        assert_eq!(BigUint::from_bytes_be(&data[72..104]), BigUint::from(190u64));
        assert!(encoder
            .encode_swap(&swap.with_protocol_state(spent.into()), &context)
            .is_err());
    }
}

#[test]
fn malformed_and_nonmonotone_snapshots_are_rejected() {
    let original = snapshot(10_000);
    let mut value = original.clone();
    value.book.bids[1].amount_in = 100u64.into();
    assert!(from_snapshot(value).is_err());
    let mut value = original.clone();
    value.book.asks[0].amount_out = BigUint::ZERO;
    assert!(from_snapshot(value).is_err());
    let mut value = original.clone();
    value.book.quote_token = value.book.base_token.clone();
    assert!(from_snapshot(value).is_err());
    let mut value = original.clone();
    value.block_hash = Bytes::zero(31);
    assert!(from_snapshot(value).is_err());
    let mut value = original;
    value.book.base_balance = BigUint::from(1u8) << 256;
    assert!(from_snapshot(value).is_err());
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
    let state = from_snapshot(value.clone()).unwrap();
    let book = &state.snapshot().book;
    assert_eq!(
        state
            .get_amount_out(
                &scale + 1u64,
                &token(&book.base_token, 0),
                &token(&book.quote_token, 0)
            )
            .unwrap()
            .amount,
        &scale * 2u64 + 1u64
    );
    let max_input: BigUint = (BigUint::from(1u8) << 256) - 1u64;
    value.book.bids = vec![Level { amount_in: max_input.clone(), amount_out: max_input.clone() }];
    value.book.quote_balance = max_input.clone();
    let state = from_snapshot(value.clone()).unwrap();
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
                    &token(&book.base_token, 0),
                    &token(&book.quote_token, 0)
                )
                .unwrap()
                .amount,
            amount
        );
    }
    assert!(state
        .get_amount_out(
            &max_input + 1u64,
            &token(&book.base_token, 0),
            &token(&book.quote_token, 0)
        )
        .is_err());
    value.book.bids[0].amount_in = &max_input + 1u64;
    assert!(from_snapshot(value).is_err());
}

#[test]
fn public_helper_snapshot_preserves_initial_quote_outputs() {
    let raw = hex::decode(
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../protocols/fixtures/elfomofi/base-52898481.hex"
        ))
        .trim()
        .trim_start_matches("0x"),
    )
    .unwrap();
    let snapshots = super::models::decode_orderbooks(
        Chain::Base,
        &raw,
        52_898_481,
        &Bytes::from(vec![1; 32]),
        1_792_586_309,
        1_792_586_309,
        2,
    )
    .unwrap();
    let base = "0x4200000000000000000000000000000000000006"
        .parse::<Bytes>()
        .unwrap();
    let quote = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
        .parse::<Bytes>()
        .unwrap();
    let snapshot = snapshots
        .into_iter()
        .find(|snapshot| snapshot.book.base_token == base && snapshot.book.quote_token == quote)
        .unwrap();
    let state = from_snapshot(snapshot).unwrap();
    let base = token(&base, 18);
    let quote = token(&quote, 6);
    let first = state
        .get_amount_out(3_000_000_000_000_000_000u64.into(), &base, &quote)
        .unwrap();
    assert_eq!(first.amount, BigUint::from(7_403_192_704u64));
    assert!(first
        .new_state
        .get_amount_out(6_000_000_000_000_000_000u64.into(), &base, &quote)
        .is_err());
    assert_eq!(
        state
            .get_amount_out(9_000_000_000_000_000_000u64.into(), &base, &quote)
            .unwrap()
            .amount,
        BigUint::from(20_667_066_592u64)
    );
}

#[test]
fn empty_books_and_wrong_pairs_have_no_liquidity() {
    let mut value = snapshot(10_000);
    value.book.bids.clear();
    value.book.asks.clear();
    let state = from_snapshot(value).unwrap();
    assert!(quote(&state, 1, false).is_err());
    assert!(state
        .get_amount_out(
            1u64.into(),
            &token(&Bytes::zero(20), 0),
            &token(&state.snapshot().book.quote_token, 0)
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
        let state = from_snapshot(snapshot).unwrap();
        let book = &state.snapshot().book;
        let base = token_on(chain, &book.base_token, 0);
        let quote = token_on(chain, &book.quote_token, 0);
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
            assert!(!next.eq(&restored));
            assert!(next
                .get_amount_out(100u64.into(), sell, buy)
                .is_err());
        }
        let wrong = token_on(Chain::Ethereum, &base.address, 0);
        assert!(restored
            .get_amount_out(100u64.into(), &wrong, &quote)
            .is_err());
        assert!(restored
            .spot_price(&wrong, &quote)
            .is_err());
    }
}

#[test]
fn missing_and_unsupported_chains_are_rejected() {
    let mut json = serde_json::to_value(state()).unwrap();
    json["snapshot"]
        .as_object_mut()
        .unwrap()
        .remove("chain");
    assert!(serde_json::from_value::<ElfomofiState>(json.clone()).is_err());
    json["snapshot"]["chain"] = serde_json::to_value(Chain::Ethereum).unwrap();
    assert!(serde_json::from_value::<ElfomofiState>(json).is_err());
}

#[test]
fn pair_directions_follow_servable_book_sides() {
    use crate::book::component::PairState;
    let mut snapshot = snapshot(10_000);
    snapshot.book.asks.clear();
    let state = from_snapshot(snapshot.clone()).unwrap();
    assert_eq!(
        state.directions(),
        vec![(snapshot.book.base_token.clone(), snapshot.book.quote_token.clone())]
    );
    snapshot.book.quote_balance = BigUint::ZERO;
    assert!(from_snapshot(snapshot)
        .unwrap()
        .directions()
        .is_empty());
}

#[test]
fn deserialization_rejects_token_metadata_from_another_pair_or_chain() {
    let state = state();
    for pointer in ["/base_token/address", "/quote_token/address"] {
        let mut json = serde_json::to_value(&state).unwrap();
        *json.pointer_mut(pointer).unwrap() =
            serde_json::to_value(Bytes::from(vec![7; 20])).unwrap();
        assert!(serde_json::from_value::<ElfomofiState>(json).is_err());
    }
    let mut json = serde_json::to_value(&state).unwrap();
    json["base_token"]["chain"] = serde_json::to_value(Chain::Bsc).unwrap();
    assert!(serde_json::from_value::<ElfomofiState>(json).is_err());
}
