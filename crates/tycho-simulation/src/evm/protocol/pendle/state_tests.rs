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
        // What the SY actually custodies, which is all it can redeem.
        token_balances: HashMap::from([
            (Bytes::from_str(WSTETH).unwrap(), u("546768952998816380")),
            (Bytes::from_str(STETH).unwrap(), u("4")),
        ]),
        token_decimals: HashMap::from([
            (Bytes::from_str(WSTETH).unwrap(), 18),
            (Bytes::from_str(STETH).unwrap(), 18),
        ]),
        component_id: SY.to_string(),
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
        token_balances: HashMap::from([(
            Bytes::from_str(SIX_DECIMAL_TOKEN).unwrap(),
            u("1000000000000"),
        )]),
        token_decimals: HashMap::from([(Bytes::from_str(SIX_DECIMAL_TOKEN).unwrap(), 6)]),
        component_id: SIX_DECIMAL_SY.to_string(),
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

/// Redeeming draws down what the wrapper custodies, which is the bound the next redemption is
/// held to. Depositing does not credit it back: an `index_rate` SY forwards the token into
/// whatever it wraps rather than holding it.
#[test]
fn a_redemption_draws_down_the_wrapper_and_a_deposit_does_not_refill_it() {
    let state = wsteth_sy();
    let wsteth = Bytes::from_str(WSTETH).unwrap();
    let PendleState::Sy(before) = &state else { unreachable!() };
    let held = before.token_balances[&wsteth];

    let amount = BigUint::from(100_000_000_000_000_000u64);
    let redeemed = quote_after(&state, amount.clone(), &token(SY, 18), &token(WSTETH, 18));
    let PendleState::Sy(redeemed) = &redeemed else { unreachable!() };
    assert_eq!(
        redeemed.token_balances[&wsteth],
        held - U256::from(100_000_000_000_000_000u64),
        "a redemption must draw down the holdings it was paid out of"
    );
    // And the next redemption is bounded by what is left.
    let (_, max_out) = PendleState::Sy(redeemed.clone())
        .get_limits(Bytes::from_str(SY).unwrap(), wsteth.clone())
        .unwrap();
    assert_eq!(max_out, u256_to_biguint(redeemed.token_balances[&wsteth]));

    let deposited = quote_after(&state, amount, &token(WSTETH, 18), &token(SY, 18));
    let PendleState::Sy(deposited) = &deposited else { unreachable!() };
    assert_eq!(deposited.token_balances[&wsteth], held, "a deposit must not invent depth");
}

/// A token the indexer reported no balance for stays absent rather than being written in at some
/// number: `held` reads an absent balance as the soft limit, and a fabricated one would become a
/// hard bound on a wrapper that has no such bound recorded.
#[test]
fn a_redemption_does_not_invent_a_balance_that_was_never_indexed() {
    let PendleState::Sy(mut unindexed) = six_decimal_asset_sy() else { panic!("not an SY") };
    unindexed.token_balances.clear();
    let state = PendleState::Sy(unindexed);
    let after = quote_after(
        &state,
        BigUint::from(1_000_000_000_000_000_000u64),
        &token(SIX_DECIMAL_SY, 18),
        &token(SIX_DECIMAL_TOKEN, 6),
    );
    let PendleState::Sy(after) = &after else { unreachable!() };
    assert!(after.token_balances.is_empty(), "an unindexed balance must stay unindexed");
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

/// Redeeming is capped by what the SY holds; depositing is not.
///
/// The two directions of a wrap edge are not symmetric. An SY mints new shares against whatever it
/// forwards on, so a deposit has no reserve to exhaust — but it can only pay out what it has. The
/// wstETH SY holds 4 wei of stETH at the indexed block, and that is the whole redeem depth.
#[test]
fn redeeming_is_bounded_by_holdings_and_depositing_is_not() {
    let sy = Bytes::from_str(SY).unwrap();
    let wsteth = Bytes::from_str(WSTETH).unwrap();
    let steth = Bytes::from_str(STETH).unwrap();

    let (_, redeem_out) = wsteth_sy()
        .get_limits(sy.clone(), steth.clone())
        .unwrap();
    assert_eq!(redeem_out, BigUint::from(4u32), "redeem depth is the SY's own stETH balance");

    let (_, wsteth_out) = wsteth_sy()
        .get_limits(sy.clone(), wsteth.clone())
        .unwrap();
    assert_eq!(wsteth_out, BigUint::from(546_768_952_998_816_380u64));

    // Depositing is unbounded, and reported as the soft limit rather than a U256 sentinel that
    // would overflow the first multiplication a router did with it.
    let (deposit_in, _) = wsteth_sy()
        .get_limits(wsteth, sy)
        .unwrap();
    assert_eq!(deposit_in, BigUint::from(u128::MAX));
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
        .get_amount_out(redeem_in, &sy_token, &usdc_token)
        .expect("the reported redeem maximum must be quotable");
    assert_eq!(quoted.amount, redeem_out, "redeem bound does not describe its own edge");
    // At or under what the SY holds, and within a wei of it — the two floors cost no more.
    let held = BigUint::from(1_000_000_000_000u64);
    assert!(redeem_out <= held && redeem_out >= &held - BigUint::from(1u32), "{redeem_out}");
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
    assert_eq!(state.get_limits(usdc.clone(), sy.clone()).unwrap(), (BigUint::ZERO, BigUint::ZERO));
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
