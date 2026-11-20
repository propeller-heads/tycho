//! Tests of the Ascend hook on its own and through `UniswapV4State`.
//!
//! Expected values come from outside the code under test: Elysium chain data named by fact id
//! (`S..`) from the integration spec (V4Quoter quotes, traces and storage reads of the deployed
//! hook), and values worked out with an independent Python model of the spec's rules
//! (S14, S16) over the inputs shown in each test. A test that states no source holds a number
//! worked by hand beside it.
use std::{collections::HashMap, str::FromStr};

use alloy::primitives::{Address, I128, I256, U256};
use num_bigint::BigUint;
use rstest::rstest;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{token::Token, Chain},
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, BlockContext, ProtocolSim},
    },
    Bytes,
};

use super::{
    hook_handler::{
        fee_ceil, fee_floor, max_gross_buy, AscendHookHandler, ASCEND_FILL_BEFORE_SWAP_GAS,
        ASCEND_HOOK_ELYSIUM,
    },
    hook_handler_creator::AscendHookCreator,
    math::{amount0_delta, observe, size_fill, Curve, FillRequest, LAUNCH_SUPPLY},
    *,
};
use crate::{
    evm::protocol::{
        uniswap_v4::{
            hooks::{
                hook_handler::HookHandler,
                hook_handler_creator::{
                    has_native_hook_handler, initialize_hook_handlers, test_fixtures::BareSnapshot,
                    HookHandlerCreator,
                },
                models::{BeforeSwapParameters, StateContext, SwapOutcome, SwapParams},
            },
            state::{UniswapV4Fees, UniswapV4State},
        },
        utils::uniswap::{
            lp_fee::DYNAMIC_FEE_FLAG, tick_list::TickInfo, tick_math::get_sqrt_ratio_at_tick,
        },
    },
    protocol::errors::InvalidSnapshotError,
};

/// The timestamp of block 8660000 on Elysium, at which S32's experiments quote (S32).
const TS: u64 = 1_795_168_252;

/// `L` of every launch with start tick -168600, read from the chain: the active liquidity of pool
/// 0x1aaf... at block 8397596 (S28), equal to S7's formula (S7).
const L_168600: u128 = 218_323_400_825_907_916_615_201;

fn u(decimal: &str) -> U256 {
    U256::from_str(decimal).expect("a decimal U256")
}

/// `sqrtAt(-168500)`, `sqrtAt(-168600)`, `sqrtAt(-168800)`: TickMath values (the second is S28's
/// `sqrtP - 2`, the third S32's bound `0xe2a3ff8d70d5dd274e6ba`).
const SQRT_M168500: &str = "17383271292983671704362294";
const SQRT_M168600: &str = "17296576189565919710191919";
const SQRT_M168800: &str = "17124480942577115259528890";
/// `sqrtAt(168500)`, `sqrtAt(168600)` and `sqrtAt(-31400)`: the mirrored position's spot, upper
/// and lower edges for start tick -168600.
const SQRT_168500: &str = "361100142176362277044286757142905";
const SQRT_168600: &str = "362910073449872328385539408603818";

fn observation(ema: U256, updated_at: u64, tick: i32, discount: u64) -> Observation {
    Observation { ema, updated_at, tick, discount }
}

fn bid(inventory: U256) -> Bid {
    Bid { inventory, filled: U256::ZERO, window_start: 0, max_clip: u128::MAX >> 2 }
}

fn no_bid() -> Bid {
    bid(U256::ZERO)
}

fn pool_state(
    token_is_currency0: bool,
    fee_pips: u32,
    observation: Observation,
    bid: Bid,
) -> AscendPoolState {
    AscendPoolState { fee_pips, token_is_currency0, start_tick: -168_600, observation, bid }
}

/// A pool with start tick -168600 and the price at `spot` (tick `tick`), the hook quoting at
/// `timestamp`. The position is the pool's single one, with S7's liquidity.
fn pool(state: AscendPoolState, spot: U256, tick: i32, timestamp: u64) -> UniswapV4State {
    let curve = Curve::new(state.token_is_currency0, state.start_tick).expect("a curve");
    let liquidity = u128::try_from(curve.liquidity).expect("fits");
    let net = i128::try_from(liquidity).expect("fits");
    let (lower, upper) = if state.token_is_currency0 {
        (state.start_tick, state.start_tick + 200_000)
    } else {
        (-(state.start_tick + 200_000), -state.start_tick)
    };
    let active = if (lower..upper).contains(&tick) { liquidity } else { 0 };
    let mut pool = UniswapV4State::new(
        active,
        spot,
        UniswapV4Fees::new(0, 0, DYNAMIC_FEE_FLAG),
        tick,
        200,
        vec![TickInfo::new(lower, net).unwrap(), TickInfo::new(upper, -net).unwrap()],
    )
    .expect("a pool");
    pool.set_hook_handler(Box::new(AscendHookHandler::new(ASCEND_HOOK_ELYSIUM, state, timestamp)));
    pool
}

fn token(byte: u8) -> Token {
    Token::new(
        &Bytes::from(Address::repeat_byte(byte).to_vec()),
        "T",
        18,
        0,
        &[Some(10_000)],
        Chain::Elysium,
        100,
    )
}

/// `(launched token, quote)`, currency0 being the lower address.
fn tokens(token_is_currency0: bool) -> (Token, Token) {
    if token_is_currency0 {
        (token(1), token(2))
    } else {
        (token(2), token(1))
    }
}

fn sell(
    pool: &UniswapV4State,
    token_is_currency0: bool,
    amount: &str,
) -> Result<BigUint, SimulationError> {
    let (launched, quote) = tokens(token_is_currency0);
    pool.get_amount_out(big(amount), &launched, &quote)
        .map(|result| result.amount)
}

fn buy(
    pool: &UniswapV4State,
    token_is_currency0: bool,
    amount: &str,
) -> Result<BigUint, SimulationError> {
    let (launched, quote) = tokens(token_is_currency0);
    pool.get_amount_out(big(amount), &quote, &launched)
        .map(|result| result.amount)
}

fn hook_of(pool: &UniswapV4State) -> &AscendHookHandler {
    pool.hook
        .as_ref()
        .expect("a hook")
        .as_any()
        .downcast_ref::<AscendHookHandler>()
        .expect("an Ascend hook")
}

fn big(decimal: &str) -> BigUint {
    BigUint::from_str(decimal).unwrap()
}

