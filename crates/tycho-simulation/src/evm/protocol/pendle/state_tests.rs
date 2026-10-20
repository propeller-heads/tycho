//! Tests for `PendleState`, in their own file only because the module they cover is long.
//!
//! The fixtures are the brief's reference market and two SY shapes on opposite sides of the
//! decimal axis. Nothing here re-checks the ported math — that is pinned against the contract in
//! `math/` — so these cover the layer above it: edge routing, expiry, limits, and units.

use std::str::FromStr;

use super::*;

fn token(address: &str, decimals: u32) -> Token {
    Token::new(
        &Bytes::from_str(address).unwrap(),
        "TKN",
        decimals,
        0,
        &[Some(10_000)],
        tycho_common::models::Chain::Ethereum,
        100,
    )
}

// The reference market's three tokens, as `readTokens()` on `0x34280882...` returns them. Copied
// from a chain read rather than from the brief's truncated addresses, so the set is checkable.
const SY: &str = "0xcbc72d92b2dc8187414f6734718563898740c0bc";
const PT: &str = "0xb253eff1104802b97ac7e3ac9fdd73aece295a2c";
const YT: &str = "0x04b7fa1e727d7290d6e24fa9b426d0c940283a95";
const WSTETH: &str = "0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0";
const STETH: &str = "0xae7ab96520de3a18e5e111b5eaab095312d7fe84";

// Placeholders for the decimal-axis fixture below. Deliberately not real addresses: that fixture
// tests a shape, and borrowing a live token's address would assert decimals it does not have.
const SIX_DECIMAL_SY: &str = "0x2222222222222222222222222222222222222222";
const SIX_DECIMAL_TOKEN: &str = "0x3333333333333333333333333333333333333333";

fn s(value: &str) -> I256 {
    I256::from_dec_str(value).unwrap()
}

fn u(value: &str) -> U256 {
    U256::from_str_radix(value, 10).unwrap()
}

/// The brief's reference market.
fn wsteth_market(rate_sampled_at: u64) -> PendleState {
    PendleState::Market(PendleMarketState {
        market: MarketState {
            total_pt: s("83658000000000000000"),
            total_sy: s("1429719000000000000000"),
            scalar_root: s("86364560000000000000"),
            expiry: 1_830_124_800,
            ln_fee_rate_root: u("499875041000000"),
            reserve_fee_percent: u("80"),
            last_ln_implied_rate: u("20211000000000000"),
        },
        py_index: u("1241884000000000000"),
        rate_sampled_at,
        head_timestamp: rate_sampled_at,
        sy_address: Bytes::from_str(SY).unwrap(),
        pt_address: Bytes::from_str(PT).unwrap(),
        yt_address: Bytes::from_str(YT).unwrap(),
        sy_paused: false,
        execution_timestamp: 0,
        fee_unread: false,
        // The reference wstETH market comes from the original factory.
        from_original_factory: true,
        memo: Default::default(),
    })
}

/// SY 18 decimals, accounting asset 18: wstETH one-to-one, stETH at the index.
fn wsteth_sy() -> PendleState {
    PendleState::Sy(PendleSyState {
        sy_address: Bytes::from_str(SY).unwrap(),
        exchange_rate: u("1241884000000000000"),
        rate_sampled_at: 1_700_000_000,
        head_timestamp: 1_700_000_000,
        sy_decimals: 18,
        asset_decimals: 18,
        tokens_in: HashMap::from([
            (Bytes::from_str(WSTETH).unwrap(), TokenClass::OneToOne),
            (Bytes::from_str(STETH).unwrap(), TokenClass::IndexRate),
        ]),
        tokens_out: HashMap::from([
            (Bytes::from_str(WSTETH).unwrap(), TokenClass::OneToOne),
            (Bytes::from_str(STETH).unwrap(), TokenClass::IndexRate),
        ]),
        // `totalSupply()` at block 25 800 000.
        total_supply: u("3248104473502927520625"),
        paused: false,
        token_decimals: HashMap::from([
            (Bytes::from_str(WSTETH).unwrap(), 18),
            (Bytes::from_str(STETH).unwrap(), 18),
        ]),
        // Uncapped, and backed by wstETH it holds in full.
        supply_cap: None,
        yield_token: Some(Bytes::from_str(WSTETH).unwrap()),
        yield_token_held: Some(u("3248104473502927520625")),
        holding_backs_supply: true,
        deposit_steps: HashMap::new(),
        entry_fee_bps: 0,
        entry_fee_from: 0,
        execution_timestamp: 0,
    })
}

/// The decimal axis: SY 18, accounting asset 6, the index carrying the 1e12 gap.
///
/// The index is a real reUSD-market reading; the addresses are placeholders, so this asserts the
/// shape without claiming to be any particular wrapper.
fn six_decimal_asset_sy() -> PendleState {
    PendleState::Sy(PendleSyState {
        sy_address: Bytes::from_str(SIX_DECIMAL_SY).unwrap(),
        exchange_rate: u("1095830"),
        rate_sampled_at: 1_700_000_000,
        head_timestamp: 1_700_000_000,
        sy_decimals: 18,
        asset_decimals: 6,
        tokens_in: HashMap::from([(
            Bytes::from_str(SIX_DECIMAL_TOKEN).unwrap(),
            TokenClass::IndexRate,
        )]),
        tokens_out: HashMap::from([(
            Bytes::from_str(SIX_DECIMAL_TOKEN).unwrap(),
            TokenClass::IndexRate,
        )]),
        total_supply: u("1000000000000000000000000"),
        paused: false,
        token_decimals: HashMap::from([(Bytes::from_str(SIX_DECIMAL_TOKEN).unwrap(), 6)]),
        supply_cap: None,
        yield_token: None,
        yield_token_held: None,
        holding_backs_supply: false,
        deposit_steps: HashMap::new(),
        entry_fee_bps: 0,
        entry_fee_from: 0,
        execution_timestamp: 0,
    })
}

/// Moves the chain head past the block the rates were read at, leaving the state itself intact.
fn head_moved_on(state: PendleState) -> PendleState {
    match state {
        PendleState::Market(mut market) => {
            market.head_timestamp = market.rate_sampled_at + 12;
            PendleState::Market(market)
        }
        PendleState::Sy(mut sy) => {
            sy.head_timestamp = sy.rate_sampled_at + 12;
            PendleState::Sy(sy)
        }
    }
}

/// A market whose rate predates the head cannot be quoted exactly, so it is not quoted at all.
///
/// Recoverable, unlike expiry: the next refresh re-pairs the rate with the head.
#[test]
fn a_market_whose_rate_predates_the_head_refuses_to_quote() {
    let state = head_moved_on(wsteth_market(1_700_000_000));
    let amount = BigUint::from(1_000_000_000_000_000_000u64);

    let error = state
        .get_amount_out(amount, &token(SY, 18), &token(PT, 18))
        .expect_err("a stale rate must not be quoted");
    let SimulationError::RecoverableError(message) = error else {
        panic!("a stale rate is recoverable, not fatal")
    };
    assert!(message.contains("1700000000"), "{message}");
    assert!(message.contains("1700000012"), "{message}");
}

/// The wrapper is held to the same standard: its rate dates the conversion.
#[test]
fn a_wrapper_whose_rate_predates_the_head_refuses_to_quote() {
    let state = head_moved_on(wsteth_sy());
    let amount = BigUint::from(1_000_000_000_000_000_000u64);

    let error = state
        .get_amount_out(amount, &token(STETH, 18), &token(SY, 18))
        .expect_err("a stale rate must not be quoted");
    assert!(matches!(error, SimulationError::RecoverableError(_)), "{error:?}");
}

/// Reported as no depth rather than an error, so a router skips it the way it skips an expired
/// market instead of treating a routine gap as a failure.
#[test]
fn a_market_that_cannot_quote_exactly_reports_no_depth() {
    let state = head_moved_on(wsteth_market(1_700_000_000));
    let limits = state
        .get_limits(Bytes::from_str(SY).unwrap(), Bytes::from_str(PT).unwrap())
        .unwrap();
    assert_eq!(limits, (BigUint::from(0u32), BigUint::from(0u32)));
}

/// A head *behind* the sample is not staleness — a snapshot can carry a reading newer than the
/// header it decoded against — and must not refuse.
#[test]
fn a_head_behind_the_sample_still_quotes() {
    let PendleState::Market(mut market) = wsteth_market(1_700_000_000) else { unreachable!() };
    market.head_timestamp = market.rate_sampled_at - 12;
    let state = PendleState::Market(market);
    state
        .get_amount_out(BigUint::from(1_000_000_000_000_000_000u64), &token(SY, 18), &token(PT, 18))
        .expect("a sample newer than the head is still exact");
}

/// The brief's central property: the same state at two timestamps quotes differently, because the
/// scalar, the anchor and the fee all move with time.
#[test]
fn the_same_state_quotes_differently_at_two_timestamps() {
    let pt = token(PT, 18);
    let sy = token(SY, 18);
    let amount = BigUint::from(1_000_000_000_000_000_000u64);

    let early = wsteth_market(1_700_000_000)
        .get_amount_out(amount.clone(), &pt, &sy)
        .unwrap();
    let late = wsteth_market(1_820_000_000)
        .get_amount_out(amount, &pt, &sy)
        .unwrap();

    assert_ne!(early.amount, late.amount);
    // PT is worth more as expiry nears, so selling it returns more SY.
    assert!(late.amount > early.amount);
}

/// Every market edge refuses once the market is dead, and says why.
#[test]
fn an_expired_market_quotes_nothing() {
    let state = wsteth_market(1_830_124_800);
    let amount = BigUint::from(1_000_000_000_000_000_000u64);

    for (a, b) in [(PT, SY), (SY, PT), (YT, SY), (SY, YT)] {
        let error = state
            .get_amount_out(amount.clone(), &token(a, 18), &token(b, 18))
            .expect_err("an expired market must not quote");
        let SimulationError::FatalError(message) = error else {
            panic!("expiry should be fatal, not retryable")
        };
        assert!(message.contains("expired"), "{message}");
    }
}

/// A market whose rate stopped resolving before it expired is still expired.
///
/// The rate's own clock froze below `expiry` and would say the market is alive forever. The head
/// is what knows better, and expiry is the verdict that gets reported — dead is not a gap the
/// next refresh closes.
#[test]
fn a_market_the_head_outlived_is_expired_even_with_a_frozen_rate() {
    let PendleState::Market(mut market) = wsteth_market(1_830_124_788) else { unreachable!() };
    market.head_timestamp = 1_830_124_800;
    let state = PendleState::Market(market);

    let error = state
        .get_amount_out(BigUint::from(1_000_000_000_000_000_000u64), &token(SY, 18), &token(PT, 18))
        .expect_err("an expired market must not quote");
    let SimulationError::FatalError(message) = error else {
        panic!("expiry should be fatal, not retryable")
    };
    assert!(message.contains("expired"), "{message}");

    let limits = state
        .get_limits(Bytes::from_str(SY).unwrap(), Bytes::from_str(PT).unwrap())
        .unwrap();
    assert_eq!(limits, (BigUint::from(0u32), BigUint::from(0u32)));
}

/// And reports no depth rather than a stale bound.
#[test]
fn an_expired_market_has_no_depth() {
    let state = wsteth_market(1_830_124_800);
    let limits = state
        .get_limits(Bytes::from_str(SY).unwrap(), Bytes::from_str(PT).unwrap())
        .unwrap();
    assert_eq!(limits, (BigUint::from(0u32), BigUint::from(0u32)));
}

/// All four market edges quote, and the YT legs are not the PT legs.
#[test]
fn all_four_market_edges_quote() {
    let state = wsteth_market(1_700_000_000);
    let amount = BigUint::from(1_000_000_000_000_000_000u64);

    let pt_to_sy = state
        .get_amount_out(amount.clone(), &token(PT, 18), &token(SY, 18))
        .unwrap();
    let sy_to_pt = state
        .get_amount_out(amount.clone(), &token(SY, 18), &token(PT, 18))
        .unwrap();
    let yt_to_sy = state
        .get_amount_out(amount.clone(), &token(YT, 18), &token(SY, 18))
        .unwrap();
    let sy_to_yt = state
        .get_amount_out(amount, &token(SY, 18), &token(YT, 18))
        .unwrap();

    for result in [&pt_to_sy, &sy_to_pt, &yt_to_sy, &sy_to_yt] {
        assert!(result.amount > BigUint::from(0u32));
    }
    // YT is the levered leg: one SY buys far more YT than PT.
    assert!(sy_to_yt.amount > sy_to_pt.amount);
    // And a whole YT is worth far less than a whole PT.
    assert!(yt_to_sy.amount < pt_to_sy.amount);
}

/// The successor state a quote returns, downcast back to the concrete type.
fn quote_after(
    state: &PendleState,
    amount: BigUint,
    token_in: &Token,
    token_out: &Token,
) -> PendleState {
    let result = state
        .get_amount_out(amount, token_in, token_out)
        .expect("the edge should quote");
    result
        .new_state
        .as_any()
        .downcast_ref::<PendleState>()
        .expect("a Pendle quote returns a Pendle state")
        .clone()
}

