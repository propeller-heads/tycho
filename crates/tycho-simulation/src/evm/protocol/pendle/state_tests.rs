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

const SY: &str = "0xcbc72d92b2dc8187414f6734718563898740c0bc";
const PT: &str = "0xcf44e8402a99db82d2acccc4d9354657be2121db";
const YT: &str = "0xa53ad7e3a87546cca450992d54d517c3c939c2bf";
const WSTETH: &str = "0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0";
const STETH: &str = "0xae7ab96520de3a18e5e111b5eaab095312d7fe84";

fn s(value: &str) -> I256 {
    I256::from_dec_str(value).unwrap()
}

fn u(value: &str) -> U256 {
    U256::from_str_radix(value, 10).unwrap()
}

/// The brief's reference market.
fn wsteth_market(block_timestamp: u64) -> PendleState {
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
        block_timestamp,
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
        rate_stale: false,
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
    })
}

/// The decimal axis: SY 18, accounting asset 6, index carrying the 1e12 gap.
fn reusd_sy() -> PendleState {
    PendleState::Sy(PendleSyState {
        sy_address: Bytes::from_str(SY).unwrap(),
        exchange_rate: u("1095830"),
        rate_stale: false,
        sy_decimals: 18,
        asset_decimals: 6,
        tokens_in: HashMap::from([(Bytes::from_str(STETH).unwrap(), TokenClass::IndexRate)]),
        tokens_out: HashMap::from([(Bytes::from_str(STETH).unwrap(), TokenClass::IndexRate)]),
    })
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
    let out = reusd_sy()
        .get_amount_out(one_sy, &token(SY, 18), &token(STETH, 6))
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
    assert_eq!(updated.block_timestamp, 1_800_000_000);
    assert_eq!(updated.market.total_pt, s("90000000000000000000"));
}