fn attributes(state: &AscendPoolState) -> HashMap<String, Bytes> {
    let u64_attr = |v: u64| Bytes::from(v.to_be_bytes().to_vec());
    [
        (FEE_PIPS_ATTRIBUTE, u64_attr(state.fee_pips.into())),
        (TOKEN_IS_CURRENCY0_ATTRIBUTE, Bytes::from(vec![u8::from(state.token_is_currency0)])),
        (START_TICK_ATTRIBUTE, Bytes::from(state.start_tick.to_be_bytes().to_vec())),
        (
            EMA_SQRT_PRICE_ATTRIBUTE,
            Bytes::from(
                state
                    .observation
                    .ema
                    .to_be_bytes::<32>()[12..]
                    .to_vec(),
            ),
        ),
        (EMA_UPDATED_AT_ATTRIBUTE, u64_attr(state.observation.updated_at)),
        (
            OBSERVED_TICK_ATTRIBUTE,
            Bytes::from(
                state
                    .observation
                    .tick
                    .to_be_bytes()
                    .to_vec(),
            ),
        ),
        (DISCOUNT_ATTRIBUTE, u64_attr(state.observation.discount)),
        (
            BID_INVENTORY_ATTRIBUTE,
            Bytes::from(
                state
                    .bid
                    .inventory
                    .to_be_bytes::<32>()
                    .to_vec(),
            ),
        ),
        (
            BID_FILLED_IN_WINDOW_ATTRIBUTE,
            Bytes::from(
                state
                    .bid
                    .filled
                    .to_be_bytes::<32>()
                    .to_vec(),
            ),
        ),
        (BID_WINDOW_START_ATTRIBUTE, u64_attr(state.bid.window_start)),
        (
            BID_MAX_CLIP_ATTRIBUTE,
            Bytes::from(
                state
                    .bid
                    .max_clip
                    .to_be_bytes()
                    .to_vec(),
            ),
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value))
    .collect()
}

// ---------------------------------------------------------------------------------------------
// Constants and the curve
// ---------------------------------------------------------------------------------------------

#[test]
fn test_launch_supply_is_1e27() {
    assert_eq!(LAUNCH_SUPPLY, U256::from(10u64).pow(U256::from(27)));
}

/// S7's formula for a pool starting at -168600 gives the liquidity the chain holds (S28), for
/// both orientations.
#[rstest]
#[case::token_is_currency0(true)]
#[case::mirrored(false)]
fn test_curve_liquidity_is_what_the_chain_holds(#[case] token_is_currency0: bool) {
    let curve = Curve::new(token_is_currency0, -168_600).expect("a curve");

    assert_eq!(curve.liquidity, U256::from(L_168600));
}

/// S32 (a): at spot `sqrtAt(-168500)` the tokens down to the curve end are
/// 4987498616072757541375877 (lib 0x15821c64 trace at block 8660000).
#[test]
fn test_tokens_to_the_curve_end_match_the_chain() {
    let curve = Curve::new(true, -168_600).unwrap();
    assert_eq!(curve.curve_end, u(SQRT_M168600));

    let to_end =
        amount0_delta(curve.curve_end, u(SQRT_M168500), curve.liquidity, false).expect("a delta");

    assert_eq!(to_end, u("4987498616072757541375877"));
}

#[test]
fn test_curve_bounds_of_both_orientations() {
    let token0 = Curve::new(true, -168_600).unwrap();
    assert_eq!(token0.start_side_bound, u(SQRT_M168800));
    assert_eq!(token0.curve_end, get_sqrt_ratio_at_tick(-168_600).unwrap());
    assert_eq!(token0.far_end, get_sqrt_ratio_at_tick(31_400).unwrap());

    let mirrored = Curve::new(false, -168_600).unwrap();
    assert_eq!(mirrored.start_side_bound, get_sqrt_ratio_at_tick(168_800).unwrap());
    assert_eq!(mirrored.curve_end, u(SQRT_168600));
    assert_eq!(mirrored.far_end, get_sqrt_ratio_at_tick(-31_400).unwrap());
}

// ---------------------------------------------------------------------------------------------
// The observation (S14)
// ---------------------------------------------------------------------------------------------

/// Tx 0xd2cce086... (block 8351072, tokenIs0, EMA rising, dt = 969): the hook's trace shows the
/// EMA 0xd9137f51e679b46a57ec68 -> 0xd9160c7dbd65710dd08a3d in steps of 600 s and 369 s, and the
/// discount 0x2bbf2ed -> ceil 0x2ac97d8 -> 0x2af920e (S14). The spot, 0xd9173164a0a4ad89a19b10,
/// is one the two steps map to that EMA (found by bisection; the steps' floors admit two).
#[test]
fn test_observation_reproduces_the_traced_ema_and_discount() {
    let curve = Curve::new(true, -168_600).unwrap();
    let before = observation(
        U256::from_str_radix("d9137f51e679b46a57ec68", 16).unwrap(),
        1_795_112_640,
        -100_000,
        0x2bbf2ed,
    );
    let spot = U256::from_str_radix("d9173164a0a4ad89a19b10", 16).unwrap();

    let after = observe(&curve, &before, spot, -99_000, 1_795_112_640 + 969).unwrap();

    assert_eq!(after.ema, U256::from_str_radix("d9160c7dbd65710dd08a3d", 16).unwrap());
    assert_eq!(after.discount, 0x2af920e);
    assert_eq!(after.updated_at, 1_795_112_640 + 969);
    assert_eq!(after.tick, -99_000);
}

#[test]
fn test_observation_changes_nothing_in_the_same_second() {
    let curve = Curve::new(true, -168_600).unwrap();
    let before = observation(u("100000000000000000000000000"), TS, -100_000, 1_000);

    let after = observe(&curve, &before, u("90000000000000000000000000"), -50, TS).unwrap();

    assert_eq!(after, before);
}

#[test]
fn test_observation_before_the_last_one_is_refused() {
    let curve = Curve::new(true, -168_600).unwrap();
    let before = observation(u("100000000000000000000000000"), TS, 0, 0);

    let result = observe(&curve, &before, u("100000000000000000000000000"), 0, TS - 1);

    assert!(matches!(result, Err(SimulationError::InvalidInput(..))), "{result:?}");
}

/// Hand-worked, as the spec's rules give them (S14), at dt = 100 s:
/// - EMA 1e26 falling to a spot of 9e25: one step of 100 s moves `floor(1e25 * 100 / 700)`; d1 =
///   ceil(1000 * 43200 / 43300) = 998; x = floor((2^32 - 998) * 1e26 / ema) is about 1.0145
///   * 2^32, above 2^32, so the stored d is the floor 0, not a negative number (S14, S18).
/// - the same with the spot clamped up to `sqrtAt(-168800)` (spot 1e20 is below the range);
/// - mirrored, EMA 1e30 rising to 1.1e30, which floors too;
/// - the plain case: d does not floor: 1e6 -> d1 997691 -> 2^32 - x = 61476137.
#[rstest]
#[case::falling_ema_floors_d(
    true,
    "100000000000000000000000000",
    1_000,
    "90000000000000000000000000",
    "98571428571428571428571429",
    0
)]
#[case::spot_below_the_range_is_clamped(
    true,
    "100000000000000000000000000",
    1_000,
    "100000000000000000000",
    "88160640134653873608504128",
    0
)]
#[case::mirrored_rising_ema_floors_d(
    false,
    "1000000000000000000000000000000",
    1_000,
    "1100000000000000000000000000000",
    "1014285714285714285714285714285",
    0
)]
#[case::rising_ema_keeps_d(
    true,
    "100000000000000000000000000",
    1_000_000,
    "110000000000000000000000000",
    "101428571428571428571428571",
    61_476_137
)]
fn test_observation_floors_the_discount_at_zero(
    #[case] token_is_currency0: bool,
    #[case] ema: &str,
    #[case] discount: u64,
    #[case] spot: &str,
    #[case] expected_ema: &str,
    #[case] expected_discount: u64,
) {
    let curve = Curve::new(token_is_currency0, -168_600).unwrap();
    let before = observation(u(ema), TS - 100, 0, discount);

    let after = observe(&curve, &before, u(spot), 5, TS).unwrap();

    assert_eq!(after.ema, u(expected_ema));
    assert_eq!(after.discount, expected_discount);
}