fn market_of(state: &PendleState) -> &MarketState {
    let PendleState::Market(market) = state else { panic!("expected a market state") };
    &market.market
}

/// Every market edge moves the reserves it trades against, in the direction the trade took them.
///
/// The two YT legs are flash-swaps against the *same* reserves as the PT legs, and they move them
/// the opposite way from the leg they are usually mistaken for: selling YT borrows PT out of the
/// market, which is the same PT flow as buying it.
#[test]
fn every_market_edge_advances_the_reserves() {
    let state = wsteth_market(1_700_000_000);
    let before = market_of(&state).clone();
    let amount = BigUint::from(1_000_000_000_000_000_000u64);

    // (token in, token out, does the market end up holding more PT?)
    let edges = [(PT, SY, true), (SY, PT, false), (YT, SY, false), (SY, YT, true)];

    for (token_in, token_out, pt_arrives) in edges {
        let after =
            quote_after(&state, amount.clone(), &token(token_in, 18), &token(token_out, 18));
        let after = market_of(&after);

        assert_ne!(after, &before, "{token_in}->{token_out} left the reserves untouched");
        if pt_arrives {
            assert!(after.total_pt > before.total_pt, "{token_in}->{token_out} PT");
            assert!(after.total_sy < before.total_sy, "{token_in}->{token_out} SY");
        } else {
            assert!(after.total_pt < before.total_pt, "{token_in}->{token_out} PT");
            assert!(after.total_sy > before.total_sy, "{token_in}->{token_out} SY");
        }
        assert_ne!(
            after.last_ln_implied_rate, before.last_ln_implied_rate,
            "{token_in}->{token_out} left the implied rate stored by the last trade"
        );
    }
}

/// What the successor state is *for*: the second trade of the same size is priced at the depth the
/// first one left, so a caller sizing a trade sees impact instead of a flat curve.
#[test]
fn a_second_quote_against_the_successor_shows_the_impact() {
    let state = wsteth_market(1_700_000_000);
    let sy = token(SY, 18);
    let pt = token(PT, 18);
    let amount = BigUint::from(10_000_000_000_000_000_000u64);

    let first = state
        .get_amount_out(amount.clone(), &sy, &pt)
        .unwrap();
    let after = quote_after(&state, amount.clone(), &sy, &pt);
    let second = after
        .get_amount_out(amount, &sy, &pt)
        .unwrap();

    assert!(
        second.amount < first.amount,
        "the same SY bought as much PT twice: {} then {}",
        first.amount,
        second.amount
    );
}

/// A swap moves reserves; it does not advance the block or re-read the PY index. The successor
/// therefore answers for the same moment, and is still quotable.
#[test]
fn a_swap_leaves_the_clocks_and_the_rate_alone() {
    let state = wsteth_market(1_700_000_000);
    let PendleState::Market(before) = &state else { unreachable!() };
    let after = quote_after(
        &state,
        BigUint::from(1_000_000_000_000_000_000u64),
        &token(PT, 18),
        &token(SY, 18),
    );
    let PendleState::Market(after) = &after else { unreachable!() };

    assert_eq!(after.py_index, before.py_index);
    assert_eq!(after.rate_sampled_at, before.rate_sampled_at);
    assert_eq!(after.head_timestamp, before.head_timestamp);
    assert_eq!(after.sy_address, before.sy_address);
    assert_eq!(after.market.expiry, before.market.expiry);
    assert_eq!(after.market.scalar_root, before.market.scalar_root);
}

/// The reserves the router would swap against are the component's, not the quote's: a quote must
/// leave the state it was asked of exactly as it found it.
#[test]
fn quoting_does_not_move_the_state_it_was_asked_of() {
    let state = wsteth_market(1_700_000_000);
    let before = state.clone();
    for (token_in, token_out) in [(PT, SY), (SY, PT), (YT, SY), (SY, YT)] {
        state
            .get_amount_out(
                BigUint::from(1_000_000_000_000_000_000u64),
                &token(token_in, 18),
                &token(token_out, 18),
            )
            .unwrap();
    }
    assert_eq!(state, before);
}

/// The successor must exist at the sizes `get_limits` reports, not only at small ones: the state
/// write re-derives the implied rate at the *new* reserves, which is where the proportion cap
/// binds, so a limit that quotes but cannot advance would be a limit no router could take.
#[test]
fn every_edge_advances_at_its_reported_limit() {
    let state = wsteth_market(1_700_000_000);
    let before = market_of(&state).clone();

    for (token_in, token_out) in [(PT, SY), (SY, PT), (YT, SY), (SY, YT)] {
        let (max_in, max_out) = state
            .get_limits(Bytes::from_str(token_in).unwrap(), Bytes::from_str(token_out).unwrap())
            .unwrap();
        let result = state
            .get_amount_out(max_in, &token(token_in, 18), &token(token_out, 18))
            .unwrap_or_else(|e| panic!("{token_in}->{token_out} at its own limit: {e}"));
        assert_eq!(result.amount, max_out, "{token_in}->{token_out}");

        let after = result
            .new_state
            .as_any()
            .downcast_ref::<PendleState>()
            .expect("a Pendle quote returns a Pendle state");
        assert_ne!(market_of(after), &before, "{token_in}->{token_out} at the limit");
    }
}

/// A redemption burns the SY it takes and a deposit mints the SY it returns, so the next quote
/// is bounded by the supply that is left.
#[test]
fn a_redemption_burns_supply_and_a_deposit_mints_it() {
    let state = wsteth_sy();
    let wsteth = Bytes::from_str(WSTETH).unwrap();
    let PendleState::Sy(before) = &state else { unreachable!() };
    let supply = before.total_supply;

    let amount = u("100000000000000000");
    let redeemed = quote_after(&state, u256_to_biguint(amount), &token(SY, 18), &token(WSTETH, 18));
    let PendleState::Sy(redeemed) = &redeemed else { unreachable!() };
    assert_eq!(redeemed.total_supply, supply - amount);
    let (max_in, _) = PendleState::Sy(redeemed.clone())
        .get_limits(Bytes::from_str(SY).unwrap(), wsteth.clone())
        .unwrap();
    assert_eq!(max_in, u256_to_biguint(redeemed.total_supply));

    let deposited = quote_after(&state, u256_to_biguint(amount), &token(STETH, 18), &token(SY, 18));
    let PendleState::Sy(deposited) = &deposited else { unreachable!() };
    let minted = wsteth_sy()
        .get_amount_out(u256_to_biguint(amount), &token(STETH, 18), &token(SY, 18))
        .unwrap()
        .amount;
    assert_eq!(u256_to_biguint(deposited.total_supply), u256_to_biguint(supply) + minted);
}

/// A paused SY refuses every edge and reports no depth, whatever its class.
#[test]
fn a_paused_sy_quotes_nothing() {
    let PendleState::Sy(mut inner) = wsteth_sy() else { unreachable!() };
    inner.paused = true;
    let state = PendleState::Sy(inner);
    for token_out in [WSTETH, STETH] {
        let error = state
            .get_amount_out(BigUint::from(1_000_000u64), &token(SY, 18), &token(token_out, 18))
            .unwrap_err();
        assert!(matches!(error, SimulationError::RecoverableError(_)), "{error:?}");
        assert_eq!(
            state
                .get_limits(Bytes::from_str(SY).unwrap(), Bytes::from_str(token_out).unwrap())
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
    }
}

/// A `one_to_one` edge never reads the rate, so a rate the chain has moved past refuses only the
/// `index_rate` edges.
#[test]
fn a_stale_rate_refuses_only_the_rate_converted_edges() {
    let state = head_moved_on(wsteth_sy());
    let one = BigUint::from(1_000_000_000_000_000_000u64);
    assert_eq!(
        state
            .get_amount_out(one.clone(), &token(WSTETH, 18), &token(SY, 18))
            .unwrap()
            .amount,
        one
    );
    assert!(state
        .get_amount_out(one, &token(STETH, 18), &token(SY, 18))
        .is_err());
    let (max_in, _) = state
        .get_limits(Bytes::from_str(SY).unwrap(), Bytes::from_str(STETH).unwrap())
        .unwrap();
    assert_eq!(max_in, BigUint::ZERO);
}

/// An upgrade re-classifies tokens: an update sets a class, a deletion removes it, and supply and
/// the pause flag follow their attributes.
#[test]
fn a_delta_re_classifies_and_moves_supply_and_pause() {
    let mut state = wsteth_sy();
    let steth = STETH.trim_start_matches("0x");
    let delta = ProtocolStateDelta {
        component_id: SY.to_string(),
        updated_attributes: HashMap::from([
            (format!("token_in_class_0x{steth}"), Bytes::from(b"one_to_one".to_vec())),
            ("sy_total_supply".to_string(), Bytes::from(vec![0x01, 0x00])),
            ("paused".to_string(), Bytes::from(vec![1])),
        ]),
        deleted_attributes: [format!("token_out_class_0x{steth}")].into(),
    };
    state
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .unwrap();
    let PendleState::Sy(state) = state else { unreachable!() };
    let steth = Bytes::from_str(STETH).unwrap();
    assert_eq!(state.tokens_in[&steth], TokenClass::OneToOne);
    assert!(!state.tokens_out.contains_key(&steth));
    assert_eq!(state.total_supply, U256::from(256u32));
    assert!(state.paused);
}

/// A paused SY reverts every transfer, so a market priced against it refuses every edge, reports
/// no depth and has no spot price, until an `Unpaused` delta clears the flag.
#[test]
fn a_market_refuses_every_edge_while_its_sy_is_paused() {
    let edges = [(SY, PT), (PT, SY), (SY, YT), (YT, SY)];
    let mut state = wsteth_market(1_700_000_000);
    let set_sy_paused = |state: &mut PendleState, paused: u8| {
        let delta = ProtocolStateDelta {
            component_id: "0x34280882267ffa6383b363e278b027be083bbe3b".to_string(),
            updated_attributes: HashMap::from([(
                "sy_paused".to_string(),
                Bytes::from(vec![paused]),
            )]),
            deleted_attributes: Default::default(),
        };
        state
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();
    };

    set_sy_paused(&mut state, 1);
    for (from, to) in edges {
        let error = state
            .get_amount_out(BigUint::from(10u64.pow(18)), &token(from, 18), &token(to, 18))
            .unwrap_err();
        assert!(matches!(error, SimulationError::RecoverableError(_)), "{from}->{to}: {error:?}");
        let (max_in, max_out) = state
            .get_limits(Bytes::from_str(from).unwrap(), Bytes::from_str(to).unwrap())
            .unwrap();
        assert_eq!((max_in, max_out), (BigUint::ZERO, BigUint::ZERO), "{from}->{to}");
        assert!(
            state
                .spot_price(&token(from, 18), &token(to, 18))
                .is_err(),
            "{from}->{to}"
        );
    }

    set_sy_paused(&mut state, 0);
    for (from, to) in edges {
        state
            .get_amount_out(BigUint::from(10u64.pow(18)), &token(from, 18), &token(to, 18))
            .unwrap_or_else(|e| panic!("{from}->{to} after unpause: {e:?}"));
    }
}

/// No caller amount reads as a broken state. Up to `u128::MAX` every edge either quotes or says
/// the request is too large; past it the amount exceeds any token supply and is refused before
/// it can overflow the math.
#[test]
fn an_oversized_amount_is_never_a_fatal_error() {
    let market = wsteth_market(1_700_000_000);
    let sy = wsteth_sy();
    let cases = [
        (&market, SY, PT),
        (&market, PT, SY),
        (&market, SY, YT),
        (&market, YT, SY),
        (&sy, WSTETH, SY),
        (&sy, STETH, SY),
        (&sy, SY, WSTETH),
        (&sy, SY, STETH),
    ];
    let at_bound = BigUint::from(u128::MAX);
    for (state, from, to) in cases {
        match state.get_amount_out(at_bound.clone(), &token(from, 18), &token(to, 18)) {
            Ok(_) | Err(SimulationError::InvalidInput(..)) => {}
            Err(error) => panic!("{from}->{to} at u128::MAX: {error:?}"),
        }
        for amount in
            [at_bound.clone() + 1u32, BigUint::from(1u32) << 200, BigUint::from(1u32) << 300]
        {
            let error = state
                .get_amount_out(amount, &token(from, 18), &token(to, 18))
                .unwrap_err();
            assert!(matches!(error, SimulationError::InvalidInput(..)), "{from}->{to}: {error:?}");
        }
    }
}

/// Selling YT borrows its amount in PT. A sale a few wei short of the whole PT reserve leaves a
/// proportion that rounds to zero, which is a trade too large, not a broken state.
#[test]
fn a_yt_sale_that_nearly_empties_the_pt_reserve_is_too_large() {
    let state = wsteth_market(1_700_000_000);
    let PendleState::Market(market) = &state else { unreachable!() };
    let total_pt = BigUint::from_str(&market.market.total_pt.to_string()).unwrap();
    for short_by in [1u32, 10, 100, 1_000, 10_000] {
        let error = state
            .get_amount_out(total_pt.clone() - short_by, &token(YT, 18), &token(SY, 18))
            .unwrap_err();
        assert!(
            matches!(error, SimulationError::InvalidInput(..)),
            "{short_by} wei short of the reserve: {error:?}"
        );
    }
}

/// A market whose closed-form bounds leave the math's domain reports no depth on those edges
/// instead of an error. On a steep curve far from expiry, `calcMaxPtOut`'s exponent passes -41: the
/// router's SY→PT search reverts there too, but selling YT never evaluates it and still trades,
/// so its limit comes from searching for what fills. With a PT reserve of dust, the SY→PT bound's
/// own exchange rate falls below par.
#[test]
fn a_bound_outside_the_math_domain_is_no_depth() {
    let at = |mut market: PendleMarketState, timestamp: u64| {
        market.rate_sampled_at = timestamp;
        market.head_timestamp = timestamp;
        PendleState::Market(market)
    };
    let expiry = 1_830_124_800;
    let steep = at(
        shaped_market(
            "4609000000000000000000",
            "59190000000000000000000",
            "214000000000000000",
            "499875041000000",
            "182000000000000000000",
            "80",
            "1458000000000000000",
        ),
        expiry - 109_114_560,
    );
    let (sy_in, _) = steep
        .get_limits(Bytes::from_str(SY).unwrap(), Bytes::from_str(PT).unwrap())
        .unwrap();
    assert_eq!(sy_in, BigUint::ZERO);
    let (yt_in, sy_out) = steep
        .get_limits(Bytes::from_str(YT).unwrap(), Bytes::from_str(SY).unwrap())
        .unwrap();
    assert!(yt_in > BigUint::ZERO && sy_out > BigUint::ZERO);
    let quote = steep
        .get_amount_out(yt_in, &token(YT, 18), &token(SY, 18))
        .unwrap();
    assert_eq!(quote.amount, sy_out);

    let dust = at(
        shaped_market(
            "14940000000",
            "7797000000000000000000000",
            "20211000000000000",
            "499875041000000",
            "86364560000000000000",
            "80",
            "3640000000000000000",
        ),
        expiry - 3_600,
    );
    for (from, to) in [(SY, PT), (SY, YT), (PT, SY), (YT, SY)] {
        dust.get_limits(Bytes::from_str(from).unwrap(), Bytes::from_str(to).unwrap())
            .unwrap_or_else(|e| panic!("{from}->{to}: {e:?}"));
    }
}

/// A quote taken at the last block before expiry executes in the next one, where the market
/// reverts with `MarketExpired`. Once the execution block reaches expiry the market refuses every
/// edge and reports no depth, and says so once.
#[test]
fn a_market_refuses_quotes_that_would_execute_at_expiry() {
    let expiry = 1_830_124_800;
    let mut state = wsteth_market(expiry - 6);
    let one = BigUint::from(10u64.pow(18));
    fn sy_for_pt(state: &PendleState, one: &BigUint) -> Result<BigUint, SimulationError> {
        state
            .get_amount_out(one.clone(), &token(SY, 18), &token(PT, 18))
            .map(|r| r.amount)
    }
    let before = sy_for_pt(&state, &one).expect("live at the sampled block");

    assert!(
        state.apply_block(&BlockContext::new(1, expiry - 1)),
        "still live, but the quote moves to the execution block"
    );
    let at_execution = sy_for_pt(&state, &one).unwrap();
    assert_ne!(at_execution, before);
    assert_eq!(at_execution, sy_for_pt(&wsteth_market(expiry - 1), &one).unwrap());

    assert!(state.apply_block(&BlockContext::new(2, expiry)), "turning expired must re-emit");
    assert!(!state.apply_block(&BlockContext::new(2, expiry)), "a repeated block changes nothing");
    for (from, to) in [(SY, PT), (PT, SY), (SY, YT), (YT, SY)] {
        assert!(
            state
                .get_amount_out(one.clone(), &token(from, 18), &token(to, 18))
                .is_err(),
            "{from}->{to}"
        );
        let (max_in, max_out) = state
            .get_limits(Bytes::from_str(from).unwrap(), Bytes::from_str(to).unwrap())
            .unwrap();
        assert_eq!((max_in, max_out), (BigUint::ZERO, BigUint::ZERO), "{from}->{to}");
    }
}

/// With an execution block recorded, a market quotes every edge at that block's time with the index
/// as sampled: the same answer as the market sampled at that moment. An execution block at or
/// before the sample leaves the quotes, and the stream, alone.
#[test]
fn a_market_quotes_at_the_execution_block() {
    let sampled = 1_787_273_051;
    let one = BigUint::from(10u64.pow(18));
    let quotes = |state: &PendleState| {
        [(SY, PT), (PT, SY), (SY, YT), (YT, SY)].map(|(from, to)| {
            state
                .get_amount_out(one.clone(), &token(from, 18), &token(to, 18))
                .unwrap()
                .amount
        })
    };
    let mut state = wsteth_market(sampled);
    let at_sample = quotes(&state);

    assert!(!state.apply_block(&BlockContext::new(1, sampled - 12)));
    assert!(!state.apply_block(&BlockContext::new(1, sampled)));
    assert_eq!(quotes(&state), at_sample);

    assert!(state.apply_block(&BlockContext::new(2, sampled + 12)));
    let at_execution = quotes(&state);
    assert_eq!(at_execution, quotes(&wsteth_market(sampled + 12)));
    for (edge, (later, earlier)) in at_execution
        .iter()
        .zip(&at_sample)
        .enumerate()
    {
        assert_ne!(later, earlier, "edge {edge} did not move with the clock");
    }
}

/// An SY has no expiry, so the execution block never changes how it quotes.
#[test]
fn an_sy_ignores_the_execution_block() {
    let mut state = wsteth_sy();
    let before = state.clone();
    assert!(!state.apply_block(&BlockContext::new(1, u64::MAX)));
    assert_eq!(state, before);
}

/// The SY-in and PT-in market limits are the largest input the edge fills, not a bound safely
/// below it: the limit fills, and anything a billionth past it is refused as too large, never as a
/// broken state. The two SY-in edges are exact to the wei. Checked from four years out to a minute
/// before expiry. Selling YT is checked against its output peak instead, below.
#[test]
fn market_limits_are_the_largest_input_that_fills() {
    let expiry = 1_830_124_800u64;
    let states = [
        wsteth_market(1_700_000_000),
        wsteth_market(expiry - 86_400),
        wsteth_market(expiry - 60),
        wsteth_market_at_block_25_800_000(),
    ];
    for state in &states {
        for (from, to) in [(SY, PT), (PT, SY), (SY, YT)] {
            let (token_in, token_out) = (token(from, 18), token(to, 18));
            let quote =
                |amount: &BigUint| state.get_amount_out(amount.clone(), &token_in, &token_out);
            let (max_in, max_out) = state
                .get_limits(token_in.address.clone(), token_out.address.clone())
                .unwrap();
            assert_eq!(quote(&max_in).unwrap().amount, max_out, "{from}->{to}");

            let past = &max_in + &max_in / 1_000_000_000u32 + 1u32;
            let error = quote(&past).unwrap_err();
            assert!(matches!(error, SimulationError::InvalidInput(..)), "{from}->{to}: {error:?}");
            if from == SY {
                assert!(quote(&(&max_in + 1u32)).is_err(), "{from}->{to} is not exact");
            }
        }
    }
}

/// Selling YT borrows PT and buys it back, fee included, so the trader's output peaks well before
/// the market stops the trade, and every YT past the peak returns less. The limit is that peak: it
/// pays something even a day before expiry, and selling a hundredth more or less pays no more.
#[test]
fn the_yt_sale_limit_is_its_output_peak() {
    let expiry = 1_830_124_800u64;
    for state in [
        wsteth_market(1_700_000_000),
        wsteth_market(expiry - 86_400),
        wsteth_market(expiry - 60),
        wsteth_market_at_block_25_800_000(),
    ] {
        let (yt, sy) = (token(YT, 18), token(SY, 18));
        let quote = |amount: &BigUint| {
            state
                .get_amount_out(amount.clone(), &yt, &sy)
                .map(|r| r.amount)
        };
        let (max_in, max_out) = state
            .get_limits(yt.address.clone(), sy.address.clone())
            .unwrap();
        assert!(max_out > BigUint::ZERO, "the peak pays something");
        assert_eq!(quote(&max_in).unwrap(), max_out);
        let step = &max_in / 100u32;
        for neighbour in [&max_in - &step, &max_in + &step] {
            let out = quote(&neighbour).unwrap_or_default();
            assert!(out <= max_out, "{neighbour} pays {out}, more than the peak's {max_out}");
        }
    }
}

/// Close to expiry, selling YT just past what fills fails on repaying the borrowed PT rather than
/// on the market's rate floor. That is the caller's amount being too large, not a broken state.
#[test]
fn selling_yt_just_past_its_depth_is_invalid_input() {
    let expiry = 1_830_124_800u64;
    for state in [wsteth_market(expiry - 86_400), wsteth_market(expiry - 60)] {
        let (yt, sy) = (token(YT, 18), token(SY, 18));
        let fills = |amount: &BigUint| {
            state
                .get_amount_out(amount.clone(), &yt, &sy)
                .is_ok()
        };
        // The limit is the output peak; what fills runs further, up to where repaying fails.
        let (mut fill, _) = state
            .get_limits(yt.address.clone(), sy.address.clone())
            .unwrap();
        let mut refuse = &fill * 2u32;
        assert!(!fills(&refuse));
        while &refuse - &fill > BigUint::from(1u32) {
            let mid = (&fill + &refuse) / 2u32;
            if fills(&mid) {
                fill = mid;
            } else {
                refuse = mid;
            }
        }
        let error = state
            .get_amount_out(refuse, &yt, &sy)
            .unwrap_err();
        assert!(matches!(error, SimulationError::InvalidInput(..)), "{error:?}");
    }
}

fn with_sy(state: PendleState, change: impl FnOnce(&mut PendleSyState)) -> PendleState {
    let PendleState::Sy(mut sy) = state else { panic!("not an SY") };
    change(&mut sy);
    // As the stream would after the change.
    sy.judge_holding();
    PendleState::Sy(sy)
}

/// A capped SY reverts a mint past its cap, so the deposit limit is the largest input whose mint
/// fits in what is left, one wei more is refused, and each quote's mint uses some of it up.
#[test]
fn a_capped_sy_deposits_up_to_its_cap() {
    let headroom = u("1000000000000000000000");
    let state = with_sy(wsteth_sy(), |sy| sy.supply_cap = Some(sy.total_supply + headroom));
    let (sy, steth) = (Bytes::from_str(SY).unwrap(), Bytes::from_str(STETH).unwrap());

    let (max_in, max_out) = state
        .get_limits(steth.clone(), sy.clone())
        .unwrap();
    assert!(max_out <= u256_to_biguint(headroom));
    let filled = state
        .get_amount_out(max_in.clone(), &token(STETH, 18), &token(SY, 18))
        .expect("the limit fills");
    assert_eq!(filled.amount, max_out);
    let error = state
        .get_amount_out(max_in + 1u32, &token(STETH, 18), &token(SY, 18))
        .unwrap_err();
    assert!(matches!(error, SimulationError::InvalidInput(..)), "{error:?}");

    let (after_in, _) = filled
        .new_state
        .get_limits(steth.clone(), sy.clone())
        .unwrap();
    assert!(after_in < BigUint::from(1000u32), "the mint used the cap up: {after_in}");

    let full = with_sy(wsteth_sy(), |sy| sy.supply_cap = Some(sy.total_supply));
    assert_eq!(full.get_limits(steth, sy).unwrap(), (BigUint::ZERO, BigUint::ZERO));
}

#[test]
fn a_delta_sets_and_lifts_the_supply_cap() {
    let mut state = wsteth_sy();
    let set = ProtocolStateDelta {
        component_id: SY.to_string(),
        updated_attributes: HashMap::from([("sy_supply_cap".to_string(), Bytes::from(vec![7]))]),
        deleted_attributes: Default::default(),
    };
    state
        .delta_transition(set, &HashMap::new(), &Balances::default())
        .unwrap();
    let PendleState::Sy(sy) = &state else { unreachable!() };
    assert_eq!(sy.supply_cap, Some(U256::from(7u8)));

    let lift = ProtocolStateDelta {
        component_id: SY.to_string(),
        updated_attributes: HashMap::new(),
        deleted_attributes: ["sy_supply_cap".to_string()].into(),
    };
    state
        .delta_transition(lift, &HashMap::new(), &Balances::default())
        .unwrap();
    let PendleState::Sy(sy) = &state else { unreachable!() };
    assert_eq!(sy.supply_cap, None);
}

/// An SY holding a little less of its yield token than its supply needs (one live SY was 28 wei
/// short) cannot pay a whole-supply redemption to that token. The exit to the yield token is
/// bounded by the holding; the stETH exit, which unwraps instead of paying the holding out, is not.
#[test]
fn a_redemption_to_the_yield_token_is_bounded_by_what_the_sy_holds() {
    let state = with_sy(wsteth_sy(), |sy| {
        sy.yield_token_held = Some(sy.total_supply - U256::from(28u8));
    });
    let PendleState::Sy(inner) = &state else { unreachable!() };
    let (sy, wsteth, steth) = (
        Bytes::from_str(SY).unwrap(),
        Bytes::from_str(WSTETH).unwrap(),
        Bytes::from_str(STETH).unwrap(),
    );

    let (max_in, _) = state
        .get_limits(sy.clone(), wsteth)
        .unwrap();
    assert_eq!(max_in, u256_to_biguint(inner.total_supply - U256::from(28u8)));
    let error = state
        .get_amount_out(max_in + 1u32, &token(SY, 18), &token(WSTETH, 18))
        .unwrap_err();
    assert!(matches!(error, SimulationError::InvalidInput(..)), "{error:?}");

    let (steth_in, _) = state.get_limits(sy, steth).unwrap();
    assert_eq!(steth_in, u256_to_biguint(inner.total_supply));
}

/// An SY that keeps its yield token in a staking contract holds little of it and pays a
/// redemption from there, so a holding far below the payout does not bound it.
#[test]
fn a_holding_far_below_the_payout_leaves_supply_as_the_bound() {
    let state = with_sy(wsteth_sy(), |sy| sy.yield_token_held = Some(U256::from(1_000u32)));
    let PendleState::Sy(inner) = &state else { unreachable!() };
    let (max_in, _) = state
        .get_limits(Bytes::from_str(SY).unwrap(), Bytes::from_str(WSTETH).unwrap())
        .unwrap();
    assert_eq!(max_in, u256_to_biguint(inner.total_supply));
}

/// The holding arrives as a component balance, keyed by the SY's id, and a quote moves it.
#[test]
fn the_yield_token_holding_follows_balances_and_quotes() {
    let mut state = wsteth_sy();
    let balances = Balances {
        component_balances: HashMap::from([(
            SY.to_string(),
            HashMap::from([(Bytes::from_str(WSTETH).unwrap(), Bytes::from(vec![0x0a]))]),
        )]),
        account_balances: HashMap::new(),
    };
    let nothing = ProtocolStateDelta {
        component_id: SY.to_string(),
        updated_attributes: HashMap::new(),
        deleted_attributes: Default::default(),
    };
    state
        .delta_transition(nothing, &HashMap::new(), &balances)
        .unwrap();
    let PendleState::Sy(inner) = &state else { unreachable!() };
    assert_eq!(inner.yield_token_held, Some(U256::from(10u8)));

    let deposited = wsteth_sy()
        .get_amount_out(BigUint::from(5u32), &token(WSTETH, 18), &token(SY, 18))
        .unwrap()
        .new_state;
    let PendleState::Sy(after) = deposited
        .as_any()
        .downcast_ref::<PendleState>()
        .unwrap()
        .clone()
    else {
        unreachable!()
    };
    let PendleState::Sy(before) = wsteth_sy() else { unreachable!() };
    assert_eq!(
        after.yield_token_held,
        before
            .yield_token_held
            .map(|h| h + U256::from(5u8))
    );
}

/// Wrapping or unwrapping through another token moves the SY's yield-token holding too: SY-wstETH
/// wraps deposited stETH into wstETH and unwraps wstETH to pay stETH out, one wstETH per SY.
#[test]
fn a_quote_through_another_token_moves_the_yield_token_holding() {
    let one = BigUint::from(10u64.pow(18));
    let new_sy = |state: &PendleState, from: &str, to: &str| {
        let quoted = state
            .get_amount_out(one.clone(), &token(from, 18), &token(to, 18))
            .unwrap();
        let PendleState::Sy(after) = quoted
            .new_state
            .as_any()
            .downcast_ref::<PendleState>()
            .unwrap()
            .clone()
        else {
            unreachable!()
        };
        (biguint_to_u256(&quoted.amount), after)
    };
    let sy = wsteth_sy();
    let PendleState::Sy(before) = &sy else { unreachable!() };
    let held = before.yield_token_held.unwrap();

    let (minted, deposited) = new_sy(&sy, STETH, SY);
    assert_eq!(deposited.yield_token_held, Some(held + minted));

    let (_, redeemed) = new_sy(&sy, SY, STETH);
    assert_eq!(redeemed.yield_token_held, Some(held - u("1000000000000000000")));
}

/// A market whose implied rate is at or below its fee rate cannot sell PT at par, so the PT bound
/// is negative. The contract reverts there, and so do the quotes, but as a trade too large and with
/// no depth on the edges that take PT out, never as a broken state that drops the market.
#[test]
fn a_market_below_its_fee_rate_reports_no_depth_where_pt_cannot_leave() {
    let PendleState::Market(mut market) = wsteth_market(1_700_000_000) else { unreachable!() };
    market.market.last_ln_implied_rate = u("400000000000000");
    market.market.ln_fee_rate_root = u("5000000000000000");
    let state = PendleState::Market(market);
    for (from, to) in [(SY, PT), (PT, SY), (SY, YT), (YT, SY)] {
        let limits = state
            .get_limits(Bytes::from_str(from).unwrap(), Bytes::from_str(to).unwrap())
            .unwrap_or_else(|e| panic!("{from}->{to} limits: {e:?}"));
        if matches!((from, to), (SY, PT) | (YT, SY)) {
            assert_eq!(limits, (BigUint::ZERO, BigUint::ZERO), "{from}->{to}");
        }
        if let Err(error) =
            state.get_amount_out(BigUint::from(10u64.pow(15)), &token(from, 18), &token(to, 18))
        {
            assert!(matches!(error, SimulationError::InvalidInput(..)), "{from}->{to}: {error:?}");
        }
    }
}

/// Whether the SY pays a redemption from its holding is judged on the stream's state and kept
/// across quotes. A quote that burns most of the supply leaves the shortfall where it was but a
/// larger share of what remains; judging again would fall back to supply and report more than the
/// SY holds.
#[test]
fn a_chained_redemption_stays_bounded_by_the_holding() {
    let state = with_sy(wsteth_sy(), |sy| {
        sy.yield_token_held = Some(sy.total_supply - u("1000000000000000000"));
    });
    let (sy, wsteth) = (Bytes::from_str(SY).unwrap(), Bytes::from_str(WSTETH).unwrap());
    let after = state
        .get_amount_out(
            BigUint::from(3000u32) * BigUint::from(10u64.pow(18)),
            &token(SY, 18),
            &token(WSTETH, 18),
        )
        .unwrap()
        .new_state;
    let PendleState::Sy(chained) = after
        .as_any()
        .downcast_ref::<PendleState>()
        .unwrap()
        .clone()
    else {
        unreachable!()
    };
    let (max_in, _) = after.get_limits(sy, wsteth).unwrap();
    assert_eq!(max_in, u256_to_biguint(chained.yield_token_held.unwrap()));
}

/// A component balance past 256 bits is malformed. It leaves the holding unknown, so supply alone
/// bounds a redemption, rather than panicking.
#[test]
fn a_malformed_balance_leaves_the_holding_unknown() {
    let mut state = wsteth_sy();
    let balances = Balances {
        component_balances: HashMap::from([(
            SY.to_string(),
            HashMap::from([(Bytes::from_str(WSTETH).unwrap(), Bytes::from(vec![0x01; 33]))]),
        )]),
        account_balances: HashMap::new(),
    };
    let nothing = ProtocolStateDelta {
        component_id: SY.to_string(),
        updated_attributes: HashMap::new(),
        deleted_attributes: Default::default(),
    };
    state
        .delta_transition(nothing, &HashMap::new(), &balances)
        .unwrap();
    let PendleState::Sy(inner) = &state else { unreachable!() };
    assert_eq!(inner.yield_token_held, None);
    assert!(!inner.holding_backs_supply);
}

/// A swap executes a block after it is quoted, where the bounds have moved: the clock, and the PY
/// index, which accrues with the yield token (a block of a 3%/yr token is about 1.1e-8). With the
/// execution block recorded, every market limit still fills at that block with the
/// index anywhere up to `INDEX_DRIFT` (1e-6) above the sampled one, on three market shapes, from
/// four years to a minute before expiry. The tests after this one pin each part of how.
#[test]
fn market_limits_still_fill_at_the_execution_block() {
    let PendleState::Market(reference) = wsteth_market(0) else { unreachable!() };
    let PendleState::Market(fork) = wsteth_market_at_block_25_800_000() else { unreachable!() };
    let mut shallow = reference.clone();
    shallow.market.total_pt /= I256::try_from(1_000_000).unwrap();
    shallow.market.total_sy /= I256::try_from(1_000_000).unwrap();
    let expiry = 1_830_124_800u64;
    for (name, market) in [("reference", reference), ("fork", fork), ("shallow", shallow)] {
        for sampled in [
            1_700_000_000u64,
            1_787_273_051,
            expiry - 30 * 86_400,
            expiry - 86_400,
            expiry - 3_600,
            expiry - 60,
        ] {
            let at = |timestamp: u64, py_index: U256| {
                PendleState::Market(PendleMarketState {
                    rate_sampled_at: timestamp,
                    head_timestamp: timestamp,
                    py_index,
                    ..market.clone()
                })
            };
            let mut state = at(sampled, market.py_index);
            state.apply_block(&BlockContext::new(1, sampled + 12));
            for (from, to) in [(SY, PT), (PT, SY), (SY, YT), (YT, SY)] {
                let (max_in, _) = state
                    .get_limits(Bytes::from_str(from).unwrap(), Bytes::from_str(to).unwrap())
                    .unwrap();
                for drift_ppt in (0..=1_000_000u64)
                    .step_by(10_000)
                    .chain([1, 2, 3, 999_999])
                {
                    let moved = market.py_index +
                        market.py_index * U256::from(drift_ppt) /
                            U256::from(1_000_000_000_000u64);
                    at(sampled + 12, moved)
                        .get_amount_out(max_in.clone(), &token(from, 18), &token(to, 18))
                        .unwrap_or_else(|e| {
                            panic!(
                                "{name} {from}->{to} at {sampled}, index +{drift_ppt} ppt: \
                                 {max_in} fails later: {e:?}"
                            )
                        });
                }
            }
        }
    }
}

/// A market of the given shape for the execution-block regressions below, keeping the reference
/// market's tokens and expiry.
fn shaped_market(
    total_pt: &str,
    total_sy: &str,
    last_ln_implied_rate: &str,
    ln_fee_rate_root: &str,
    scalar_root: &str,
    reserve_fee_percent: &str,
    py_index: &str,
) -> PendleMarketState {
    let PendleState::Market(mut market) = wsteth_market(0) else { unreachable!() };
    market.market.total_pt = s(total_pt);
    market.market.total_sy = s(total_sy);
    market.market.last_ln_implied_rate = u(last_ln_implied_rate);
    market.market.ln_fee_rate_root = u(ln_fee_rate_root);
    market.market.scalar_root = s(scalar_root);
    market.market.reserve_fee_percent = u(reserve_fee_percent);
    market.py_index = u(py_index);
    market
}

/// Asserts that `from`→`to`'s limit, taken at `sampled` with the execution block `delay` seconds
/// later, still fills at the execution block with the index risen by every listed amount, in parts
/// per trillion.
fn assert_limit_holds_at_execution(
    market: &PendleMarketState,
    sampled: u64,
    delay: u64,
    edge: (&str, &str),
    drifts_ppt: &[u64],
) {
    let rises = drifts_ppt
        .iter()
        .map(|drift| market.py_index * U256::from(*drift) / U256::from(1_000_000_000_000u64));
    assert_limit_holds_at_rises(market, sampled, delay, edge, rises);
}

/// As [`assert_limit_holds_at_execution`], with the index risen by every listed amount in wei.
fn assert_limit_holds_at_rises(
    market: &PendleMarketState,
    sampled: u64,
    delay: u64,
    (from, to): (&str, &str),
    rises: impl IntoIterator<Item = U256>,
) {
    let at = |timestamp: u64, py_index: U256| {
        PendleState::Market(PendleMarketState {
            rate_sampled_at: timestamp,
            head_timestamp: timestamp,
            py_index,
            ..market.clone()
        })
    };
    let mut state = at(sampled, market.py_index);
    state.apply_block(&BlockContext::new(1, sampled + delay));
    let (max_in, _) = state
        .get_limits(Bytes::from_str(from).unwrap(), Bytes::from_str(to).unwrap())
        .unwrap();
    if max_in == BigUint::ZERO {
        return;
    }
    for rise in rises {
        at(sampled + delay, market.py_index + rise)
            .get_amount_out(max_in.clone(), &token(from, 18), &token(to, 18))
            .unwrap_or_else(|e| panic!("{from}->{to}, index +{rise} wei: {max_in} fails: {e:?}"));
    }
}

/// A YT sale whose output still rises at the fill bound has its peak on the bound, where whether it
/// fills flips with the index: here, PT at the 96% cap 75 s before expiry, a 1e-12 rise in the
/// index puts the exchange rate below par. The limit keeps `EXECUTION_HAIRCUT` from the bound.
#[test]
fn a_yt_sale_whose_peak_is_its_bound_holds_at_execution() {
    let market = shaped_market(
        "48032100000000000000000000000",
        "1967900000000000000000000000",
        "6031030000000000",
        "220000000000000",
        "4630000000000000000",
        "22",
        "1000000000000000000",
    );
    assert_limit_holds_at_execution(&market, 1_830_124_725, 74, (YT, SY), &[1, 2, 3, 1_000_000]);
}

/// Next to the router search's bound, whether a request fills flips with the index, so an SY-in
/// limit there fails at some index between the checked states: here the reference market at a
/// 95.99% PT proportion, a minute before expiry, with the index 1e-6 higher. The limit keeps
/// `EXECUTION_HAIRCUT` from the bound.
#[test]
fn an_sy_in_limit_keeps_clear_of_the_search_bound() {
    let market = shaped_market(
        "1452690582300000000000",
        "60686417700000000000",
        "20211000000000000",
        "499875041000000",
        "86364560000000000000",
        "80",
        "1241884000000000000",
    );
    assert_limit_holds_at_execution(
        &market,
        1_830_124_740,
        12,
        (SY, PT),
        &[1, 500_000, 999_999, 1_000_000],
    );
}

/// A YT sale that pays about as much as the repayment's rounding, a few wei of SY, fills or not
/// depending on the index, so a limit that must hold at execution reports no depth for it. Here,
/// 26 s before expiry, the peak sale of a thin market pays a few wei.
#[test]
fn a_yt_sale_paying_dust_has_no_depth_at_execution() {
    let market = shaped_market(
        "6363000000000000",
        "8993637000000000000",
        "370245000000000",
        "170000000000000",
        "267000000000000000000",
        "48",
        "15580716062387469786442",
    );
    assert_limit_holds_at_execution(
        &market,
        1_830_124_800 - 57,
        31,
        (YT, SY),
        &[50_000, 200_000, 257_739, 297_958],
    );
}

/// With an execution block recorded, a limit gives up a hundredth of its bound rather than sit on
/// it, where rounding makes filling flip with the index. Without one it stays exact.
#[test]
fn an_execution_limit_gives_up_a_hundredth_of_the_bound() {
    let PendleState::Market(market) = wsteth_market(0) else { unreachable!() };
    let sampled = PendleState::Market(PendleMarketState {
        rate_sampled_at: 1_787_273_051,
        head_timestamp: 1_787_273_051,
        ..market
    });
    let mut later = sampled.clone();
    later.apply_block(&BlockContext::new(1, 1_787_273_063));
    for (from, to) in [(SY, PT), (PT, SY), (SY, YT)] {
        let (from, to) = (Bytes::from_str(from).unwrap(), Bytes::from_str(to).unwrap());
        let (exact, _) = sampled
            .get_limits(from.clone(), to.clone())
            .unwrap();
        let (held, _) = later.get_limits(from, to).unwrap();
        let share = u256_to_f64(biguint_to_u256(&held)).unwrap() /
            u256_to_f64(biguint_to_u256(&exact)).unwrap();
        assert!((0.989..0.991).contains(&share), "{share}");
    }
}

/// What a YT sale pays at the sampled block says nothing about what it pays at the execution block:
/// here the peak sale pays 1323 wei of SY 53 minutes before expiry, and none a second before it,
/// where it fills at some indices between the checked states and not at others.
#[test]
fn a_yt_sale_is_dust_by_what_it_pays_at_execution() {
    let market = shaped_market(
        "322760082594980495360",
        "326566283156562",
        "2351298711647608",
        "2127031505991566",
        "3368677611678347264",
        "64",
        "41181032800451869802496",
    );
    let drift = market.py_index / U256::from(1_000_000u64);
    assert_limit_holds_at_rises(
        &market,
        1_830_121_656,
        3_143,
        (YT, SY),
        (0..=1_000u64).map(|k| drift * U256::from(k) / U256::from(1_000u64)),
    );
}

/// An SY-in request whose `eps` window spans a few wei of SY lands or exhausts the search depending
/// on the index: here SY→YT at the 96% proportion cap, where the whole depth is 223 wei, fails at
/// 38 wei above the sampled index. The limit is dust and reports no depth.
#[test]
fn an_sy_in_limit_of_dust_has_no_depth_at_execution() {
    let market = shaped_market(
        "2603547584329881088",
        "589388881545565732",
        "40955002355374024",
        "7182686133247823",
        "180796900341603008512",
        "63",
        "184057000000000000",
    );
    assert_limit_holds_at_rises(
        &market,
        1_830_123_560,
        51,
        (SY, YT),
        (0..2_000u64).map(U256::from),
    );
}

/// Where the implied rate sits on the fee floor, SY→PT's whole depth is a small difference of the
/// reserves, which a wei of rounding in the rate anchor moves by a large share. Here, 2.3 days and
/// 10 hours before expiry, the bound swings by up to a thousandth as the index rises 4e-11 and
/// 1e-9. `EXECUTION_HAIRCUT` covers that much; `ANCHOR_NOISE` covers a market whose whole
/// depth is a smaller difference still.
#[rstest::rstest]
#[case::days_before_expiry(
    [
        "106025040478981328994304",
        "15630542653613299084640",
        "187391880338219",
        "187391879947682",
        "8018284678340328448",
        "2",
        "1480437058153351168",
    ],
    1_829_925_939
)]
#[case::hours_before_expiry(
    [
        "919625788013335543808",
        "1407863053710254718867",
        "141366356533720",
        "141366356077238",
        "5686617717805697024",
        "94",
        "1348676432448877568",
    ],
    1_830_088_382
)]
#[case::bought_down_to_the_floor(
    [
        "2313564182338614136302202",
        "7687924261069278625891",
        "91846189217668539",
        "91846189217662944",
        "1117110417598502912",
        "3",
        "518161302992038167852",
    ],
    1_737_746_720
)]
fn an_sy_in_limit_at_the_fee_floor_holds_at_execution(
    #[case] shape: [&str; 7],
    #[case] sampled: u64,
) {
    let [pt, sy, rate, fee, scalar, reserve_fee, index] = shape;
    let market = shaped_market(pt, sy, rate, fee, scalar, reserve_fee, index);
    let drift = market.py_index / U256::from(1_000_000u64);
    assert_limit_holds_at_rises(
        &market,
        sampled,
        12,
        (SY, PT),
        (0..=1_000u64).map(|k| drift * U256::from(k) / U256::from(1_000u64)),
    );
}