/// The EMA takes at most 8 steps: after 1e6 s toward a spot at twice the EMA it has closed
/// `1 - 2^-8` of the distance (each step of 600 s halves it), not all of it.
#[test]
fn test_observation_takes_at_most_eight_steps() {
    let curve = Curve::new(true, -168_600).unwrap();
    let before = observation(u("100000000000000000000000000"), TS - 1_000_000, 0, 1_000_000);

    let after = observe(&curve, &before, u("200000000000000000000000000"), 0, TS).unwrap();

    assert_eq!(after.ema, u("199609375000000000000000000"));
    assert_eq!(after.discount, 2_143_301_883);
}

/// dt = 2000 s with the spot at the EMA: four steps move nothing; only d decays,
/// `ceil(1e6 * 43200 / 45200) = 955753`, and the re-anchor leaves `2^32 - x = d1`.
#[test]
fn test_observation_decays_the_discount_with_the_elapsed_time() {
    let curve = Curve::new(true, -168_600).unwrap();
    let before = observation(u("100000000000000000000000000"), TS - 2_000, 0, 1_000_000);

    let after = observe(&curve, &before, u("100000000000000000000000000"), 0, TS).unwrap();

    assert_eq!(after.ema, before.ema);
    assert_eq!(after.discount, 955_753);
}

// ---------------------------------------------------------------------------------------------
// Fill sizing (S16)
// ---------------------------------------------------------------------------------------------

/// A curve with the given end price and liquidity, in the orientation of `token_is_currency0`.
fn curve_of(token_is_currency0: bool, curve_end: U256, liquidity: U256) -> Curve {
    Curve {
        token_is_currency0,
        liquidity,
        curve_end,
        far_end: U256::ZERO,
        start_side_bound: U256::ZERO,
        clamp: (U256::ZERO, U256::MAX),
    }
}

/// The inputs of the Python model's token0 vectors: S28's liquidity, a curve end at
/// `sqrtAt(-168600)`, the spot 0.2% above it, the EMA at the spot and a fill price 0.05% below
/// the EMA.
struct Token0Fixture {
    curve: Curve,
    spot: U256,
    fill_price: U256,
    ema: U256,
}

fn token0_fixture() -> Token0Fixture {
    Token0Fixture {
        curve: curve_of(true, u(SQRT_M168600), U256::from(L_168600)),
        spot: u("17331169341945051549612302"),
        fill_price: u("17322503759888906322764708"),
        ema: u("17331169341945051549612302"),
    }
}

fn token0_request(
    fixture: &Token0Fixture,
    amount_in: U256,
    inventory: U256,
    max_clip: U256,
    filled: U256,
) -> FillRequest {
    FillRequest {
        spot: fixture.spot,
        fill_price: fixture.fill_price,
        ema: fixture.ema,
        amount_in,
        inventory,
        max_clip,
        filled,
    }
}

const T0_TO_FILL_PRICE: &str = "499274149441997029379948";

const HUGE: &str = "10000000000000000000000000000000000000000";

/// Each vector: the input, the inventory, the window's clip and filled, and the Python model's
/// `(tokensIn, quoteOut, newD)` for the token0 fixture. `T` is 499274149441997029379948, so the
/// first input is `T + 1e21`, the second `T + V / 2` and the third the int128 maximum.
#[rstest]
#[case::uncapped(
    "500274149441997029379948",
    HUGE,
    HUGE,
    "0",
    "1000000000000000000000",
    "47803723542469",
    2_151_783
)]
#[case::half_of_the_curve(
    "1247686400766107017066594",
    HUGE,
    HUGE,
    "0",
    "748412251324109987686646",
    "35750133459258891",
    5_362_543
)]
#[case::clamped_to_the_curve_end(
    "170141183460469231731687303715884105727",
    HUGE,
    HUGE,
    "0",
    "1496824502648219975373292",
    "71446757673424275",
    8_572_790
)]
#[case::capped_by_the_inventory(
    "500274149441997029379948",
    "10000000000000",
    HUGE,
    "0",
    "209188559788946367260",
    "9999999999999",
    2_148_383
)]
#[case::capped_by_the_window(
    "500274149441997029379948",
    HUGE,
    "50000000000000",
    "20000000000000",
    "627565942307744545808",
    "29999999999999",
    2_150_181
)]
fn test_token0_fill_sizing_matches_the_model(
    #[case] amount: &str,
    #[case] inventory: &str,
    #[case] max_clip: &str,
    #[case] filled: &str,
    #[case] tokens_in: &str,
    #[case] quote_out: &str,
    #[case] discount: u64,
) {
    let fixture = token0_fixture();

    let fill = size_fill(
        &fixture.curve,
        &token0_request(&fixture, u(amount), u(inventory), u(max_clip), u(filled)),
    )
    .expect("sizes")
    .expect("a fill");

    assert_eq!(fill.tokens_in, u(tokens_in));
    assert_eq!(fill.quote_out, u(quote_out));
    assert_eq!(fill.discount, discount);
}

#[rstest]
#[case::input_at_the_price_move(T0_TO_FILL_PRICE)]
#[case::input_below_the_price_move("1")]
fn test_token0_input_up_to_the_price_move_is_no_fill(#[case] amount: &str) {
    let fixture = token0_fixture();

    let fill = size_fill(
        &fixture.curve,
        &token0_request(
            &fixture,
            u(amount),
            U256::from(10u64).pow(U256::from(40)),
            U256::from(10u64).pow(U256::from(40)),
            U256::ZERO,
        ),
    )
    .expect("sizes");

    assert_eq!(fill, None);
}

#[test]
fn test_token0_fill_price_at_the_curve_end_is_no_fill() {
    let fixture = token0_fixture();
    let mut request = token0_request(
        &fixture,
        U256::from(10u64).pow(U256::from(30)),
        U256::from(10u64).pow(U256::from(40)),
        U256::from(10u64).pow(U256::from(40)),
        U256::ZERO,
    );
    request.fill_price = fixture.curve.curve_end;

    assert_eq!(size_fill(&fixture.curve, &request).expect("sizes"), None);
}