/// A bound the execution block cannot evaluate means the edge does not fill there, not that the
/// quote is broken: here the reference market, bought to its SY→PT limit five times, has its rate
/// so close to the fee floor that 12 s later its closed form would put the exchange rate below
/// par. The sampled block still quotes it.
#[test]
fn a_bound_the_execution_block_cannot_evaluate_is_no_depth() {
    let market = shaped_market(
        "15678057691949226403",
        "1484444859453893147647",
        "499875041000046",
        "499875041000000",
        "86364560000000000000",
        "80",
        "1241884000000000000",
    );
    let at = PendleState::Market(PendleMarketState {
        rate_sampled_at: 1_812_844_788,
        head_timestamp: 1_812_844_788,
        ..market
    });
    let (sy, pt) = (Bytes::from_str(SY).unwrap(), Bytes::from_str(PT).unwrap());
    assert!(at
        .get_limits(sy.clone(), pt.clone())
        .is_ok());
    let mut later = at;
    later.apply_block(&BlockContext::new(1, 1_812_844_800));
    assert_eq!(later.get_limits(sy, pt).unwrap(), (BigUint::ZERO, BigUint::ZERO));
}

/// `get_amount_out` refuses any amount above `u128::MAX`, so a limit past it would not be
/// quotable. With 7.6e12 SY of reserves at 1e8 asset per SY, PT→SY's bound is past it.
#[test]
fn a_limit_past_u128_is_clamped_to_a_quotable_amount() {
    let market = shaped_market(
        "405992000000000000000000000000",
        "7594008000000000000000000000000",
        "12198070000000000",
        "81500000000000000",
        "1070000000000000000000",
        "33",
        "100000000000000000000206358",
    );
    assert_limit_holds_at_execution(&market, 1_830_124_699, 100, (PT, SY), &[0, 1_000_000]);
}

/// The margin alone covers the index drift at an ordinary index, but not where the drift is large
/// in wei: at 1e8 asset per SY, SY→PT's limit at the sampled index exhausts the search once the
/// index has risen 1e-6. The execution state with the risen index catches it.
#[test]
fn a_limit_holds_against_the_full_drift_at_a_large_index() {
    let market = shaped_market(
        "621600000000000000000",
        "799378400000000000000000",
        "1625573000000000",
        "974000000000000",
        "780000000000000000",
        "61",
        "100000000000000000000324951",
    );
    assert_limit_holds_at_execution(&market, 1_830_124_674, 6, (SY, PT), &[1_000_000]);
}

/// PT against YT is not a market edge: minting and redeeming PY is the yield token's business, and
/// SY is one side of every market swap.
#[test]
fn pt_against_yt_is_not_an_edge() {
    let state = wsteth_market(1_700_000_000);
    let error = state
        .get_amount_out(BigUint::from(1_000_000_000_000_000_000u64), &token(PT, 18), &token(YT, 18))
        .expect_err("PT->YT must not quote against the market");
    assert!(matches!(error, SimulationError::FatalError(_)));
}

/// Each direction has its own binding constraint, so the bounds are different numbers rather than
/// one reserve reported four times.
#[test]
fn each_direction_reports_its_own_depth() {
    let state = wsteth_market(1_700_000_000);
    let sy = Bytes::from_str(SY).unwrap();
    let pt = Bytes::from_str(PT).unwrap();
    let yt = Bytes::from_str(YT).unwrap();

    let (sy_for_pt, _) = state
        .get_limits(sy.clone(), pt.clone())
        .unwrap();
    let (sy_for_yt, _) = state
        .get_limits(sy.clone(), yt.clone())
        .unwrap();
    let (pt_for_sy, _) = state
        .get_limits(pt, sy.clone())
        .unwrap();
    let (yt_for_sy, _) = state.get_limits(yt, sy).unwrap();

    assert_ne!(sy_for_pt, sy_for_yt, "the two SY-in legs share a bound");
    assert!(pt_for_sy > BigUint::from(0u32));
    assert!(yt_for_sy > BigUint::from(0u32));
}

/// Enumerating a component's pairs is a question, not a request. A pair the component does not
/// trade reports zero depth; only an actual swap request is wrong enough to fail.
///
/// The integration test walks every pair of a component's three tokens, PT↔YT included, and an
/// error there fails the whole run.
#[test]
fn an_untradeable_pair_reports_no_depth_rather_than_failing() {
    let state = wsteth_market(1_700_000_000);
    let pt = Bytes::from_str(PT).unwrap();
    let yt = Bytes::from_str(YT).unwrap();
    let stranger = Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap();
    let zero = (BigUint::from(0u32), BigUint::from(0u32));

    assert_eq!(
        state
            .get_limits(pt.clone(), yt.clone())
            .unwrap(),
        zero
    );
    assert_eq!(
        state
            .get_limits(yt.clone(), pt.clone())
            .unwrap(),
        zero
    );
    assert_eq!(
        state
            .get_limits(pt.clone(), pt.clone())
            .unwrap(),
        zero
    );
    assert_eq!(
        state
            .get_limits(stranger, pt.clone())
            .unwrap(),
        zero
    );

    // But a swap request for the same pair still refuses, loudly.
    assert!(state
        .get_amount_out(BigUint::from(1_000_000_000_000_000_000u64), &token(PT, 18), &token(YT, 18))
        .is_err());

    // The SY component answers the same way for a token it does not wrap.
    assert_eq!(
        wsteth_sy()
            .get_limits(Bytes::from_str(WSTETH).unwrap(), Bytes::from_str(PT).unwrap())
            .unwrap(),
        zero
    );
}

/// The spot price is quote-per-base, and it is answered from the buying side.
///
/// SY-wstETH accepts WETH as a deposit but does not redeem to it. Pricing SY in WETH is therefore
/// only answerable by asking what WETH buys; asking what selling SY yields in WETH has no answer,
/// and quoting that direction is what failed the range test.
#[test]
fn spot_price_is_answered_from_the_buying_side() {
    let sy = token(SY, 18);
    let steth = token(STETH, 18);

    // 1 SY is worth ~1.2419 stETH at this index, so buying one costs about that much.
    let price = wsteth_sy()
        .spot_price(&sy, &steth)
        .unwrap();
    assert!((price - 1.241884).abs() < 1e-6, "{price}");

    // And the inverse direction is the reciprocal.
    let inverse = wsteth_sy()
        .spot_price(&steth, &sy)
        .unwrap();
    assert!((inverse - 1.0 / 1.241884).abs() < 1e-6, "{inverse}");
}