#[test]
fn test_token0_spot_outside_the_band_is_no_fill() {
    let fixture = token0_fixture();
    let mut request = token0_request(
        &fixture,
        U256::from(10u64).pow(U256::from(30)),
        U256::from(10u64).pow(U256::from(40)),
        U256::from(10u64).pow(U256::from(40)),
        U256::ZERO,
    );
    // The band is `floor(ema * 500 / 20000)`, 2.5% each way: an EMA twice the spot is outside it.
    request.ema = fixture.ema * U256::from(2);

    assert_eq!(size_fill(&fixture.curve, &request).expect("sizes"), None);
}

#[test]
fn test_a_filled_window_is_no_fill() {
    let fixture = token0_fixture();
    let request = token0_request(
        &fixture,
        U256::from(10u64).pow(U256::from(30)),
        U256::from(10u64).pow(U256::from(40)),
        U256::from(50_000_000_000_000u64),
        U256::from(50_000_000_000_000u64),
    );

    assert_eq!(size_fill(&fixture.curve, &request).expect("sizes"), None);
}

/// A spot below the curve end with X above it is the one state the spec does not settle; it is
/// refused, not quoted.
#[test]
fn test_token0_spot_below_the_curve_end_is_refused() {
    let fixture = token0_fixture();
    let mut request = token0_request(
        &fixture,
        U256::from(10u64).pow(U256::from(30)),
        U256::from(10u64).pow(U256::from(40)),
        U256::from(10u64).pow(U256::from(40)),
        U256::ZERO,
    );
    request.spot = fixture.curve.curve_end - U256::from(1);
    request.ema = request.spot;

    let result = size_fill(&fixture.curve, &request);

    assert!(matches!(result, Err(SimulationError::InvalidInput(..))), "{result:?}");
}

/// The mirrored vectors of the Python model: liquidity 1e23, curve end 1e28, spot 0.2% below it,
/// EMA at the spot and a fill price 0.05% above the EMA. `T` is 6301414272978079463.
fn mirrored_fixture() -> (Curve, FillRequest) {
    let curve = curve_of(false, u("10000000000000000000000000000"), u("100000000000000000000000"));
    let request = FillRequest {
        spot: u("9980000000000000000000000000"),
        fill_price: u("9984992494740892121390327734"),
        ema: u("9980000000000000000000000000"),
        amount_in: U256::ZERO,
        inventory: U256::from(10u64).pow(U256::from(40)),
        max_clip: U256::from(10u64).pow(U256::from(40)),
        filled: U256::ZERO,
    };
    (curve, request)
}

#[rstest]
#[case::uncapped(
    "5000000000000000000",
    "10000000000000000000000000000000000000000",
    "5000000000000000000",
    "314674405303738053758",
    3_849_925
)]
#[case::clamped_to_the_curve_end(
    "100000000000000000000",
    "10000000000000000000000000000000000000000",
    "18942134694094298311",
    "1190804165580026714939",
    8_589_935
)]
#[case::capped_by_the_inventory(
    "100000000000000000000",
    "100000000",
    "1588313",
    "99999948",
    2_147_484
)]
fn test_mirrored_fill_sizing_matches_the_model(
    #[case] extra_input: &str,
    #[case] inventory: &str,
    #[case] tokens_in: &str,
    #[case] quote_out: &str,
    #[case] discount: u64,
) {
    let (curve, mut request) = mirrored_fixture();
    request.amount_in = u("6301414272978079463") + u(extra_input);
    request.inventory = u(inventory);

    let fill = size_fill(&curve, &request)
        .expect("sizes")
        .expect("a fill");

    assert_eq!(fill.tokens_in, u(tokens_in));
    assert_eq!(fill.quote_out, u(quote_out));
    assert_eq!(fill.discount, discount);
}

#[test]
fn test_mirrored_fill_price_at_the_curve_end_is_no_fill() {
    let (curve, mut request) = mirrored_fixture();
    request.amount_in = U256::from(10u64).pow(U256::from(30));
    request.fill_price = curve.curve_end;

    assert_eq!(size_fill(&curve, &request).expect("sizes"), None);
}

// ---------------------------------------------------------------------------------------------
// Fees (S11, S13)
// ---------------------------------------------------------------------------------------------

/// Tx 0xe607911e... (S11): total in 0xeff84958f86b75, the specified delta 0x026652a74a2a09 is
/// `ceil(1%)`.
#[test]
fn test_a_buys_fee_is_the_ceiling_of_the_input_share() {
    let amount = U256::from_str_radix("eff84958f86b75", 16).unwrap();

    assert_eq!(
        fee_ceil(amount, 10_000).unwrap(),
        U256::from_str_radix("026652a74a2a09", 16).unwrap()
    );
}

/// Tx 0xd2cce086... (S13): the fill fee 0x2171adf6b561a is `floor(1%)` of the fill's quote
/// 0xd1067f45eda23a, the afterSwap fee 0x7b5521ee20e9f95 is `ceil(1%)` of the AMM output
/// 0x302d414104db655dd, and the user got `(quote - fill fee) + out - afterSwap fee` =
/// 0x2fbedb155d22a0268.
#[test]
fn test_a_fill_splits_the_output_as_on_chain() {
    let hex = |s: &str| U256::from_str_radix(s, 16).unwrap();
    let quote = hex("d1067f45eda23a");
    let amm_out = hex("302d414104db655dd");

    let fill_fee = fee_floor(quote, 10_000).unwrap();
    let after_fee = fee_ceil(amm_out, 10_000).unwrap();

    assert_eq!(fill_fee, hex("2171adf6b561a"));
    assert_eq!(after_fee, hex("7b5521ee20e9f95"));
    assert_eq!(quote - fill_fee + amm_out - after_fee, hex("2fbedb155d22a0268"));
}

#[rstest]
fn test_max_gross_buy_is_the_largest_fitting_input(
    #[values(0, 1, 9_999, 10_000, 30_000, 110_000, 999_999)] fee_pips: u32,
    #[values(0, 1, 99, 1_000_000, 123_456_789_012_345_678_901, u128::MAX)] max_net: u128,
) {
    let max_net = U256::from(max_net);
    // The net of a gross input: `A - ceil(A * f / 1e6)`.
    let net =
        |gross: U256| gross - (gross * U256::from(fee_pips)).div_ceil(U256::from(1_000_000u32));
    let gross = max_gross_buy(max_net, fee_pips).expect("fits");

    assert!(net(gross) <= max_net, "{gross} at {fee_pips}");
    assert!(net(gross + U256::from(1)) > max_net, "{gross} + 1 at {fee_pips}");
}