/// A deposit-only token still prices, because the price is asked from the direction that exists.
#[test]
fn a_deposit_only_token_still_prices() {
    let weth = token("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", 18);
    let sy = token(SY, 18);
    let mut state = wsteth_sy();
    // WETH goes in but never comes out, which is how the wstETH SY really behaves.
    if let PendleState::Sy(inner) = &mut state {
        inner
            .tokens_in
            .insert(weth.address.clone(), TokenClass::IndexRate);
        inner.tokens_out.remove(&weth.address);
        // The decoder records the decimals of every token the stream carries.
        inner
            .token_decimals
            .insert(weth.address.clone(), 18);
    }

    let price = state.spot_price(&sy, &weth).unwrap();
    assert!(price > 0.0, "a deposit-only token should still price the SY");

    // Selling SY for it remains unquotable, and says so.
    assert!(state
        .get_amount_out(BigUint::from(1_000_000_000_000_000_000u64), &sy, &weth)
        .is_err());
}

/// Gas is charged per direction and per SY class, not per leg.
///
/// The exact-SY-in directions run the router's approximation loop on chain and cost meaningfully
/// more than their exact-PT counterparts — 40% more for PT, 27% for YT, measured. Charging one
/// figure to both directions would misprice the cheaper one badly enough to move routing decisions.
#[test]
fn gas_reflects_the_direction_and_the_wrap_class() {
    let state = wsteth_market(1_700_000_000);
    let amount = BigUint::from(1_000_000_000_000_000_000u64);

    let gas = |a: &str, b: &str| {
        state
            .get_amount_out(amount.clone(), &token(a, 18), &token(b, 18))
            .unwrap()
            .gas
    };

    // Buying is dearer than selling on both legs.
    assert!(gas(SY, PT) > gas(PT, SY));
    assert!(gas(SY, YT) > gas(YT, SY));
    // And the YT legs cost more than the PT legs, since they tokenize or redeem PY on top.
    assert!(gas(SY, YT) > gas(SY, PT));
    assert!(gas(YT, SY) > gas(PT, SY));

    // A plain wrapper is the cheapest edge; reading through to a wrapped protocol is not.
    let one_to_one = wsteth_sy()
        .get_amount_out(amount.clone(), &token(WSTETH, 18), &token(SY, 18))
        .unwrap()
        .gas;
    let index_rate = wsteth_sy()
        .get_amount_out(amount.clone(), &token(STETH, 18), &token(SY, 18))
        .unwrap()
        .gas;
    assert!(index_rate > one_to_one);
    assert!(one_to_one < gas(PT, SY), "a wrap should be cheaper than a market swap");
}

/// Redeeming is capped by the SY's supply; depositing is not.
///
/// The two directions of a wrap edge are not symmetric. An SY mints new shares against whatever it
/// forwards on, so a deposit has no reserve to exhaust, but no redemption can take more SY than
/// exists. Supply bounds every exit alike: wstETH paid out directly, and stETH paid by unwrapping
/// the same wstETH. The limit is the supply to the wei, which redeems in full on a mainnet fork.
#[test]
fn redeeming_is_bounded_by_supply_and_depositing_is_not() {
    let sy = Bytes::from_str(SY).unwrap();
    let wsteth = Bytes::from_str(WSTETH).unwrap();
    let steth = Bytes::from_str(STETH).unwrap();
    let limit = u("3248104473502927520625");

    let (wsteth_in, wsteth_out) = wsteth_sy()
        .get_limits(sy.clone(), wsteth.clone())
        .unwrap();
    assert_eq!(wsteth_in, u256_to_biguint(limit));
    assert_eq!(wsteth_out, u256_to_biguint(limit));

    let (steth_in, steth_out) = wsteth_sy()
        .get_limits(sy.clone(), steth.clone())
        .unwrap();
    assert_eq!(steth_in, u256_to_biguint(limit));
    let quoted = wsteth_sy()
        .get_amount_out(steth_in, &token(SY, 18), &token(STETH, 18))
        .unwrap();
    assert_eq!(steth_out, quoted.amount);

    // An uncapped SY has no deposit limit of its own, so it reports the repo's unbounded soft
    // limit, whatever the entry token's decimals.
    let (deposit_in, _) = wsteth_sy()
        .get_limits(wsteth, sy)
        .unwrap();
    assert_eq!(deposit_in, BigUint::from(u128::MAX));
    let (six_decimal_in, _) = six_decimal_asset_sy()
        .get_limits(
            Bytes::from_str(SIX_DECIMAL_TOKEN).unwrap(),
            Bytes::from_str(SIX_DECIMAL_SY).unwrap(),
        )
        .unwrap();
    assert_eq!(six_decimal_in, BigUint::from(u128::MAX));
}

/// A redemption past the limit is refused rather than quoted: on chain the burn reverts once it
/// asks for more SY than exists, and a quote that floored the supply at zero would price it.
#[rstest::rstest]
#[case::paid_in_wsteth(WSTETH)]
#[case::paid_in_steth(STETH)]
fn a_redemption_past_the_limit_is_invalid_input(#[case] exit: &str) {
    let sy = Bytes::from_str(SY).unwrap();
    let (limit, _) = wsteth_sy()
        .get_limits(sy, Bytes::from_str(exit).unwrap())
        .unwrap();
    wsteth_sy()
        .get_amount_out(limit.clone(), &token(SY, 18), &token(exit, 18))
        .expect("the limit itself must be quotable");

    for amount in [limit + 1u32, u256_to_biguint(u("1000000000000000000000000000000"))] {
        let error = wsteth_sy()
            .get_amount_out(amount, &token(SY, 18), &token(exit, 18))
            .unwrap_err();
        assert!(matches!(error, SimulationError::InvalidInput(_, None)), "{error:?}");
    }
}

/// Both bounds on a wrap edge are only meaningful if the input they report actually produces the
/// output they report. Asserted on the decimal-gap component, where substituting the SY's decimals
/// for the token's — the shape this once had — leaves the redeem bound yielding nothing at all.
#[test]
fn a_wrap_bound_is_reachable_across_a_decimal_gap() {
    let sy = Bytes::from_str(SIX_DECIMAL_SY).unwrap();
    let usdc = Bytes::from_str(SIX_DECIMAL_TOKEN).unwrap();
    let sy_token = token(SIX_DECIMAL_SY, 18);
    let usdc_token = token(SIX_DECIMAL_TOKEN, 6);

    let (deposit_in, deposit_out) = six_decimal_asset_sy()
        .get_limits(usdc.clone(), sy.clone())
        .unwrap();
    let quoted = six_decimal_asset_sy()
        .get_amount_out(deposit_in, &usdc_token, &sy_token)
        .expect("the reported deposit maximum must be quotable");
    assert_eq!(quoted.amount, deposit_out, "deposit bound does not describe its own edge");

    let (redeem_in, redeem_out) = six_decimal_asset_sy()
        .get_limits(sy, usdc)
        .unwrap();
    let quoted = six_decimal_asset_sy()
        .get_amount_out(redeem_in.clone(), &sy_token, &usdc_token)
        .expect("the reported redeem maximum must be quotable");
    assert_eq!(quoted.amount, redeem_out, "redeem bound does not describe its own edge");
    // The input side is the supply, in SY units rather than the token's.
    assert_eq!(redeem_in, BigUint::from(10u32).pow(24));
}

/// A token the stream does not carry has no decimals to convert through, and every bound on a wrap
/// edge is a conversion. Reported as no depth rather than as a bound scaled by a guess, which is
/// wrong by orders of magnitude in whichever direction the guess falls.
#[test]
fn a_token_with_unknown_decimals_reports_no_depth() {
    let PendleState::Sy(mut state) = six_decimal_asset_sy() else { panic!("not an SY") };
    state.token_decimals.clear();
    let state = PendleState::Sy(state);

    let sy = Bytes::from_str(SIX_DECIMAL_SY).unwrap();
    let usdc = Bytes::from_str(SIX_DECIMAL_TOKEN).unwrap();
    assert_eq!(
        state
            .get_limits(usdc.clone(), sy.clone())
            .unwrap(),
        (BigUint::ZERO, BigUint::ZERO)
    );
    assert_eq!(state.get_limits(sy, usdc).unwrap(), (BigUint::ZERO, BigUint::ZERO));
}

/// The reported maximum is quotable, and past it the quote fails rather than returning a number the
/// swap would revert on.
#[test]
fn the_reported_limit_is_the_boundary() {
    let state = wsteth_market(1_700_000_000);
    let (max_in, max_out) = state
        .get_limits(Bytes::from_str(SY).unwrap(), Bytes::from_str(PT).unwrap())
        .unwrap();

    let at_limit = state
        .get_amount_out(max_in.clone(), &token(SY, 18), &token(PT, 18))
        .expect("the reported maximum must be quotable");
    assert_eq!(at_limit.amount, max_out);

    let past = &max_in * BigUint::from(2u32);
    assert!(state
        .get_amount_out(past, &token(SY, 18), &token(PT, 18))
        .is_err());
}

/// A trade too big for the market is the caller's size, not a broken state, so a router can retry
/// smaller instead of dropping the component. Checked on the two flash-swap legs, whose oversize
/// failures surface from the PT borrow and from the router's search rather than from a market
/// check of their own.
#[rstest::rstest]
#[case::yt_in_past_the_pt_the_market_holds(YT, SY, "83658000000000000001")]
#[case::sy_in_whose_tokenized_pt_the_market_cannot_absorb(SY, YT, "100000000000000000000000")]
fn an_oversized_flash_swap_is_invalid_input(
    #[case] token_in: &str,
    #[case] token_out: &str,
    #[case] amount: &str,
) {
    let error = wsteth_market(1_700_000_000)
        .get_amount_out(
            BigUint::from_str(amount).unwrap(),
            &token(token_in, 18),
            &token(token_out, 18),
        )
        .unwrap_err();
    assert!(matches!(error, SimulationError::InvalidInput(_, None)), "{error:?}");
}

/// A one-to-one wrap in matching decimals returns exactly what went in. This is the case where a
/// wrong `index_rate` classification is invisible to the indexer but wrong by 24% here.
#[test]
fn a_one_to_one_wrap_is_the_identity() {
    let amount = BigUint::from(1_000_000_000_000_000_000u64);
    let out = wsteth_sy()
        .get_amount_out(amount.clone(), &token(WSTETH, 18), &token(SY, 18))
        .unwrap();
    assert_eq!(out.amount, amount);
}

/// The accounting asset converts at the index instead, in both directions.
#[test]
fn an_index_rate_wrap_uses_the_exchange_rate() {
    let one = BigUint::from(1_000_000_000_000_000_000u64);
    let deposit = wsteth_sy()
        .get_amount_out(one.clone(), &token(STETH, 18), &token(SY, 18))
        .unwrap();
    // 1 stETH at an index of 1.241884 is less than one SY.
    assert!(deposit.amount < one);

    let redeem = wsteth_sy()
        .get_amount_out(one.clone(), &token(SY, 18), &token(STETH, 18))
        .unwrap();
    assert!(redeem.amount > one);
}

/// The decimal axis end to end. One whole SY at the reUSD index is ~1.09583 units of a 6-decimal
/// asset, not 1.09583e18.
#[test]
fn the_index_carries_the_decimal_gap_through_the_quote() {
    let one_sy = BigUint::from(1_000_000_000_000_000_000u64);
    let out = six_decimal_asset_sy()
        .get_amount_out(one_sy, &token(SIX_DECIMAL_SY, 18), &token(SIX_DECIMAL_TOKEN, 6))
        .unwrap();
    assert_eq!(out.amount, BigUint::from(1_095_830u32));
}

/// A token the indexer could not classify is absent from the component, and quoting it says so
/// rather than assuming 1:1.
#[test]
fn an_unclassified_token_is_not_quotable() {
    let unknown = token("0x1111111111111111111111111111111111111111", 18);
    let error = wsteth_sy()
        .get_amount_out(BigUint::from(1u32), &unknown, &token(SY, 18))
        .expect_err("an unclassified token must not quote");
    let SimulationError::FatalError(message) = error else { panic!("wrong variant") };
    assert!(message.contains("could not classify"), "{message}");
}

/// A zero amount is the caller's mistake, not a zero quote.
#[test]
fn a_zero_amount_is_rejected() {
    let error = wsteth_market(1_700_000_000)
        .get_amount_out(BigUint::from(0u32), &token(PT, 18), &token(SY, 18))
        .expect_err("zero must not quote");
    assert!(matches!(error, SimulationError::InvalidInput(_, _)));
}

/// The fee is the market's, decays toward expiry, and a wrapper charges none.
#[test]
fn the_fee_decays_and_wrappers_charge_none() {
    let early = wsteth_market(1_700_000_000).fee();
    let late = wsteth_market(1_820_000_000).fee();
    assert!(early > 0.0);
    assert!(late < early);
    assert_eq!(wsteth_sy().fee(), 0.0);
}

/// A delta moves the state the quote runs on, including the clock.
#[test]
fn a_delta_updates_the_state() {
    let mut state = wsteth_market(1_700_000_000);
    let delta = ProtocolStateDelta {
        component_id: "market".to_string(),
        updated_attributes: HashMap::from([
            ("rate_sampled_at".to_string(), Bytes::from(1_800_000_000u64.to_be_bytes().to_vec())),
            ("block_timestamp".to_string(), Bytes::from(1_800_000_000u64.to_be_bytes().to_vec())),
            (
                "total_pt".to_string(),
                Bytes::from(
                    s("90000000000000000000")
                        .to_be_bytes::<32>()
                        .to_vec(),
                ),
            ),
        ]),
        deleted_attributes: Default::default(),
    };
    state
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .unwrap();
    let PendleState::Market(updated) = state else { panic!("expected a market") };
    assert_eq!(updated.rate_sampled_at, 1_800_000_000);
    assert_eq!(updated.head_timestamp, 1_800_000_000);
    assert_eq!(updated.market.total_pt, s("90000000000000000000"));
}

/// The reference market as `readState(router)` and `YT.pyIndexCurrent()` return it at block
/// 25 800 000 (timestamp 1 787 273 051).
fn wsteth_market_at_block_25_800_000() -> PendleState {
    PendleState::Market(PendleMarketState {
        market: MarketState {
            total_pt: s("87682116111289735171"),
            total_sy: s("1420858529864045104476"),
            scalar_root: s("86364560000000000000"),
            expiry: 1_830_124_800,
            ln_fee_rate_root: u("499875041000000"),
            reserve_fee_percent: u("80"),
            last_ln_implied_rate: u("20809737743012450"),
        },
        py_index: u("1242113888677499794"),
        rate_sampled_at: 1_787_273_051,
        head_timestamp: 1_787_273_051,
        sy_address: Bytes::from_str(SY).unwrap(),
        pt_address: Bytes::from_str(PT).unwrap(),
        yt_address: Bytes::from_str(YT).unwrap(),
        sy_paused: false,
        execution_timestamp: 0,
        fee_unread: false,
        // The reference wstETH market comes from the original factory.
        from_original_factory: true,
        memo: Default::default(),
    })
}

/// Swaps executed on a mainnet fork at block 25 800 000, shared with the executor's fork test.
#[derive(serde::Deserialize)]
struct ExecutedSwaps {
    executed: Vec<ExecutedSwap>,
    rejected: Vec<RejectedSwap>,
}

#[derive(serde::Deserialize)]
struct ExecutedSwap {
    edge: String,
    amount_in: String,
    amount_out: String,
}

#[derive(serde::Deserialize)]
struct RejectedSwap {
    edge: String,
    amount_in: String,
}

fn executed_swaps() -> ExecutedSwaps {
    serde_json::from_str(include_str!("tests/fixtures/executed_swaps.json")).unwrap()
}

/// The tokens an edge name such as `sy_for_pt` trades, in and out.
fn edge_tokens(edge: &str) -> (Token, Token) {
    let address = |name: &str| match name {
        "sy" => SY,
        "pt" => PT,
        "yt" => YT,
        other => panic!("no token {other}"),
    };
    let (token_in, token_out) = edge.split_once("_for_").unwrap();
    (token(address(token_in), 18), token(address(token_out), 18))
}

#[test]
fn every_market_edge_quotes_what_the_chain_executed() {
    let state = wsteth_market_at_block_25_800_000();
    let swaps = executed_swaps().executed;
    assert_eq!(swaps.len(), 39);
    for swap in swaps {
        let (token_in, token_out) = edge_tokens(&swap.edge);
        let quote = state
            .get_amount_out(BigUint::from_str(&swap.amount_in).unwrap(), &token_in, &token_out)
            .unwrap_or_else(|e| panic!("{} {}: {e}", swap.edge, swap.amount_in));
        assert_eq!(quote.amount.to_string(), swap.amount_out, "{} {}", swap.edge, swap.amount_in);
    }
}

/// Swaps executed on two V6-factory markets at block 26 228 000, with the state each market had
/// there.
#[derive(serde::Deserialize)]
struct ExecutedSwapsV6 {
    markets: Vec<ExecutedMarket>,
}

#[derive(serde::Deserialize)]
struct ExecutedMarket {
    name: String,
    sy: String,
    pt: String,
    yt: String,
    decimals: u32,
    state: ExecutedMarketState,
    executed: Vec<ExecutedSwap>,
    rejected: Vec<RejectedV6Swap>,
}

/// The market's `readState(router)` and its YT's `pyIndexCurrent()`, as decimal strings.
#[derive(serde::Deserialize)]
struct ExecutedMarketState {
    total_pt: String,
    total_sy: String,
    scalar_root: String,
    expiry: String,
    ln_fee_rate_root: String,
    reserve_fee_percent: String,
    last_ln_implied_rate: String,
    py_index: String,
    timestamp: String,
}

#[derive(serde::Deserialize)]
struct RejectedV6Swap {
    edge: String,
    amount_in: String,
    /// The revert's selector; `0x880ccd2c` is `MarketZeroNetLPFee`.
    revert: String,
}

impl ExecutedMarket {
    fn state(&self) -> PendleState {
        let signed = |value: &str| I256::from_dec_str(value).unwrap();
        let unsigned = |value: &str| U256::from_str(value).unwrap();
        let timestamp: u64 = self.state.timestamp.parse().unwrap();
        PendleState::Market(PendleMarketState {
            market: MarketState {
                total_pt: signed(&self.state.total_pt),
                total_sy: signed(&self.state.total_sy),
                scalar_root: signed(&self.state.scalar_root),
                expiry: self.state.expiry.parse().unwrap(),
                ln_fee_rate_root: unsigned(&self.state.ln_fee_rate_root),
                reserve_fee_percent: unsigned(&self.state.reserve_fee_percent),
                last_ln_implied_rate: unsigned(&self.state.last_ln_implied_rate),
            },
            py_index: unsigned(&self.state.py_index),
            rate_sampled_at: timestamp,
            head_timestamp: timestamp,
            sy_address: Bytes::from_str(&self.sy).unwrap(),
            pt_address: Bytes::from_str(&self.pt).unwrap(),
            yt_address: Bytes::from_str(&self.yt).unwrap(),
            sy_paused: false,
            execution_timestamp: 0,
            fee_unread: false,
            from_original_factory: false,
            memo: Default::default(),
        })
    }

    /// The tokens an edge name such as `sy_for_pt` trades on this market, in and out.
    fn edge_tokens(&self, edge: &str) -> (Token, Token) {
        let address = |name: &str| match name {
            "sy" => &self.sy,
            "pt" => &self.pt,
            "yt" => &self.yt,
            other => panic!("no token {other}"),
        };
        let (token_in, token_out) = edge.split_once("_for_").unwrap();
        (token(address(token_in), self.decimals), token(address(token_out), self.decimals))
    }
}

fn executed_swaps_v6() -> ExecutedSwapsV6 {
    serde_json::from_str(include_str!("tests/fixtures/executed_swaps_v6.json")).unwrap()
}

/// Every swap executed on the two V6-factory markets is quoted to the wei, on all four edges: an
/// 18-decimal market and one whose SY and asset have 6 decimals. Every swap the chain reverted is
/// refused as `InvalidInput`, and those that reverted with `MarketZeroNetLPFee` are refused for
/// that reason.
#[test]
fn every_v6_market_edge_quotes_what_the_chain_executed() {
    let markets = executed_swaps_v6().markets;
    assert_eq!(markets.len(), 2);
    for market in markets {
        let state = market.state();
        assert!(market.executed.len() >= 20, "{}", market.name);
        for swap in &market.executed {
            let (token_in, token_out) = market.edge_tokens(&swap.edge);
            let quote = state
                .get_amount_out(BigUint::from_str(&swap.amount_in).unwrap(), &token_in, &token_out)
                .unwrap_or_else(|e| {
                    panic!("{} {} {}: {e}", market.name, swap.edge, swap.amount_in)
                });
            assert_eq!(
                quote.amount.to_string(),
                swap.amount_out,
                "{} {} {}",
                market.name,
                swap.edge,
                swap.amount_in
            );
        }
        for swap in &market.rejected {
            let (token_in, token_out) = market.edge_tokens(&swap.edge);
            let error = state
                .get_amount_out(BigUint::from_str(&swap.amount_in).unwrap(), &token_in, &token_out)
                .expect_err(&format!(
                    "{} {} {} must be refused",
                    market.name, swap.edge, swap.amount_in
                ));
            let SimulationError::InvalidInput(message, None) = &error else {
                panic!("{} {} {}: {error:?}", market.name, swap.edge, swap.amount_in);
            };
            if swap.revert == "0x880ccd2c" {
                assert!(
                    message.contains("MarketZeroNetLPFee"),
                    "{} {} {}: {message}",
                    market.name,
                    swap.edge,
                    swap.amount_in
                );
            }
        }
    }
}

/// The router's search runs out of range on these inputs at that block, so the swap reverts.
#[test]
fn a_trade_the_chain_rejects_is_rejected() {
    let state = wsteth_market_at_block_25_800_000();
    for swap in executed_swaps().rejected {
        let (token_in, token_out) = edge_tokens(&swap.edge);
        let error = state
            .get_amount_out(BigUint::from_str(&swap.amount_in).unwrap(), &token_in, &token_out)
            .unwrap_err();
        assert!(matches!(error, SimulationError::InvalidInput(_, None)), "{error:?}");
    }
}

/// The indexer writes unsigned values with `to_signed_bytes_be`, so one at or above 2^255 carries
/// a leading zero byte and is 33 bytes long. Only a value with more than 32 significant bytes is
/// malformed.
#[test]
fn a_value_is_read_past_its_sign_byte_but_not_past_256_bits() {
    let mut top_bit = vec![0u8, 0x80];
    top_bit.extend([0u8; 31]);
    assert_eq!(u256_from_be(&top_bit, "rate").unwrap(), U256::from(1u8) << 255);

    let mut too_wide = vec![1u8];
    too_wide.extend([0u8; 32]);
    assert!(u256_from_be(&too_wide, "rate").is_err());
}

#[test]
fn an_amount_wider_than_256_bits_is_invalid_input() {
    let error = wsteth_market(1_700_000_000)
        .get_amount_out(BigUint::from(1u8) << 256, &token(SY, 18), &token(PT, 18))
        .unwrap_err();
    assert!(matches!(error, SimulationError::InvalidInput(_, None)), "{error:?}");
}

/// A delta carrying a zero rate leaves the SY never sampled, whatever timestamp rides with it: its
/// `index_rate` edges refuse and report no depth, its `one_to_one` edges go on quoting, and the
/// delta itself does not fail. A later non-zero rate brings it back.
#[test]
fn a_delta_with_a_zero_exchange_rate_stops_the_rate() {
    let mut state = wsteth_sy();
    let rate_update = |rate: Vec<u8>| ProtocolStateDelta {
        component_id: SY.to_string(),
        updated_attributes: HashMap::from([
            ("sy_exchange_rate".to_string(), Bytes::from(rate)),
            ("rate_sampled_at".to_string(), Bytes::from(1_700_000_012u64.to_be_bytes().to_vec())),
            ("block_timestamp".to_string(), Bytes::from(1_700_000_012u64.to_be_bytes().to_vec())),
        ]),
        ..Default::default()
    };
    state
        .delta_transition(rate_update(vec![0]), &HashMap::new(), &Balances::default())
        .unwrap();
    let one = BigUint::from(10u64.pow(18));
    assert!(state
        .get_amount_out(one.clone(), &token(SY, 18), &token(STETH, 18))
        .is_err());
    let (max_in, _) = state
        .get_limits(Bytes::from_str(SY).unwrap(), Bytes::from_str(STETH).unwrap())
        .unwrap();
    assert_eq!(max_in, BigUint::ZERO);
    assert!(state
        .get_amount_out(one.clone(), &token(SY, 18), &token(WSTETH, 18))
        .is_ok());

    state
        .delta_transition(
            rate_update(
                u("1241884000000000000")
                    .to_be_bytes::<32>()
                    .to_vec(),
            ),
            &HashMap::new(),
            &Balances::default(),
        )
        .unwrap();
    assert!(state
        .get_amount_out(one, &token(SY, 18), &token(STETH, 18))
        .is_ok());
}

/// On a market too shallow to fill a whole unit, the price is still answered, from a probe inside
/// the depth.
#[test]
fn a_shallow_market_still_has_a_spot_price() {
    let PendleState::Market(mut shallow) = wsteth_market(1_700_000_000) else { unreachable!() };
    shallow.market.total_pt /= I256::try_from(1_000_000).unwrap();
    shallow.market.total_sy /= I256::try_from(1_000_000).unwrap();
    let state = PendleState::Market(shallow);
    let one = BigUint::from(1_000_000_000_000_000_000u64);
    assert!(state
        .get_amount_out(one, &token(SY, 18), &token(YT, 18))
        .is_err());

    let price = state
        .spot_price(&token(YT, 18), &token(SY, 18))
        .unwrap();
    let deep = wsteth_market(1_700_000_000)
        .spot_price(&token(YT, 18), &token(SY, 18))
        .unwrap();
    assert!((price - deep).abs() / deep < 0.01, "shallow {price} against deep {deep}");
}