// ---------------------------------------------------------------------------------------------
// Quotes through the state
// ---------------------------------------------------------------------------------------------

/// S32 (a): V4Quoter at block 8660000 (time 1795168252) on pool 0xc0be (token0, fee 1%) with the
/// price at tick -168500 and a discount so deep that X is clamped (no fill) quotes a sell of 1e21
/// as the AMM alone does: 47658306588964.
#[test]
fn test_a_sell_with_a_clamped_fill_price_quotes_the_amm_alone_on_a_token0_pool() {
    let ema = u(SQRT_M168500);
    let state = pool_state(
        true,
        10_000,
        observation(ema, TS, -168_500, 0xf000_0000),
        bid(U256::from(10u64).pow(U256::from(30))),
    );
    let pool = pool(state, u(SQRT_M168500), -168_500, TS);

    assert_eq!(sell(&pool, true, "1000000000000000000000").unwrap(), big("47658306588964"));
}

/// The same on pool 0x37a9 (mirrored, fee 2%), the price at tick 168500: 47176909552712.
#[test]
fn test_a_sell_with_a_clamped_fill_price_quotes_the_amm_alone_on_a_mirrored_pool() {
    let ema = u(SQRT_168500);
    let state = pool_state(
        false,
        20_000,
        observation(ema, TS, 168_500, 0xf000_0000),
        bid(U256::from(10u64).pow(U256::from(30))),
    );
    let pool = pool(state, u(SQRT_168500), 168_500, TS);

    assert_eq!(sell(&pool, false, "1000000000000000000000").unwrap(), big("47176909552712"));
}

/// A funded pool whose price is at tick -168500, the EMA 0.2% above it and `d = 2147483`: the fill
/// price is above the spot, so `T = 0` and a small sell fills entirely at the spot (the Python
/// model: 1e18 tokens -> quote 48139751956, newD 8572794).
fn funded_token0_pool(inventory: &str, timestamp: u64) -> UniswapV4State {
    let state = pool_state(
        true,
        10_000,
        observation(u("17418037835569639047771018"), TS, -168_500, 2_147_483),
        bid(u(inventory)),
    );
    pool(state, u(SQRT_M168500), -168_500, timestamp)
}

const PLENTY: &str = "1000000000000000000000000000000";

/// Fills as the model sizes them (S16 at the S32 anchor: the quote at `A = V` is the venue's
/// 238899517702555132), with the swapper's share `quote - floor(quote * 1%)`.
#[rstest]
#[case::the_fill_takes_all_of_the_input("1000000000000000000", "47658354437")]
#[case::the_fill_runs_to_the_curve_end_and_the_amm_takes_the_rest(
    "4988498616072757541375877",
    "236558180832118545"
)]
fn test_a_sell_fills_from_the_bid_on_a_token0_pool(#[case] amount: &str, #[case] expected: &str) {
    let pool = funded_token0_pool(PLENTY, TS);

    assert_eq!(sell(&pool, true, amount).unwrap(), big(expected));
}

/// A sell that fills nothing is the AMM's: the same pool without inventory quotes the S32
/// anchor's AMM-only 47658306588964 for 1e21, below what the fill gives at the spot.
#[test]
fn test_a_sell_without_inventory_is_the_amms() {
    let pool = funded_token0_pool("0", TS);

    assert_eq!(sell(&pool, true, "1000000000000000000000").unwrap(), big("47658306588964"));
}

/// Mirrored fill at `A = V + 5` (S17: the quote at `A = V` is 238899517702555132 on both pools),
/// fee 2%: the swapper gets `238899517702555132 - floor(2%)`, and the pool takes the 5 left over
/// at no output.
#[test]
fn test_a_sell_fills_from_the_bid_on_a_mirrored_pool() {
    let state = pool_state(
        false,
        20_000,
        observation(u("360377941892009552490198183628619"), TS, 168_500, 2_147_483),
        bid(u(PLENTY)),
    );
    let pool = pool(state, u(SQRT_168500), 168_500, TS);

    assert_eq!(sell(&pool, false, "4987498616072757541375877").unwrap(), big("234121527348504030"));
}

fn before_swap_delta(
    pool: &UniswapV4State,
    zero_for_one: bool,
    amount: i128,
    spot: U256,
    tick: i32,
) -> (I128, I128, u64) {
    let params = BeforeSwapParameters {
        context: StateContext {
            currency_0: Address::repeat_byte(1),
            currency_1: Address::repeat_byte(2),
            fees: UniswapV4Fees::new(0, 0, DYNAMIC_FEE_FLAG),
            tick_spacing: 200,
            sqrt_price_x96: spot,
            tick,
            liquidity: 0,
        },
        sender: Address::ZERO,
        swap_params: SwapParams {
            zero_for_one,
            amount_specified: I256::try_from(amount).unwrap(),
            sqrt_price_limit: U256::ZERO,
        },
        hook_data: Bytes::new(),
    };
    let output = hook_of(pool)
        .before_swap(params, None, None)
        .expect("a hook output");
    (
        output
            .result
            .amount_delta
            .get_specified_delta(),
        output
            .result
            .amount_delta
            .get_unspecified_delta(),
        output.gas_estimate,
    )
}

/// The delta of a capped fill: with 1e9 of quote in the bid, the cap `min(per - filled, inventory)
/// - 1` binds and the model gives tokensIn 20772853149254418 for quoteOut 999999999; the swapper's
///   share is 990000000 after the 1% fill fee.
#[test]
fn test_a_capped_fill_takes_tokens_and_pays_the_quote_net_of_its_fee() {
    let pool = funded_token0_pool("1000000000", TS);

    let (specified, unspecified, gas) =
        before_swap_delta(&pool, true, -1_000_000_000_000_000_000, u(SQRT_M168500), -168_500);

    assert_eq!(specified, I128::try_from(20_772_853_149_254_418i128).unwrap());
    assert_eq!(unspecified, I128::try_from(-990_000_000i128).unwrap());
    assert_eq!(gas, ASCEND_FILL_BEFORE_SWAP_GAS);
}

/// A buy's beforeSwap takes `ceil(A * f / 1e6)` of the input as a positive specified delta (S11).
#[test]
fn test_a_buy_takes_its_fee_in_before_swap() {
    let pool = funded_token0_pool("0", TS);

    let (specified, unspecified, _) =
        before_swap_delta(&pool, false, -67_545_513_342_823_285, u(SQRT_M168500), -168_500);

    assert_eq!(specified, I128::try_from(675_455_133_428_233i128).unwrap());
    assert_eq!(unspecified, I128::ZERO);
}