/// A redemption with no supply to take has no depth, but its conversion rate is still a price.
#[test]
fn a_direction_without_depth_still_has_a_spot_price() {
    let mut state = wsteth_sy();
    if let PendleState::Sy(inner) = &mut state {
        inner.total_supply = U256::ZERO;
    }
    let (max_in, _) = state
        .get_limits(Bytes::from_str(SY).unwrap(), Bytes::from_str(WSTETH).unwrap())
        .unwrap();
    assert_eq!(max_in, BigUint::ZERO);

    let price = state
        .spot_price(&token(WSTETH, 18), &token(SY, 18))
        .unwrap();
    assert!((price - 1.0).abs() < 1e-9, "{price}");
}

/// A token the SY accepts but never pays out still prices in the direction that trades, which is
/// every direction a caller enumerates and finds depth in.
#[test]
fn a_deposit_only_token_prices_in_the_direction_it_trades() {
    let weth = token("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", 18);
    let sy = token(SY, 18);
    let mut state = wsteth_sy();
    if let PendleState::Sy(inner) = &mut state {
        inner
            .tokens_in
            .insert(weth.address.clone(), TokenClass::IndexRate);
        inner.tokens_out.remove(&weth.address);
        inner
            .token_decimals
            .insert(weth.address.clone(), 18);
    }

    // WETH -> SY trades, SY -> WETH does not.
    let price = state.spot_price(&weth, &sy).unwrap();
    // One WETH wraps into 1 / exchange_rate SY.
    assert!((price - 1.0 / 1.241884).abs() < 1e-6, "{price}");
}

/// A repeated `get_limits` call answers what the first did, and after a delta, an execution block
/// or a field changed in place the state answers what a fresh copy of it would.
#[test]
fn kept_limits_follow_every_change_to_the_state() {
    let fresh = |state: &PendleState, sell: &str, buy: &str| {
        state
            .clone()
            .get_limits(Bytes::from_str(sell).unwrap(), Bytes::from_str(buy).unwrap())
            .unwrap()
    };
    let limits = |state: &PendleState, sell: &str, buy: &str| {
        state
            .get_limits(Bytes::from_str(sell).unwrap(), Bytes::from_str(buy).unwrap())
            .unwrap()
    };
    let mut state = wsteth_market(1_700_000_000);
    for (sell, buy) in [(SY, PT), (PT, SY), (SY, YT), (YT, SY)] {
        assert_eq!(limits(&state, sell, buy), limits(&state, sell, buy));
    }
    let before = limits(&state, YT, SY);

    let delta = ProtocolStateDelta {
        component_id: "market".to_string(),
        updated_attributes: HashMap::from([(
            "total_pt".to_string(),
            Bytes::from(
                u("60000000000000000000")
                    .to_be_bytes::<32>()
                    .to_vec(),
            ),
        )]),
        deleted_attributes: Default::default(),
    };
    state
        .delta_transition(delta, &HashMap::new(), &Balances::default())
        .unwrap();
    let after_delta = limits(&state, YT, SY);
    assert_ne!(after_delta, before);
    assert_eq!(after_delta, fresh(&state, YT, SY));

    state.apply_block(&BlockContext::new(1, 1_700_000_012));
    let after_block = limits(&state, YT, SY);
    assert_ne!(after_block, after_delta);
    assert_eq!(after_block, fresh(&state, YT, SY));

    if let PendleState::Market(market) = &mut state {
        market.head_timestamp = market.rate_sampled_at + 12;
    }
    assert_eq!(limits(&state, YT, SY), no_depth());
}

/// A market on an SY listed in `MARKET_GAS_SURCHARGE` is charged the SY's surcharge on top of every
/// edge's constant, and a listed `(SY, token)` wrap its pair's surcharge on top of its class's
/// constant. Anything unlisted is charged the constant alone.
#[test]
fn a_listed_sy_is_charged_its_gas_surcharge() {
    const MIX_WETH_SY: &str = "0x3870cf5e2eab623a74d2af1007295963b1c5a12f";
    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    let one = BigUint::from(10u64.pow(18));
    let gas = |state: &PendleState, from: &str, to: &str| {
        state
            .get_amount_out(one.clone(), &token(from, 18), &token(to, 18))
            .unwrap()
            .gas
    };

    let PendleState::Market(plain) = wsteth_market(1_787_273_051) else { unreachable!() };
    let listed = PendleState::Market(PendleMarketState {
        sy_address: Bytes::from_str(MIX_WETH_SY).unwrap(),
        ..plain.clone()
    });
    let plain = PendleState::Market(plain);
    for (from, to, constant) in [
        (SY, PT, GAS_SY_FOR_PT),
        (PT, SY, GAS_PT_FOR_SY),
        (SY, YT, GAS_SY_FOR_YT),
        (YT, SY, GAS_YT_FOR_SY),
    ] {
        assert_eq!(gas(&plain, from, to), BigUint::from(constant), "{from}->{to}");
        let (from, to) = (from.replace(SY, MIX_WETH_SY), to.replace(SY, MIX_WETH_SY));
        assert_eq!(gas(&listed, &from, &to), BigUint::from(constant + 535_000), "{from}->{to}");
    }

    let PendleState::Sy(mut sy) = wsteth_sy() else { unreachable!() };
    sy.sy_address = Bytes::from_str(MIX_WETH_SY).unwrap();
    let weth = Bytes::from_str(WETH).unwrap();
    sy.tokens_out
        .insert(weth.clone(), TokenClass::IndexRate);
    sy.token_decimals.insert(weth, 18);
    let sy = PendleState::Sy(sy);
    assert_eq!(gas(&sy, MIX_WETH_SY, WETH), BigUint::from(GAS_WRAP_INDEX_RATE + 1_380_000));
    assert_eq!(gas(&sy, MIX_WETH_SY, STETH), BigUint::from(GAS_WRAP_INDEX_RATE));
    assert_eq!(gas(&sy, WSTETH, MIX_WETH_SY), BigUint::from(GAS_WRAP_ONE_TO_ONE));
}

/// Every market factory but the original deploys `PendleMarketV7`, which reverts with
/// `MarketZeroNetLPFee` when the LPs' share of the fee converts to zero asset units. A dust trade
/// is refused there as `InvalidInput`, on all four edges, and quoted on an original-factory market;
/// a trade of size quotes on both.
#[test]
fn a_trade_paying_the_lps_nothing_is_refused_on_later_factory_markets() {
    let PendleState::Market(original) = wsteth_market(1_787_273_051) else { unreachable!() };
    let later =
        PendleState::Market(PendleMarketState { from_original_factory: false, ..original.clone() });
    let original = PendleState::Market(original);
    let quote = |state: &PendleState, amount: u64, from: &str, to: &str| {
        state.get_amount_out(BigUint::from(amount), &token(from, 18), &token(to, 18))
    };
    for (from, to) in [(PT, SY), (YT, SY), (SY, PT), (SY, YT)] {
        // Ten wei pays the LPs nothing; a tenth of a token pays them something.
        match quote(&later, 10, from, to) {
            Err(SimulationError::InvalidInput(message, None)) => {
                assert!(message.contains("MarketZeroNetLPFee"), "{from}->{to}: {message}")
            }
            other => panic!("{from}->{to}: a dust trade must be refused, got {other:?}"),
        }
        assert!(quote(&later, 100_000_000_000_000_000, from, to).is_ok(), "{from}->{to}");
        // Dust trades can fail on the original factory's market for their own reasons (a YT sale
        // too small to repay its loan); none of them is this one.
        if let Err(SimulationError::InvalidInput(message, _)) = quote(&original, 10, from, to) {
            assert!(!message.contains("MarketZeroNetLPFee"), "{from}->{to}: {message}");
        }
    }
}

/// The spot price prices a small probe to read the curve's slope, and is answered even where every
/// such probe's fee to the LPs rounds to zero, as on a market charging almost no fee.
#[test]
fn the_spot_price_ignores_the_lp_fee_refusal() {
    let PendleState::Market(mut market) = wsteth_market(1_787_273_051) else { unreachable!() };
    market.from_original_factory = false;
    market.market.ln_fee_rate_root = U256::from(1u8);
    let state = PendleState::Market(market);
    assert!(state
        .get_amount_out(BigUint::from(10u64.pow(18)), &token(PT, 18), &token(SY, 18))
        .is_err());
    let price = state
        .spot_price(&token(PT, 18), &token(SY, 18))
        .expect("the spot price is answered");
    assert!(price > 0.0);
}

const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const USDAT: &str = "0x23238f20b894f29041f48d88ee91131c395aaa71";

/// An 18-decimal SY over a 6-decimal asset, taking a 6-decimal token as `index_rate` at `rate`, as
/// SY-mixUSDC, SY-PPLUS and SY-sUSDat do.
fn rate_sy(rate: u64, asset_decimals: u32, token: &str) -> PendleSyState {
    let PendleState::Sy(mut sy) = wsteth_sy() else { unreachable!() };
    let token = Bytes::from_str(token).unwrap();
    sy.exchange_rate = U256::from(rate);
    sy.asset_decimals = asset_decimals;
    sy.tokens_in = HashMap::from([(token.clone(), TokenClass::IndexRate)]);
    sy.token_decimals = HashMap::from([(token, 6)]);
    sy
}

fn deposit(sy: &PendleSyState, entry: &str, amount: u64) -> BigUint {
    PendleState::Sy(sy.clone())
        .get_amount_out(BigUint::from(amount), &token(entry, 6), &token(SY, 18))
        .unwrap()
        .amount
}

/// SY-mixUSDC's rate carries seven digits (1,024,060 at block 26,228,000), and one USDC mints
/// 976,504,993,258,965,504 SY wei where that rate alone predicts 976,505,282,893,580,454. Quoted at
/// the rate plus one, the deposit never promises more than the SY mints, and stays within one part
/// in the rate of it.
#[test]
fn an_index_rate_deposit_never_promises_more_than_the_sy_mints() {
    let minted = BigUint::from(976_504_993_258_965_504u64);
    let quoted = deposit(&rate_sy(1_024_060, 6, USDC), USDC, 1_000_000);
    assert!(quoted <= minted, "{quoted} > {minted}");
    assert!(minted.clone() - quoted <= minted / BigUint::from(1_000_000u32));
}

/// SY-PPLUS computes in USDC's six decimals and mints a multiple of 10^12 SY wei: one USDC mints
/// 978,906 × 10^12 and ten mint 9,789,069 × 10^12. With its step, each quote is exactly that.
#[test]
fn a_deposit_with_a_step_mints_a_whole_multiple_of_it() {
    let mut sy = rate_sy(1_021_547_529_624_145_398, 18, USDC);
    sy.deposit_steps = HashMap::from([(Bytes::from_str(USDC).unwrap(), U256::from(10u64.pow(12)))]);
    assert_eq!(deposit(&sy, USDC, 1_000_000), BigUint::from(978_906_000_000_000_000u64));
    assert_eq!(deposit(&sy, USDC, 10_000_000), BigUint::from(9_789_069_000_000_000_000u64));
}

/// The sUSDat vault charges 25 bp on deposits from its announced close until it opens again. At
/// block 26,100,000 (rate 1,040,756) the fee applied and one USDat minted 958,437,362,004,859,566
/// SY wei. A quote before the close charges nothing; one from the close on, or for an execution
/// block past it, charges the fee and stays within a part per million below what the SY minted;
/// and recording such an execution block re-emits the state.
#[test]
fn a_deposit_pays_the_entry_fee_from_the_close() {
    let mut sy = rate_sy(1_040_756, 6, USDAT);
    sy.entry_fee_bps = 25;
    sy.entry_fee_from = sy.head_timestamp + 600;
    let free = deposit(&sy, USDAT, 1_000_000);

    let mut closed = sy.clone();
    closed.head_timestamp = sy.entry_fee_from;
    closed.rate_sampled_at = sy.entry_fee_from;
    let charged = deposit(&closed, USDAT, 1_000_000);
    let minted = BigUint::from(958_437_362_004_859_566u64);
    assert!(charged <= minted, "{charged} > {minted}");
    assert!(minted.clone() - charged.clone() <= minted / BigUint::from(1_000_000u32));
    assert!(free > charged);

    let mut state = PendleState::Sy(sy.clone());
    assert!(!state.apply_block(&BlockContext::new(1, sy.entry_fee_from - 12)));
    assert!(state.apply_block(&BlockContext::new(2, sy.entry_fee_from)));
    let at_execution = state
        .get_amount_out(BigUint::from(1_000_000u32), &token(USDAT, 6), &token(SY, 18))
        .unwrap()
        .amount;
    assert_eq!(at_execution, charged);
}