/// A buy pays the fee out of its input and the AMM swaps the rest; the output is the AMM's.
#[test]
fn test_a_buy_quotes_the_amm_on_the_net_input() {
    let pool = funded_token0_pool("0", TS);
    // The same net swapped on a pool whose hook charges no fee.
    let free = {
        let mut free = funded_token0_pool("0", TS);
        let state = pool_state(
            true,
            0,
            observation(u("17418037835569639047771018"), TS, -168_500, 2_147_483),
            no_bid(),
        );
        free.set_hook_handler(Box::new(AscendHookHandler::new(ASCEND_HOOK_ELYSIUM, state, TS)));
        free
    };

    let with_fee = buy(&pool, true, "1010101010101010102").unwrap();
    let without_fee = buy(&free, true, "1000000000000000000").unwrap();

    // 1010101010101010102 - ceil(1010101010101010102 * 0.01) = 1e18 exactly, by hand:
    // the fee is 10101010101010102 (the share 10101010101010101.02 rounded up).
    assert_eq!(with_fee, without_fee);
    assert!(with_fee > BigUint::from(0u32));
}

// ---------------------------------------------------------------------------------------------
// Refusals (S28, D12)
// ---------------------------------------------------------------------------------------------

fn assert_refused(result: &Result<BigUint, SimulationError>, label: &str) {
    assert!(matches!(result, Err(SimulationError::InvalidInput(..))), "{label}: {result:?}");
}

/// Pool 0x1aaf... at block 8397596 (S28): sqrtP = sqrtAt(-168600) + 2, tick -168600, active
/// liquidity L, so the in-range step yields 0 and the swap crosses into the empty range. V4Quoter
/// reverts NotEnoughLiquidity on a sell of 1e6 or 1e18; a buy of 1e15 quotes nonzero.
#[test]
fn test_a_pool_sold_back_to_its_lower_edge_refuses_sells_and_quotes_buys() {
    let spot = u("17296576189565919710191921");
    let state = pool_state(true, 10_000, observation(spot, TS, -168_600, 0), no_bid());
    // The tick -168600 is the position's lower tick, so `pool` gives it the full liquidity, as
    // the chain stores it.
    let edge = pool(state, spot, -168_600, TS);

    assert_refused(&sell(&edge, true, "1000000"), "sell 1e6");
    assert_refused(&sell(&edge, true, "1000000000000000000"), "sell 1e18");
    assert!(buy(&edge, true, "1000000000000000").unwrap() > BigUint::from(0u32));
}

/// A pool at its start price has no liquidity on the sell side: S28's `0x057fcb64 (L=0)`.
#[rstest]
#[case::token0(true, SQRT_M168800, -168_800)]
#[case::mirrored(false, "366557197058144561653914105585283", 168_800)]
fn test_a_pool_at_its_start_price_refuses_sells_and_has_a_zero_sell_limit(
    #[case] token_is_currency0: bool,
    #[case] spot: &str,
    #[case] tick: i32,
) {
    let state = pool_state(token_is_currency0, 10_000, observation(u(spot), TS, tick, 0), no_bid());
    let pool = pool(state, u(spot), tick, TS);
    let (launched, quote) = tokens(token_is_currency0);

    assert_refused(&sell(&pool, token_is_currency0, "1000000000000000000"), "sell");
    assert_eq!(
        pool.get_limits(launched.address, quote.address)
            .unwrap()
            .0,
        BigUint::from(0u32)
    );
    assert!(buy(&pool, token_is_currency0, "1000000000000000000").unwrap() > BigUint::from(0u32));
}

#[rstest]
#[case::zero(I256::ZERO)]
#[case::exact_output(I256::ONE)]
#[case::above_int128(-I256::from_raw(U256::from(u128::MAX >> 1) + U256::from(1)))]
fn test_before_swap_refuses_what_the_pool_reverts(#[case] amount: I256) {
    let pool = funded_token0_pool("0", TS);
    let params = BeforeSwapParameters {
        context: StateContext {
            currency_0: Address::repeat_byte(1),
            currency_1: Address::repeat_byte(2),
            fees: UniswapV4Fees::new(0, 0, DYNAMIC_FEE_FLAG),
            tick_spacing: 200,
            sqrt_price_x96: u(SQRT_M168500),
            tick: -168_500,
            liquidity: L_168600,
        },
        sender: Address::ZERO,
        swap_params: SwapParams {
            zero_for_one: true,
            amount_specified: amount,
            sqrt_price_limit: U256::ZERO,
        },
        hook_data: Bytes::new(),
    };

    let result = hook_of(&pool).before_swap(params, None, None);

    assert!(matches!(result, Err(SimulationError::InvalidInput(..))), "{result:?}");
}

#[test]
fn test_an_unfilled_swap_is_refused_and_a_filled_one_is_not() {
    let pool = funded_token0_pool("0", TS);
    let hook = hook_of(&pool);
    let outcome = |unfilled: u64| SwapOutcome {
        sqrt_price_x96: U256::ZERO,
        amount_unfilled: U256::from(unfilled),
    };

    assert!(hook
        .check_swap_outcome(true, &outcome(0))
        .is_ok());
    assert!(matches!(
        hook.check_swap_outcome(true, &outcome(1)),
        Err(SimulationError::InvalidInput(..))
    ));
}

// ---------------------------------------------------------------------------------------------
// Limits (D13): the limit settles and one unit more is refused
// ---------------------------------------------------------------------------------------------

fn assert_limit_is_exact(pool: &UniswapV4State, token_is_currency0: bool, quote_in: bool) {
    let (launched, quote) = tokens(token_is_currency0);
    let (token_in, token_out) = if quote_in { (quote, launched) } else { (launched, quote) };
    let (max_in, max_out) = pool
        .get_limits(token_in.address.clone(), token_out.address.clone())
        .expect("limits");
    assert!(max_in > BigUint::from(0u32), "a zero limit");
    let at = pool
        .get_amount_out(max_in.clone(), &token_in, &token_out)
        .expect("the limit settles");
    assert_eq!(at.amount, max_out);
    let past = pool.get_amount_out(&max_in + 1u32, &token_in, &token_out);
    assert!(
        matches!(past, Err(SimulationError::InvalidInput(..))),
        "one past the limit: {:?}",
        past.map(|r| r.amount)
    );
}

#[rstest]
#[case::buy("0", false)]
#[case::sell_without_inventory("0", true)]
#[case::sell_with_a_funded_bid(PLENTY, true)]
#[case::sell_capped_by_the_inventory("1000000000", true)]
fn test_limits_of_a_token0_pool_settle_and_one_more_unit_is_refused(
    #[case] inventory: &str,
    #[case] is_sell: bool,
) {
    let pool = funded_token0_pool(inventory, TS);

    assert_limit_is_exact(&pool, true, !is_sell);
}

#[rstest]
#[case::buy("0", false)]
#[case::sell_without_inventory("0", true)]
#[case::sell_with_a_funded_bid(PLENTY, true)]
fn test_limits_of_a_mirrored_pool_settle_and_one_more_unit_is_refused(
    #[case] inventory: &str,
    #[case] is_sell: bool,
) {
    let state = pool_state(
        false,
        20_000,
        observation(u("360377941892009552490198183628619"), TS, 168_500, 2_147_483),
        bid(u(inventory)),
    );
    let pool = pool(state, u(SQRT_168500), 168_500, TS);

    assert_limit_is_exact(&pool, false, !is_sell);
}

/// A pool at its start price: a buy fits the whole position, from the empty range.
#[rstest]
#[case::token0(true, SQRT_M168800, -168_800)]
#[case::mirrored(false, "366557197058144561653914105585283", 168_800)]
fn test_the_buy_limit_of_a_pool_at_its_start_price_settles(
    #[case] token_is_currency0: bool,
    #[case] spot: &str,
    #[case] tick: i32,
) {
    let state = pool_state(token_is_currency0, 10_000, observation(u(spot), TS, tick, 0), no_bid());
    let pool = pool(state, u(spot), tick, TS);

    assert_limit_is_exact(&pool, token_is_currency0, true);
}

// ---------------------------------------------------------------------------------------------
// State carried by a quote (D11) and time (D10, D15)
// ---------------------------------------------------------------------------------------------

/// A fill consumes the inventory, fills the window, opens it at the block's time and moves the
/// discount to the model's `newD`; the pool's price is the AMM's alone.
#[test]
fn test_a_fill_updates_the_hook_in_the_returned_state() {
    let pool = funded_token0_pool(PLENTY, TS);
    let (launched, quote) = tokens(true);

    let result = pool
        .get_amount_out(big("1000000000000000000"), &launched, &quote)
        .unwrap();
    let next = result
        .new_state
        .as_any()
        .downcast_ref::<UniswapV4State>()
        .unwrap();
    let hook = hook_of(next).pool_state();

    assert_eq!(hook.bid.inventory, u(PLENTY) - U256::from(48_139_751_956u64));
    assert_eq!(hook.bid.filled, U256::from(48_139_751_956u64));
    assert_eq!(hook.bid.window_start, TS);
    assert_eq!(hook.observation.discount, 8_572_794);
    // The old state is untouched.
    assert_eq!(
        hook_of(&pool)
            .pool_state()
            .bid
            .inventory,
        u(PLENTY)
    );
}

/// A swap observes the price even without a fill, so the next quote of a route reads `dt = 0`
/// and the new tick: after a buy the stored observed tick is the one the buy started from, not
/// the one it ended at, and a sell in the same block does not fill (S15).
#[test]
fn test_a_sell_after_a_buy_in_the_same_block_does_not_fill() {
    let funded = funded_token0_pool(PLENTY, TS);
    let empty = funded_token0_pool("0", TS);
    let (launched, quote) = tokens(true);
    let after_buy = |pool: &UniswapV4State| -> Box<dyn ProtocolSim> {
        pool.get_amount_out(big("1000000000000000000000"), &quote, &launched)
            .unwrap()
            .new_state
    };
    let (funded_after, empty_after) = (after_buy(&funded), after_buy(&empty));

    let with_bid = funded_after
        .get_amount_out(big("1000000000000000000"), &launched, &quote)
        .unwrap()
        .amount;
    let without_bid = empty_after
        .get_amount_out(big("1000000000000000000"), &launched, &quote)
        .unwrap()
        .amount;

    assert_eq!(with_bid, without_bid);
    // The same sell with no buy before it fills.
    assert_ne!(sell(&funded, true, "1000000000000000000").unwrap(), without_bid);
}

/// With the stored observed tick one off the pool's, a quote at the same second rejects the fill
/// (S15), and a quote one second later re-observes and fills (S15, D15): the same state quotes
/// differently when the block moves, which is why `apply_block` reports a change.
#[test]
fn test_a_funded_pool_quotes_differently_one_second_later() {
    let state = pool_state(
        true,
        10_000,
        observation(u("17418037835569639047771018"), TS, -168_501, 2_147_483),
        bid(u(PLENTY)),
    );
    let mut pool = pool(state, u(SQRT_M168500), -168_500, TS);
    let now = sell(&pool, true, "1000000000000000000").unwrap();

    assert!(ProtocolSim::apply_block(&mut pool, &BlockContext::new(1, TS + 1)));
    let later = sell(&pool, true, "1000000000000000000").unwrap();

    // The Python model: the AMM alone gives 47658354435 for 1e18; at +1 s the observation
    // (EMA step 1 s, d decay, re-anchor floor 2133177) lets the fill take the whole input for
    // quote 48139751956, which pays 47658354437 after the 1% fee.
    assert_eq!(now, big("47658354435"));
    assert_eq!(later, big("47658354437"));
}

#[test]
fn test_apply_block_reports_a_change_only_for_a_pool_with_inventory() {
    let mut funded = funded_token0_pool(PLENTY, TS);
    let mut empty = funded_token0_pool("0", TS);

    assert!(!ProtocolSim::apply_block(&mut funded, &BlockContext::new(1, TS)));
    assert!(ProtocolSim::apply_block(&mut funded, &BlockContext::new(2, TS + 1)));
    assert_eq!(hook_of(&funded).block_timestamp(), TS + 1);
    assert!(!ProtocolSim::apply_block(&mut empty, &BlockContext::new(2, TS + 1)));
    assert_eq!(hook_of(&empty).block_timestamp(), TS + 1);
}

// ---------------------------------------------------------------------------------------------
// Attributes, deltas, decoding and registration (D12, agreements)
// ---------------------------------------------------------------------------------------------

fn sample_state() -> AscendPoolState {
    pool_state(
        true,
        30_000,
        observation(u("17418037835569639047771018"), TS - 5, -168_500, 2_147_483),
        Bid {
            inventory: U256::from(7u64),
            filled: U256::from(3u64),
            window_start: TS - 20,
            max_clip: 11_862_044_423_356_365_469,
        },
    )
}

#[test]
fn test_attributes_decode_in_the_indexers_formats() {
    let decoded = AscendPoolState::from_attributes(&attributes(&sample_state())).unwrap();

    assert_eq!(decoded, sample_state());
}

/// Pool 0xa138...'s attributes as the package decodes them at block 8365800 (the indexing
/// round's `PoolData`/`Bid` fixtures): fee 10000, tokenIs0 0, start tick -168600.
#[test]
fn test_attribute_bytes_of_a_chain_pool_decode() {
    let mut attrs = attributes(&sample_state());
    attrs.insert(FEE_PIPS_ATTRIBUTE.into(), Bytes::from(10_000u64.to_be_bytes().to_vec()));
    attrs.insert(TOKEN_IS_CURRENCY0_ATTRIBUTE.into(), Bytes::from(vec![0]));
    attrs.insert(START_TICK_ATTRIBUTE.into(), Bytes::from(vec![0xff, 0xfd, 0x6d, 0x68]));
    attrs.insert(
        BID_MAX_CLIP_ATTRIBUTE.into(),
        Bytes::from(vec![0, 0, 0, 0, 0, 0, 0, 0, 0xa4, 0x9e, 0x72, 0x8e, 0x24, 0x0f, 0x3a, 0x9d]),
    );

    let decoded = AscendPoolState::from_attributes(&attrs).unwrap();

    assert_eq!(decoded.fee_pips, 10_000);
    assert!(!decoded.token_is_currency0);
    assert_eq!(decoded.start_tick, -168_600);
    assert_eq!(decoded.bid.max_clip, 11_862_044_423_356_365_469);
}

#[test]
fn test_every_ascend_attribute_is_required() {
    for name in POOL_ATTRIBUTES {
        let mut attrs = attributes(&sample_state());
        attrs.remove(name);

        let result = AscendPoolState::from_attributes(&attrs);

        assert!(
            matches!(&result, Err(InvalidSnapshotError::MissingAttribute(missing)) if missing == name),
            "{name}: {result:?}"
        );
    }
}

#[rstest]
#[case::fee_of_100_percent(FEE_PIPS_ATTRIBUTE, 1_000_000u64.to_be_bytes().to_vec())]
#[case::orientation_2(TOKEN_IS_CURRENCY0_ATTRIBUTE, vec![2])]
#[case::short_tick(START_TICK_ATTRIBUTE, vec![0xff, 0xfd, 0x6d])]
#[case::tick_beyond_int24(OBSERVED_TICK_ATTRIBUTE, 0x0080_0000i32.to_be_bytes().to_vec())]
#[case::discount_beyond_uint32(DISCOUNT_ATTRIBUTE, (1u64 << 32).to_be_bytes().to_vec())]
#[case::short_ema(EMA_SQRT_PRICE_ATTRIBUTE, vec![1; 19])]
#[case::short_inventory(BID_INVENTORY_ATTRIBUTE, vec![1; 31])]
#[case::short_clip(BID_MAX_CLIP_ATTRIBUTE, vec![1; 15])]
fn test_values_the_hook_could_not_have_stored_are_rejected_at_decode(
    #[case] name: &str,
    #[case] value: Vec<u8>,
) {
    let mut attrs = attributes(&sample_state());
    attrs.insert(name.to_string(), Bytes::from(value));

    let result = AscendPoolState::from_attributes(&attrs);

    assert!(matches!(result, Err(InvalidSnapshotError::ValueError(_))), "{name}: {result:?}");
}

fn transition(
    handler: &mut AscendHookHandler,
    updated: HashMap<String, Bytes>,
    deleted: &[&str],
) -> Result<(), TransitionError> {
    let delta = ProtocolStateDelta {
        component_id: "0x".into(),
        updated_attributes: updated,
        deleted_attributes: deleted
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    };
    handler.delta_transition(delta, &HashMap::new(), &Balances::default())
}

#[test]
fn test_a_delta_updates_only_the_attributes_it_carries() {
    let mut handler = AscendHookHandler::new(ASCEND_HOOK_ELYSIUM, sample_state(), TS);
    let updated: HashMap<String, Bytes> = [
        (
            BID_INVENTORY_ATTRIBUTE.to_string(),
            Bytes::from(
                U256::from(99u64)
                    .to_be_bytes::<32>()
                    .to_vec(),
            ),
        ),
        (DISCOUNT_ATTRIBUTE.to_string(), Bytes::from(5u64.to_be_bytes().to_vec())),
        ("liquidity".to_string(), Bytes::from(vec![1])),
    ]
    .into();

    transition(&mut handler, updated, &[]).unwrap();

    let mut expected = sample_state();
    expected.bid.inventory = U256::from(99u64);
    expected.observation.discount = 5;
    assert_eq!(*handler.pool_state(), expected);
}

#[test]
fn test_a_bad_delta_leaves_the_handler_unchanged() {
    let mut handler = AscendHookHandler::new(ASCEND_HOOK_ELYSIUM, sample_state(), TS);
    let updated: HashMap<String, Bytes> = [
        (
            BID_INVENTORY_ATTRIBUTE.to_string(),
            Bytes::from(
                U256::from(99u64)
                    .to_be_bytes::<32>()
                    .to_vec(),
            ),
        ),
        (START_TICK_ATTRIBUTE.to_string(), Bytes::from(vec![1])),
    ]
    .into();

    assert!(transition(&mut handler, updated, &[]).is_err());
    assert!(transition(&mut handler, HashMap::new(), &[FEE_PIPS_ATTRIBUTE]).is_err());

    assert_eq!(*handler.pool_state(), sample_state());
}

#[test]
fn test_the_creator_builds_a_handler_at_the_decoded_blocks_timestamp() {
    let snapshot = BareSnapshot::with_attributes(attributes(&sample_state()));

    let handler = AscendHookCreator
        .instantiate_hook_handler(
            snapshot
                .params(ASCEND_HOOK_ELYSIUM)
                .with_block_timestamp(TS),
        )
        .expect("a handler");

    let handler = handler
        .as_any()
        .downcast_ref::<AscendHookHandler>()
        .unwrap();
    assert_eq!(*handler.pool_state(), sample_state());
    assert_eq!(handler.block_timestamp(), TS);
}

#[test]
fn test_the_creator_needs_the_blocks_timestamp_and_every_attribute() {
    let snapshot = BareSnapshot::with_attributes(attributes(&sample_state()));
    let no_time = AscendHookCreator.instantiate_hook_handler(snapshot.params(ASCEND_HOOK_ELYSIUM));
    assert!(matches!(no_time, Err(InvalidSnapshotError::ValueError(_))));

    let bare = BareSnapshot::default();
    let no_attributes = AscendHookCreator.instantiate_hook_handler(
        bare.params(ASCEND_HOOK_ELYSIUM)
            .with_block_timestamp(TS),
    );
    assert!(matches!(no_attributes, Err(InvalidSnapshotError::MissingAttribute(_))));
}

#[test]
fn test_the_hook_is_registered_on_elysium_only() {
    initialize_hook_handlers().expect("registration");

    assert!(has_native_hook_handler(Chain::Elysium, &ASCEND_HOOK_ELYSIUM));
    assert!(!has_native_hook_handler(Chain::Ethereum, &ASCEND_HOOK_ELYSIUM));
}

/// The address bits of the hook grant `beforeSwap`, `afterSwap` and both return deltas (S2:
/// 0x28ec), which the generic permission check reads.
#[test]
fn test_the_hook_address_grants_the_swap_callbacks() {
    use crate::evm::protocol::uniswap_v4::hooks::utils::HookOptions;
    let handler = AscendHookHandler::new(ASCEND_HOOK_ELYSIUM, sample_state(), TS);

    for option in [
        HookOptions::BeforeSwap,
        HookOptions::AfterSwap,
        HookOptions::BeforeSwapReturnsDelta,
        HookOptions::AfterSwapReturnsDelta,
    ] {
        assert!(handler.has_permission(option));
    }
}
