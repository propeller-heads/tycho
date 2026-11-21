//! Tests of the Ascend hook on its own and through `UniswapV4State`.
//!
//! Expected values come from outside the code under test: Elysium chain data (V4Quoter quotes,
//! traces and storage reads of the deployed hook, and the deployed math library's own outputs
//! read with `eth_call`), named by block or transaction in each test. A test that states no
//! source holds a number worked by hand beside it.
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
    hook_handler::{fee_ceil, fee_floor, max_gross_buy, AscendHookHandler, ASCEND_HOOK_ELYSIUM},
    hook_handler_creator::AscendHookCreator,
    math::{amount0_delta_down, observe, size_fill, tokens_for_capped_quote, Curve, FillRequest},
    *,
};
use crate::{
    evm::protocol::{
        u256_num::u256_to_biguint,
        uniswap_v4::{
            hooks::{
                hook_handler::HookHandler,
                hook_handler_creator::{
                    has_native_hook_handler, initialize_hook_handlers, test_fixtures::BareSnapshot,
                    HookHandlerCreator,
                },
                models::{
                    AfterSwapParameters, BalanceDelta, BeforeSwapParameters, StateContext,
                    SwapOutcome, SwapParams,
                },
                test_hook::ExtensionTestHook,
            },
            state::{UniswapV4Fees, UniswapV4State},
        },
        utils::uniswap::{
            lp_fee::DYNAMIC_FEE_FLAG, tick_list::TickInfo, tick_math::get_sqrt_ratio_at_tick,
        },
    },
    protocol::errors::InvalidSnapshotError,
};

/// The timestamp of block 8660000 on Elysium, at which the chain quotes below were read.
const TS: u64 = 1_795_168_252;

/// `L` of every launch with start tick -168600, read from the chain: the active liquidity of pool
/// 0x1aaf6a3f4b78f09f65edddd3921057733adcf582401ab3da4ac16f4d865c3644 at block 8397596, equal to
/// the launch formula's.
const L_168600: u128 = 218_323_400_825_907_916_615_201;

fn u(decimal: &str) -> U256 {
    U256::from_str(decimal).expect("a decimal U256")
}

/// `sqrtAt(-168500)`: the spot of the token0 pools the quote tests start from.
const SQRT_M168500: &str = "17383271292983671704362294";
/// `sqrtAt(-168600)`: the token0 curve end (the price of pool
/// 0x1aaf6a3f4b78f09f65edddd3921057733adcf582401ab3da4ac16f4d865c3644 at block 8397596 less 2).
const SQRT_M168600: &str = "17296576189565919710191919";
/// `sqrtAt(-168800)`: the bound the hook's library receives for the token0 fill price,
/// `0xe2a3ff8d70d5dd274e6ba` scaled.
const SQRT_M168800: &str = "17124480942577115259528890";
/// `sqrtAt(168500)`: the spot of the mirrored pools the quote tests start from.
const SQRT_168500: &str = "361100142176362277044286757142905";
/// `sqrtAt(168600)`: the mirrored curve end.
const SQRT_168600: &str = "362910073449872328385539408603818";

fn observation(ema: U256, updated_at: u64, tick: i32, discount: u32) -> Observation {
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
    AscendPoolState::try_new(fee_pips, token_is_currency0, -168_600, observation, bid)
        .expect("a state the hook could have stored")
}

/// A pool with start tick -168600 and the price at `spot` (tick `tick`), the hook quoting at
/// `timestamp`. The position is the pool's single one, with the launch liquidity.
fn pool(state: AscendPoolState, spot: U256, tick: i32, timestamp: u64) -> UniswapV4State {
    let liquidity = state.curve.liquidity;
    let net = i128::try_from(liquidity).expect("fits");
    let (lower, upper) = if state.token_is_currency0() {
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

/// The pool with a hook that does nothing but allow swaps from the empty range: the AMM alone,
/// which on a dynamic-fee pool with no override charges no LP fee, as the pool's swap leg does on
/// chain.
fn amm_only(pool: &UniswapV4State) -> UniswapV4State {
    let mut amm_only = pool.clone();
    amm_only.set_hook_handler(Box::new(ExtensionTestHook {
        swaps_from_empty_range: true,
        ..Default::default()
    }));
    amm_only
}

/// What a buy of `gross` raw units of quote on `pool` gives: the hook takes `ceil(gross * fee /
/// 1e6)` and the AMM swaps the rest, with the pool's own fee and orientation. Only the fee's
/// rounding is worked here: the AMM leg is the production path, which the literal quotes of
/// `test_a_buy_and_a_sell_without_a_fill_quote_the_pinned_amounts` pin.
fn buy_through_the_amm_on_the_net(pool: &UniswapV4State, gross: u128) -> BigUint {
    let state = hook_of(pool).state;
    let fee = (gross * u128::from(state.fee_pips)).div_ceil(1_000_000);
    buy(&amm_only(pool), state.token_is_currency0(), &(gross - fee).to_string()).unwrap()
}

/// What a sell of `amount` on `pool` gives with no fill: the AMM's output less the hook's
/// `ceil(out * fee / 1e6)`, with the pool's own fee and orientation. Only the fee's rounding is
/// worked here, as in [`buy_through_the_amm_on_the_net`].
fn sell_through_the_amm_less_the_fee(pool: &UniswapV4State, amount: &str) -> BigUint {
    let state = hook_of(pool).state;
    let out = sell(&amm_only(pool), state.token_is_currency0(), amount).unwrap();
    let fee = (&out * state.fee_pips + 999_999u32) / 1_000_000u32;
    out - fee
}

fn attributes(state: &AscendPoolState) -> HashMap<String, Bytes> {
    let u64_attr = |v: u64| Bytes::from(v.to_be_bytes().to_vec());
    [
        ("ascend_fee_pips", u64_attr(state.fee_pips.into())),
        ("ascend_token_is_currency0", Bytes::from(vec![u8::from(state.token_is_currency0())])),
        ("ascend_start_tick", Bytes::from(state.start_tick.to_be_bytes().to_vec())),
        (
            "ascend_ema_sqrt_price_x96",
            Bytes::from(
                state
                    .observation
                    .ema
                    .to_be_bytes::<32>()[12..]
                    .to_vec(),
            ),
        ),
        ("ascend_ema_updated_at", u64_attr(state.observation.updated_at)),
        (
            "ascend_observed_tick",
            Bytes::from(
                state
                    .observation
                    .tick
                    .to_be_bytes()
                    .to_vec(),
            ),
        ),
        ("ascend_discount", u64_attr(state.observation.discount.into())),
        (
            "ascend_bid_inventory",
            Bytes::from(
                state
                    .bid
                    .inventory
                    .to_be_bytes::<32>()
                    .to_vec(),
            ),
        ),
        (
            "ascend_bid_filled_in_window",
            Bytes::from(
                state
                    .bid
                    .filled
                    .to_be_bytes::<32>()
                    .to_vec(),
            ),
        ),
        ("ascend_bid_window_start", u64_attr(state.bid.window_start)),
        (
            "ascend_bid_max_clip",
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

/// The launch formula for a pool starting at -168600 gives the liquidity the chain holds (pool
/// 0x1aaf6a3f4b78f09f65edddd3921057733adcf582401ab3da4ac16f4d865c3644 at block 8397596), for both
/// orientations.
#[rstest]
#[case::token_is_currency0(true)]
#[case::mirrored(false)]
fn test_curve_liquidity_is_what_the_chain_holds(#[case] token_is_currency0: bool) {
    let curve = Curve::try_new(token_is_currency0, -168_600).expect("a curve");

    assert_eq!(curve.liquidity, L_168600);
}

/// The tokens down to the curve end: from spot `sqrtAt(-168500)` they are
/// 4987498616072757541375877 (the math library's trace at block 8660000), and from the sizing
/// fixture's fill price, where a sell clamps to the curve end, the library's `eth_call` at that
/// block takes 1496824502648219975373292.
#[rstest]
#[case::from_the_launch_spot(SQRT_M168500, "4987498616072757541375877")]
#[case::from_the_fixture_fill_price("17322503759888906322764708", "1496824502648219975373292")]
fn test_tokens_to_the_curve_end_match_the_chain(#[case] start_price: &str, #[case] expected: &str) {
    let curve = Curve::try_new(true, -168_600).unwrap();
    assert_eq!(curve.curve_end, u(SQRT_M168600));

    let to_end =
        amount0_delta_down(curve.curve_end, u(start_price), curve.liquidity).expect("a delta");

    assert_eq!(to_end, u(expected));
}

#[test]
fn test_curve_bounds_of_both_orientations() {
    let token0 = Curve::try_new(true, -168_600).unwrap();
    assert_eq!(token0.start_side_bound, u(SQRT_M168800));
    assert_eq!(token0.curve_end, get_sqrt_ratio_at_tick(-168_600).unwrap());
    assert_eq!(token0.far_end, get_sqrt_ratio_at_tick(31_400).unwrap());

    let mirrored = Curve::try_new(false, -168_600).unwrap();
    assert_eq!(mirrored.start_side_bound, get_sqrt_ratio_at_tick(168_800).unwrap());
    assert_eq!(mirrored.curve_end, u(SQRT_168600));
    assert_eq!(mirrored.far_end, get_sqrt_ratio_at_tick(-31_400).unwrap());
}

/// A start tick far below -660000 floors the token0 liquidity to 0, which would divide the
/// fill's sizing by 0; one near the top of the tick range makes the mirrored liquidity exceed
/// `uint128`; a tick past the price range has no curve at all.
#[rstest]
#[case::zero_liquidity(true, -880_000)]
#[case::liquidity_beyond_uint128(false, 687_000)]
#[case::tick_out_of_range(true, 1_000_000)]
fn test_a_curve_the_hook_could_not_launch_is_refused(
    #[case] token_is_currency0: bool,
    #[case] start_tick: i32,
) {
    let result = Curve::try_new(token_is_currency0, start_tick);

    assert!(matches!(result, Err(SimulationError::InvalidInput(..))), "{result:?}");
}

// ---------------------------------------------------------------------------------------------
// The observation
// ---------------------------------------------------------------------------------------------

/// Tx 0xd2cce086b4b757570059c981735f534fcbe85a1dc28d06935400989a2d2f7e5d (block 8351072, token0,
/// EMA rising, dt = 969): the hook's trace shows the EMA 0xd9137f51e679b46a57ec68 ->
/// 0xd9160c7dbd65710dd08a3d in steps of 600 s and 369 s, and the discount 0x2bbf2ed -> ceil
/// 0x2ac97d8 -> 0x2af920e. The spot, 0xd9173164a0a4ad89a19b10, is one the two steps map to that EMA
/// (found by bisection; the steps' floors admit two).
#[test]
fn test_observation_reproduces_the_traced_ema_and_discount() {
    let curve = Curve::try_new(true, -168_600).unwrap();
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
    let curve = Curve::try_new(true, -168_600).unwrap();
    let before = observation(u("100000000000000000000000000"), TS, -100_000, 1_000);

    let after = observe(&curve, &before, u("90000000000000000000000000"), -50, TS).unwrap();

    assert_eq!(after, before);
}

#[test]
fn test_observation_before_the_last_one_is_refused() {
    let curve = Curve::try_new(true, -168_600).unwrap();
    let before = observation(u("100000000000000000000000000"), TS, 0, 0);

    let result = observe(&curve, &before, u("100000000000000000000000000"), 0, TS - 1);

    assert!(matches!(result, Err(SimulationError::InvalidInput(..))), "{result:?}");
}

/// Hand-worked, as the hook's rules give them, at dt = 100 s:
/// - EMA 1e26 falling to a spot of 9e25: one step of 100 s moves `floor(1e25 * 100 / 700)`; d1 =
///   ceil(1000 * 43200 / 43300) = 998; x = floor((2^32 - 998) * 1e26 / ema) is about 1.0145
///   * 2^32, above 2^32, so the stored d is the floor 0, not a negative number;
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
    #[case] discount: u32,
    #[case] spot: &str,
    #[case] expected_ema: &str,
    #[case] expected_discount: u32,
) {
    let curve = Curve::try_new(token_is_currency0, -168_600).unwrap();
    let before = observation(u(ema), TS - 100, 0, discount);

    let after = observe(&curve, &before, u(spot), 5, TS).unwrap();

    assert_eq!(after.ema, u(expected_ema));
    assert_eq!(after.discount, expected_discount);
}

/// The EMA takes at most 8 steps: after 1e6 s toward a spot at twice the EMA it has closed
/// `1 - 2^-8` of the distance (each step of 600 s halves it), not all of it.
#[test]
fn test_observation_takes_at_most_eight_steps() {
    let curve = Curve::try_new(true, -168_600).unwrap();
    let before = observation(u("100000000000000000000000000"), TS - 1_000_000, 0, 1_000_000);

    let after = observe(&curve, &before, u("200000000000000000000000000"), 0, TS).unwrap();

    assert_eq!(after.ema, u("199609375000000000000000000"));
    assert_eq!(after.discount, 2_143_301_883);
}

/// dt = 2000 s with the spot at the EMA: four steps move nothing; only d decays,
/// `ceil(1e6 * 43200 / 45200) = 955753`, and the re-anchor leaves `2^32 - x = d1`.
#[test]
fn test_observation_decays_the_discount_with_the_elapsed_time() {
    let curve = Curve::try_new(true, -168_600).unwrap();
    let before = observation(u("100000000000000000000000000"), TS - 2_000, 0, 1_000_000);

    let after = observe(&curve, &before, u("100000000000000000000000000"), 0, TS).unwrap();

    assert_eq!(after.ema, before.ema);
    assert_eq!(after.discount, 955_753);
}

// ---------------------------------------------------------------------------------------------
// Fill sizing: the outputs of the deployed library's `0x15821c64`
// ---------------------------------------------------------------------------------------------

const HUGE: &str = "10000000000000000000000000000000000000000";
const INT128_MAX_DECIMAL: &str = "170141183460469231731687303715884105727";

/// The sizing inputs of a pool at tick -168500 with the EMA a bid's fill price sits 0.05% below
/// (`d = 2147483`), for either orientation.
struct SizingFixture {
    curve: Curve,
    spot: U256,
    fill_price: U256,
    ema: U256,
}

fn token0_fixture() -> SizingFixture {
    SizingFixture {
        curve: Curve::try_new(true, -168_600).unwrap(),
        spot: u("17331169341945051549612302"),
        fill_price: u("17322503759888906322764708"),
        ema: u("17331169341945051549612302"),
    }
}

fn mirrored_fixture() -> SizingFixture {
    SizingFixture {
        curve: Curve::try_new(false, -168_600).unwrap(),
        spot: u(SQRT_168500),
        // ceil(ema * 2^32 / (2^32 - 2147483))
        fill_price: u("360558220948084641088334577914306"),
        ema: u("360377941892009552490198183628619"),
    }
}

fn size(
    fixture: &SizingFixture,
    amount_in: &str,
    inventory: &str,
    max_clip: &str,
    filled: &str,
) -> Option<(U256, U256, u32)> {
    size_fill(
        &fixture.curve,
        &FillRequest {
            spot: fixture.spot,
            fill_price: fixture.fill_price,
            ema: fixture.ema,
            amount_in: u(amount_in),
            inventory: u(inventory),
            max_clip: u(max_clip),
            filled: u(filled),
        },
    )
    .expect("sizes")
    .map(|fill| (fill.tokens_in, fill.quote_out, fill.discount))
}

/// `(tokensIn, quoteOut, newD)` the library returned to `eth_call` at block 8660000 for the same
/// arguments, on the token0 fixture. The tokens to the fill price are 499274149441997029379948,
/// so the first input is that plus 1e21.
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
    INT128_MAX_DECIMAL,
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
#[case::capped_by_an_inventory_of_3(
    "500274149441997029379948",
    "3",
    HUGE,
    "0",
    "41837692",
    "2",
    2_147_484
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
fn test_token0_fill_sizing_matches_the_library(
    #[case] amount: &str,
    #[case] inventory: &str,
    #[case] max_clip: &str,
    #[case] filled: &str,
    #[case] tokens_in: &str,
    #[case] quote_out: &str,
    #[case] discount: u32,
) {
    let fill = size(&token0_fixture(), amount, inventory, max_clip, filled);

    assert_eq!(fill, Some((u(tokens_in), u(quote_out), discount)));
}

/// The same on the mirrored fixture. `V` is 4987498616072757541375872, the tokens to the curve
/// end: the library's own output for `A = V` is the quote the venue paid at that input on pool
/// 0x37a99c5c3fb3f8431e718494fed8746c856de1c6a61f8cd96aa145cf3f1bc9a5 (238899517702555132), and
/// its output is the same past `V`.
#[rstest]
#[case::at_the_curve_end(
    "4987498616072757541375872",
    HUGE,
    HUGE,
    "0",
    "4987498616072757541375872",
    "238899517702555132",
    29_967_265
)]
#[case::just_past_the_curve_end(
    "4987498616072757541375877",
    HUGE,
    HUGE,
    "0",
    "4987498616072757541375872",
    "238899517702555132",
    29_967_265
)]
#[case::far_past_the_curve_end(
    "7481247924109136312063808",
    HUGE,
    HUGE,
    "0",
    "4987498616072757541375872",
    "238899517702555132",
    29_967_265
)]
#[case::small_input(
    "1000000000000000000",
    HUGE,
    HUGE,
    "0",
    "1000000000000000000",
    "48139751956",
    8_589_939
)]
#[case::capped_by_the_inventory(
    "4987498616072757541375872",
    "1000000000",
    HUGE,
    "0",
    "20772853135848501",
    "999999999",
    8_589_935
)]
#[case::capped_by_a_smaller_inventory(
    "4987498616072757541375872",
    "100000000",
    HUGE,
    "0",
    "2077285282784893",
    "99999999",
    8_589_935
)]
#[case::capped_by_the_window(
    "4987498616072757541375872",
    HUGE,
    "50000000000000",
    "20000000000000",
    "623185985377143558092",
    "29999999999999",
    8_592_620
)]
fn test_mirrored_fill_sizing_matches_the_library(
    #[case] amount: &str,
    #[case] inventory: &str,
    #[case] max_clip: &str,
    #[case] filled: &str,
    #[case] tokens_in: &str,
    #[case] quote_out: &str,
    #[case] discount: u32,
) {
    let fill = size(&mirrored_fixture(), amount, inventory, max_clip, filled);

    assert_eq!(fill, Some((u(tokens_in), u(quote_out), discount)));
}

/// Inputs for which the library returns `(0, 0, 0)` at block 8660000, as `eth_call` shows.
#[rstest]
#[case::input_at_the_price_move("499274149441997029379948", HUGE, HUGE, "0")]
#[case::input_below_the_price_move("1", HUGE, HUGE, "0")]
#[case::a_filled_window(
    "1000000000000000000000000000000",
    HUGE,
    "50000000000000",
    "50000000000000"
)]
#[case::a_bid_of_1("1000000000000000000000000000000", "1", HUGE, "0")]
#[case::a_bid_of_2("1000000000000000000000000000000", "2", HUGE, "0")]
fn test_token0_sizing_the_library_declines_is_no_fill(
    #[case] amount: &str,
    #[case] inventory: &str,
    #[case] max_clip: &str,
    #[case] filled: &str,
) {
    assert_eq!(size(&token0_fixture(), amount, inventory, max_clip, filled), None);
}

#[test]
fn test_token0_fill_price_at_the_curve_end_is_no_fill() {
    let mut fixture = token0_fixture();
    fixture.fill_price = fixture.curve.curve_end;

    assert_eq!(size(&fixture, "1000000000000000000000000000000", HUGE, HUGE, "0"), None);
}

/// An EMA twice the spot puts the spot outside the band of 2.5% each way.
#[test]
fn test_token0_spot_outside_the_band_is_no_fill() {
    let mut fixture = token0_fixture();
    fixture.ema *= U256::from(2);

    assert_eq!(size(&fixture, "1000000000000000000000000000000", HUGE, HUGE, "0"), None);
}

/// A spot below the curve end with the fill price above it: the library returns no fill.
#[test]
fn test_token0_spot_beyond_the_curve_end_is_no_fill() {
    let mut fixture = token0_fixture();
    fixture.spot = u("17279279613376353790481728");
    fixture.ema = fixture.spot;
    fixture.fill_price = u("17313872765755485629902110");

    assert_eq!(size(&fixture, "1000000000000000000000000", HUGE, HUGE, "0"), None);
}

/// A spot exactly at the curve end, with the fill price on the near side of it: nothing is left
/// to fill and the library returns `(0, 0, 0)` (`eth_call` at block 8660000, on both orientations).
#[rstest]
#[case::token0(token0_fixture(), "17313872765755485629902110")]
#[case::mirrored(mirrored_fixture(), "360558220948084641088334577914306")]
fn test_spot_at_the_curve_end_is_no_fill(
    #[case] mut fixture: SizingFixture,
    #[case] fill_price: &str,
) {
    fixture.spot = fixture.curve.curve_end;
    fixture.ema = fixture.spot;
    fixture.fill_price = u(fill_price);

    assert_eq!(size(&fixture, "1000000000000000000000000", HUGE, HUGE, "0"), None);
}

#[test]
fn test_mirrored_fill_price_at_the_curve_end_is_no_fill() {
    let mut fixture = mirrored_fixture();
    fixture.fill_price = fixture.curve.curve_end;

    assert_eq!(size(&fixture, "1000000000000000000000000000000", HUGE, HUGE, "0"), None);
}

/// The mirrored spot above the curve end, the EMA there too: the library returns no fill.
#[test]
fn test_mirrored_spot_beyond_the_curve_end_is_no_fill() {
    let mut fixture = mirrored_fixture();
    fixture.spot = u("363272983523322200713924948012421");
    fixture.ema = fixture.spot;

    assert_eq!(size(&fixture, "1000000000000000000000000000000", HUGE, HUGE, "0"), None);
}

// ---------------------------------------------------------------------------------------------
// Fees
// ---------------------------------------------------------------------------------------------

/// Tx 0xe607911ed6009338af2013bf355bad1284185a9bc37626801de1d1914a49bb1f: total in
/// 0xeff84958f86b75, the specified delta 0x026652a74a2a09 is `ceil(1%)`.
#[test]
fn test_a_buys_fee_is_the_ceiling_of_the_input_share() {
    let amount = U256::from_str_radix("eff84958f86b75", 16).unwrap();

    assert_eq!(
        fee_ceil(amount, 10_000).unwrap(),
        U256::from_str_radix("026652a74a2a09", 16).unwrap()
    );
}

/// Tx 0xd2cce086b4b757570059c981735f534fcbe85a1dc28d06935400989a2d2f7e5d: the fill fee
/// 0x2171adf6b561a is `floor(1%)` of the fill's quote 0xd1067f45eda23a, the afterSwap fee
/// 0x7b5521ee20e9f95 is `ceil(1%)` of the AMM output 0x302d414104db655dd, and the user got `(quote
/// - fill fee) + out - afterSwap fee` = 0x2fbedb155d22a0268.
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

/// V4Quoter at block 8660000 (time 1795168252) on pool
/// 0xc0be17156606216257c638d839c1dd6d43955a565e0b3d6c568d79ebe82cf84c (token0, fee 1%) with the
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

/// The same on pool 0x37a99c5c3fb3f8431e718494fed8746c856de1c6a61f8cd96aa145cf3f1bc9a5 (mirrored,
/// fee 2%), the price at tick 168500: 47176909552712.
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

const PLENTY: &str = "1000000000000000000000000000000";

/// A pool of either orientation, 100 ticks from its curve end (tick -168500 for token0, 168500
/// mirrored), with the given fee and bid inventory, observed at `TS` at its own tick with the EMA
/// 0.2% above its price and `d = 2147483`: the fill price is above the spot, so `T = 0` and a
/// small sell fills entirely at the spot (the library at block 8660000: 1e18 tokens -> quote
/// 48139751956, newD 8572794, in either orientation).
fn funded_pool(token_is_currency0: bool, fee_pips: u32, inventory: &str) -> UniswapV4State {
    if token_is_currency0 {
        let state = pool_state(
            true,
            fee_pips,
            observation(u("17418037835569639047771018"), TS, -168_500, 2_147_483),
            bid(u(inventory)),
        );
        pool(state, u(SQRT_M168500), -168_500, TS)
    } else {
        let state = pool_state(
            false,
            fee_pips,
            observation(u("360377941892009552490198183628619"), TS, 168_500, 2_147_483),
            bid(u(inventory)),
        );
        pool(state, u(SQRT_168500), 168_500, TS)
    }
}

/// A fill that runs to the curve end and leaves the AMM the rest, as the library sizes it (at
/// `A = V` its quote is the venue's 238899517702555132, read at block 8660000), with the
/// swapper's share `quote - floor(quote * 1%)`. The fill that takes the whole input is a case of
/// `test_the_fee_moves_the_quote_against_the_trader`.
#[test]
fn test_a_sell_fills_to_the_curve_end_and_the_amm_takes_the_rest_on_a_token0_pool() {
    let pool = funded_pool(true, 10_000, PLENTY);

    assert_eq!(sell(&pool, true, "4988498616072757541375877").unwrap(), big("236558180832118545"));
}

/// A sell that fills nothing is the AMM's: the same pool without inventory quotes the
/// AMM-only 47658306588964 for 1e21 that V4Quoter gives at block 8660000, below what the fill gives
/// at the spot.
#[test]
fn test_a_sell_without_inventory_is_the_amms() {
    let pool = funded_pool(true, 10_000, "0");

    assert_eq!(sell(&pool, true, "1000000000000000000000").unwrap(), big("47658306588964"));
}

/// Mirrored fill at `A = V + 5` (the quote at `A = V` is 238899517702555132 on both pools),
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

/// The context of a swap on the pool at price `spot` and tick `tick`.
fn state_context(spot: U256, tick: i32) -> StateContext {
    StateContext {
        currency_0: Address::repeat_byte(1),
        currency_1: Address::repeat_byte(2),
        fees: UniswapV4Fees::new(0, 0, DYNAMIC_FEE_FLAG),
        tick_spacing: 200,
        sqrt_price_x96: spot,
        tick,
    }
}

/// The parameters of an exact-input (negative) or exact-output swap of `amount_specified`.
fn swap_params(zero_for_one: bool, amount_specified: I256) -> SwapParams {
    SwapParams { zero_for_one, amount_specified, sqrt_price_limit: U256::ZERO }
}

fn before_swap_delta(
    pool: &UniswapV4State,
    zero_for_one: bool,
    amount: i128,
    spot: U256,
    tick: i32,
) -> (I128, I128, u64) {
    let params = BeforeSwapParameters {
        context: state_context(spot, tick),
        sender: Address::ZERO,
        swap_params: swap_params(zero_for_one, I256::try_from(amount).unwrap()),
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
/// - 1` binds and the library gives tokensIn 20772853149254418 for quoteOut 999999999 (block
///   8660000); the swapper's share is 990000000 after the 1% fill fee. The gas is the one measured
///   for a fill.
#[test]
fn test_a_capped_fill_delta_is_the_tokens_for_the_quote_net_of_its_fee() {
    let pool = funded_pool(true, 10_000, "1000000000");

    let (specified, unspecified, gas) =
        before_swap_delta(&pool, true, -1_000_000_000_000_000_000, u(SQRT_M168500), -168_500);

    assert_eq!(specified, I128::try_from(20_772_853_149_254_418i128).unwrap());
    assert_eq!(unspecified, I128::try_from(-990_000_000i128).unwrap());
    assert_eq!(gas, 171_820);
}

/// A buy's beforeSwap takes `ceil(A * f / 1e6)` of the input as a positive specified delta.
#[test]
fn test_a_buy_takes_its_fee_in_before_swap() {
    let pool = funded_pool(true, 10_000, "0");

    let (specified, unspecified, _) =
        before_swap_delta(&pool, false, -67_545_513_342_823_285, u(SQRT_M168500), -168_500);

    assert_eq!(specified, I128::try_from(675_455_133_428_233i128).unwrap());
    assert_eq!(unspecified, I128::ZERO);
}

/// A buy pays the fee out of its input and the AMM swaps the rest; the output is the AMM's.
#[test]
fn test_a_buy_quotes_the_amm_on_the_net_input() {
    let pool = funded_pool(true, 10_000, "0");

    let with_fee = buy(&pool, true, "1010101010101010102").unwrap();
    let without_fee = buy(&amm_only(&pool), true, "1000000000000000000").unwrap();

    // 1010101010101010102 - ceil(1010101010101010102 * 0.01) = 1e18 exactly, by hand:
    // the fee is 10101010101010102 (the share 10101010101010101.02 rounded up).
    assert_eq!(with_fee, without_fee);
}

// ---------------------------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------------------------

fn assert_refused(result: &Result<BigUint, SimulationError>, label: &str) {
    assert!(matches!(result, Err(SimulationError::InvalidInput(..))), "{label}: {result:?}");
}

/// Pool 0x1aaf6a3f4b78f09f65edddd3921057733adcf582401ab3da4ac16f4d865c3644 at block 8397596:
/// sqrtP = sqrtAt(-168600) + 2, tick -168600, active liquidity L, so the in-range step yields 0
/// and the swap crosses into the empty range. V4Quoter reverts NotEnoughLiquidity on a sell of
/// 1e6 or 1e18; a buy of 1e15 quotes nonzero.
fn pool_at_its_lower_edge() -> UniswapV4State {
    let spot = u("17296576189565919710191921");
    let state = pool_state(true, 10_000, observation(spot, TS, -168_600, 0), no_bid());
    // The tick -168600 is the position's lower tick, so `pool` gives it the full liquidity, as
    // the chain stores it.
    pool(state, spot, -168_600, TS)
}

#[test]
fn test_a_pool_sold_back_to_its_lower_edge_refuses_sells() {
    let edge = pool_at_its_lower_edge();

    assert_refused(&sell(&edge, true, "1000000"), "sell 1e6");
    assert_refused(&sell(&edge, true, "1000000000000000000"), "sell 1e18");
}

#[test]
fn test_a_pool_sold_back_to_its_lower_edge_quotes_buys() {
    let edge = pool_at_its_lower_edge();

    let quote = buy(&edge, true, "1000000000000000").unwrap();

    // The AMM alone on the net input after the 1% fee (1e13), nonzero as on chain.
    assert_eq!(quote, buy_through_the_amm_on_the_net(&edge, 1_000_000_000_000_000));
    assert!(quote > BigUint::from(0u32));
}

/// A pool of either orientation at its start price, one tick spacing outside its position, with no
/// bid.
fn pool_at_its_start_price(token_is_currency0: bool) -> UniswapV4State {
    let (spot, tick) = if token_is_currency0 {
        (SQRT_M168800, -168_800)
    } else {
        ("366557197058144561653914105585283", 168_800)
    };
    let state = pool_state(token_is_currency0, 10_000, observation(u(spot), TS, tick, 0), no_bid());
    pool(state, u(spot), tick, TS)
}

/// A pool at its start price has no liquidity on the sell side: V4Quoter reverts with
/// `0x057fcb64` (zero liquidity) on a sell there.
#[rstest]
#[case::token0(true)]
#[case::mirrored(false)]
fn test_a_pool_at_its_start_price_refuses_sells(#[case] token_is_currency0: bool) {
    let pool = pool_at_its_start_price(token_is_currency0);

    assert_refused(&sell(&pool, token_is_currency0, "1000000000000000000"), "sell");
}

#[rstest]
#[case::token0(true)]
#[case::mirrored(false)]
fn test_a_pool_at_its_start_price_has_a_zero_sell_limit(#[case] token_is_currency0: bool) {
    let pool = pool_at_its_start_price(token_is_currency0);
    let (launched, quote) = tokens(token_is_currency0);

    let (max_in, max_out) = pool
        .get_limits(launched.address, quote.address)
        .unwrap();

    assert_eq!((max_in, max_out), (BigUint::from(0u32), BigUint::from(0u32)));
}

/// A buy on a pool at its start price swaps from the empty range into the position: its quote is
/// the AMM's on the net of the 1% fee, which is the 1e16 of a 1e18 buy.
#[rstest]
#[case::token0(true)]
#[case::mirrored(false)]
fn test_a_pool_at_its_start_price_quotes_buys(#[case] token_is_currency0: bool) {
    let pool = pool_at_its_start_price(token_is_currency0);

    let quote = buy(&pool, token_is_currency0, "1000000000000000000").unwrap();

    assert_eq!(quote, buy_through_the_amm_on_the_net(&pool, 1_000_000_000_000_000_000));
    assert!(quote > BigUint::from(0u32));
}

#[rstest]
#[case::zero(I256::ZERO)]
#[case::exact_output(I256::ONE)]
#[case::above_int128(-I256::from_raw(U256::from(u128::MAX >> 1) + U256::from(1)))]
fn test_before_swap_refuses_what_the_pool_reverts(#[case] amount: I256) {
    let pool = funded_pool(true, 10_000, "0");
    let params = BeforeSwapParameters {
        context: state_context(u(SQRT_M168500), -168_500),
        sender: Address::ZERO,
        swap_params: swap_params(true, amount),
        hook_data: Bytes::new(),
    };

    let result = hook_of(&pool).before_swap(params, None, None);

    assert!(matches!(result, Err(SimulationError::InvalidInput(..))), "{result:?}");
}

fn outcome(unfilled: u64) -> SwapOutcome {
    SwapOutcome { sqrt_price_x96: U256::ZERO, amount_unfilled: U256::from(unfilled) }
}

#[test]
fn test_an_unfilled_swap_is_refused() {
    let pool = funded_pool(true, 10_000, "0");

    assert!(matches!(
        hook_of(&pool).check_swap_outcome(true, &outcome(1)),
        Err(SimulationError::InvalidInput(..))
    ));
}

#[test]
fn test_a_swap_filled_in_full_is_accepted() {
    let pool = funded_pool(true, 10_000, "0");

    assert!(hook_of(&pool)
        .check_swap_outcome(true, &outcome(0))
        .is_ok());
}

// ---------------------------------------------------------------------------------------------
// Limits: the limit settles and one unit more is refused
// ---------------------------------------------------------------------------------------------

/// Checks one direction of `pool`: its limit settles for exactly the limit's output and one unit
/// more is refused. Returns whether the limit is above 0 (a limit of 0 has nothing to settle).
fn check_limit(
    pool: &UniswapV4State,
    token_is_currency0: bool,
    quote_in: bool,
    label: &str,
) -> bool {
    let (launched, quote) = tokens(token_is_currency0);
    let (token_in, token_out) = if quote_in { (quote, launched) } else { (launched, quote) };
    let (max_in, max_out) = pool
        .get_limits(token_in.address.clone(), token_out.address.clone())
        .unwrap_or_else(|e| panic!("{label}: limits: {e}"));
    if max_in == BigUint::from(0u32) {
        return false;
    }
    let at = pool
        .get_amount_out(max_in.clone(), &token_in, &token_out)
        .unwrap_or_else(|e| panic!("{label}: the limit {max_in} does not settle: {e}"));
    assert_eq!(at.amount, max_out, "{label}: output at the limit {max_in}");
    let past = pool.get_amount_out(&max_in + 1u32, &token_in, &token_out);
    assert!(
        matches!(past, Err(SimulationError::InvalidInput(..))),
        "{label}: one past the limit {max_in}: {:?}",
        past.map(|r| r.amount)
    );
    true
}

fn assert_limit_is_exact(pool: &UniswapV4State, token_is_currency0: bool, quote_in: bool) {
    assert!(check_limit(pool, token_is_currency0, quote_in, "the pool"), "a zero limit");
}

/// A deterministic generator (splitmix64), so that a failing case replays from its number.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn below_i32(&mut self, n: u64) -> i32 {
        i32::try_from(self.below(n)).unwrap()
    }

    fn pick<T: Copy>(&mut self, options: &[T]) -> T {
        let index = self.below(u64::try_from(options.len()).unwrap());
        options[usize::try_from(index).unwrap()]
    }
}

/// A random pool and its orientation, quoting at `TS`: any start tick the launches use, the price
/// from the empty range outside the position to deep inside it, an EMA within 3% of the price, an
/// observation from the same second to long ago at the pool's tick or one below, any discount, a
/// bid with no inventory, a bid of 1, or a bid of any size, with a window that has or has not
/// run out and a clip that does or does not bind.
fn random_pool(rng: &mut Rng) -> (UniswapV4State, bool) {
    let token_is_currency0 = rng.below(2) == 0;
    // Launch ticks are multiples of the tick spacing, 200.
    let start_tick = -200 * (500 + rng.below_i32(1_250));
    let offset = match rng.below(3) {
        0 => rng.below_i32(500) - 200,
        1 => rng.below_i32(30_000),
        _ => rng.below_i32(199_000),
    };
    let tick = if token_is_currency0 { start_tick + offset } else { -start_tick - offset };
    let spot = get_sqrt_ratio_at_tick(tick).unwrap();
    let ema = spot * U256::from(9_700 + rng.below(600)) / U256::from(10_000);
    let discount = u32::try_from(match rng.below(3) {
        0 => rng.pick(&[0, 1, 2_147_483, u64::from(u32::MAX)]),
        _ => rng.below(1 << 27),
    })
    .unwrap();
    let inventory = match rng.below(5) {
        0 => U256::ZERO,
        1 => U256::from(1),
        2 => U256::from(rng.below(1_000_000_000_000)),
        3 => U256::from(rng.below(1_000_000_000_000_000)) * U256::from(1_000_000_000u64),
        _ => U256::from(10u64).pow(U256::from(30)),
    };
    let max_clip = 10u128.pow(8 + u32::try_from(rng.below(24)).unwrap());
    let filled =
        if rng.below(3) == 0 { U256::from(max_clip) } else { U256::from(rng.below(1_000_000_000)) };
    let state = AscendPoolState::try_new(
        rng.pick(&[0, 1, 10_000, 20_000, 30_000, 99_999, 500_000]),
        token_is_currency0,
        start_tick,
        observation(
            ema,
            TS - rng.pick(&[0, 1, 7, 600, 5_000, 100_000]),
            if rng.below(4) == 0 { tick - 1 } else { tick },
            discount,
        ),
        Bid {
            inventory,
            filled,
            window_start: TS - rng.pick(&[0, 100, 599, 600, 1_000]),
            max_clip,
        },
    )
    .expect("a state the hook could have stored");
    (pool(state, spot, tick, TS), token_is_currency0)
}

/// Over random states (every orientation, price, observation, timestamp, window and inventory
/// above) the limit of each direction settles and one unit more is refused, so `get_limits` is
/// exactly the boundary of `get_amount_out`.
#[test]
fn test_limits_of_random_states_settle_and_one_more_unit_is_refused() {
    let mut rng = Rng(0x00a5_ce2d);
    let mut checked = 0;
    for case in 0..500 {
        let (pool, token_is_currency0) = random_pool(&mut rng);
        for quote_in in [false, true] {
            let label = format!("case {case}, quote in: {quote_in}");
            checked += usize::from(check_limit(&pool, token_is_currency0, quote_in, &label));
        }
    }

    // Most states have a limit to test: a pool inside the empty range sells nothing.
    assert!(checked > 600, "only {checked} limits were above 0");
}

/// A pool 100 ticks from its curve end, in either orientation.
#[rstest]
#[case::token0_buy(true, "0", false)]
#[case::token0_sell_without_inventory(true, "0", true)]
#[case::token0_sell_with_a_funded_bid(true, PLENTY, true)]
#[case::token0_sell_capped_by_the_inventory(true, "1000000000", true)]
#[case::mirrored_buy(false, "0", false)]
#[case::mirrored_sell_without_inventory(false, "0", true)]
#[case::mirrored_sell_with_a_funded_bid(false, PLENTY, true)]
fn test_limits_settle_and_one_more_unit_is_refused(
    #[case] token_is_currency0: bool,
    #[case] inventory: &str,
    #[case] is_sell: bool,
) {
    let pool = funded_pool(token_is_currency0, 20_000, inventory);

    assert_limit_is_exact(&pool, token_is_currency0, !is_sell);
}

/// A pool at its start price: a buy fits the whole position, from the empty range.
#[rstest]
#[case::token0(true)]
#[case::mirrored(false)]
fn test_the_buy_limit_of_a_pool_at_its_start_price_settles(#[case] token_is_currency0: bool) {
    assert_limit_is_exact(&pool_at_its_start_price(token_is_currency0), token_is_currency0, true);
}

// ---------------------------------------------------------------------------------------------
// State carried by a quote, and time
// ---------------------------------------------------------------------------------------------

/// A fill consumes the inventory, fills the window, opens it at the block's time and moves the
/// discount to the library's `newD`; the pool's price is the AMM's alone.
#[test]
fn test_a_fill_updates_the_hook_in_the_returned_state() {
    let pool = funded_pool(true, 10_000, PLENTY);
    let (launched, quote) = tokens(true);

    let result = pool
        .get_amount_out(big("1000000000000000000"), &launched, &quote)
        .unwrap();
    let next = result
        .new_state
        .as_any()
        .downcast_ref::<UniswapV4State>()
        .unwrap();
    let hook = hook_of(next).state;

    assert_eq!(hook.bid.inventory, u(PLENTY) - U256::from(48_139_751_956u64));
    assert_eq!(hook.bid.filled, U256::from(48_139_751_956u64));
    assert_eq!(hook.bid.window_start, TS);
    assert_eq!(hook.observation.discount, 8_572_794);
    // The old state is untouched.
    assert_eq!(hook_of(&pool).state.bid.inventory, u(PLENTY));
}

/// A pool at its token0 price with the observation `dt = 100` s old, funded or not.
fn pool_observed_100_seconds_ago(inventory: &str) -> UniswapV4State {
    let state = pool_state(
        true,
        10_000,
        observation(u("17418037835569639047771018"), TS - 100, -168_500, 2_147_483),
        bid(u(inventory)),
    );
    pool(state, u(SQRT_M168500), -168_500, TS)
}

/// The pool after a buy of 3e16 quote, which moves the price a few ticks up (the pool's liquidity
/// is 2e23).
fn after_a_buy(pool: &UniswapV4State) -> UniswapV4State {
    let (launched, quote) = tokens(true);
    let after = pool
        .get_amount_out(big("30000000000000000"), &quote, &launched)
        .unwrap()
        .new_state
        .as_any()
        .downcast_ref::<UniswapV4State>()
        .unwrap()
        .clone();
    assert_ne!(after.price_and_tick().1, -168_500, "the buy must move the tick");
    after
}

/// A swap observes the price even without a fill, and the quote's state carries that
/// observation: after a buy at second `TS` the stored observed tick is the one the buy started
/// from, not the one it ended at, and `updated_at` is `TS`, so a sell in the same block reads
/// `dt = 0` and does not fill: it is the AMM's, as on the pool with no inventory.
#[test]
fn test_a_sell_after_a_buy_in_the_same_block_does_not_fill() {
    let funded = after_a_buy(&pool_observed_100_seconds_ago(PLENTY));
    let empty = after_a_buy(&pool_observed_100_seconds_ago("0"));

    let with_bid = sell(&funded, true, "1000000000000000000").unwrap();
    let without_bid = sell(&empty, true, "1000000000000000000").unwrap();

    assert_eq!(with_bid, without_bid);
}

/// What a missing carry would do: the same pool, with the hook as it was before the buy, observes
/// again (`dt = 100`) and fills, which pays more than the AMM. The sell with no buy before it
/// fills at the spot: the library's quote for 1e18 there is 48139751956 (block 8660000), which
/// pays 47658354437 after the 1% fill fee, against the AMM's 47658354435.
#[test]
fn test_a_sell_fills_when_the_buy_before_it_is_not_carried() {
    let funded = pool_observed_100_seconds_ago(PLENTY);
    let mut stale = after_a_buy(&funded);
    stale.set_hook_handler(Box::new(hook_of(&funded).clone()));

    let uncarried = sell(&stale, true, "1000000000000000000").unwrap();
    let carried = sell(&after_a_buy(&funded), true, "1000000000000000000").unwrap();

    // Both quotes are at the price the buy moved to. The carried one is the AMM's (no fill, as in
    // the test above); the uncarried one fills 2 units higher. Neither has a chain read (none
    // holds a stale hook state), so both are the model's own, pinned as regression values.
    assert_eq!(uncarried, big("47717470825"));
    assert_eq!(carried, big("47717470823"));
}

/// With the stored observed tick one off the pool's, a quote at the same second rejects the fill
/// and a quote one second later re-observes and fills: the same state quotes differently when
/// the block moves, which is why `apply_block` reports a change.
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

    // The AMM alone gives 47658354435 for 1e18; at +1 s the observation (EMA step 1 s, d decay,
    // re-anchor floor 2133177) lets the fill take the whole input for the library's quote
    // 48139751956 at block 8660000 (newD 8558558), which pays 47658354437 after the 1% fee.
    assert_eq!(now, big("47658354435"));
    assert_eq!(later, big("47658354437"));
}

#[test]
fn test_apply_block_reports_a_change_only_for_a_pool_with_inventory() {
    let mut funded = funded_pool(true, 10_000, PLENTY);
    let mut empty = funded_pool(true, 10_000, "0");

    assert!(!ProtocolSim::apply_block(&mut funded, &BlockContext::new(1, TS)));
    assert!(ProtocolSim::apply_block(&mut funded, &BlockContext::new(2, TS + 1)));
    assert_eq!(hook_of(&funded).block_timestamp, TS + 1);
    assert!(!ProtocolSim::apply_block(&mut empty, &BlockContext::new(2, TS + 1)));
    assert_eq!(hook_of(&empty).block_timestamp, TS + 1);
}

/// A delta can carry `ema_updated_at` ahead of the block the handler is at. Every swap refuses
/// then (the hook's subtraction would revert), whatever the inventory, and `apply_block` reports
/// the block that ends that refusal, or starts it again, so that the pool's quotes are
/// re-emitted; any other move of a pool with no inventory changes no quote.
#[test]
fn test_apply_block_reports_the_start_and_the_end_of_a_refusal_of_every_swap() {
    let state = pool_state(
        true,
        10_000,
        observation(u("17418037835569639047771018"), TS + 10, -168_500, 2_147_483),
        no_bid(),
    );
    let mut pool = pool(state, u(SQRT_M168500), -168_500, TS);
    let one = "1000000000000000000";
    assert_refused(&sell(&pool, true, one), "a sell before the observation");
    assert_refused(&buy(&pool, true, one), "a buy before the observation");
    // The limits refuse as the quotes do, in both directions.
    let (launched, quote) = tokens(true);
    assert!(
        pool.get_limits(quote.address.clone(), launched.address.clone())
            .is_err(),
        "a buy limit before the observation"
    );
    assert!(
        pool.get_limits(launched.address, quote.address)
            .is_err(),
        "a sell limit before the observation"
    );

    assert!(!ProtocolSim::apply_block(&mut pool, &BlockContext::new(1, TS + 5)));
    assert_refused(&sell(&pool, true, one), "a sell at TS + 5");
    assert!(ProtocolSim::apply_block(&mut pool, &BlockContext::new(2, TS + 10)));
    assert_eq!(sell(&pool, true, one).unwrap(), big("47658354435"));
    assert!(!ProtocolSim::apply_block(&mut pool, &BlockContext::new(3, TS + 11)));
    assert!(ProtocolSim::apply_block(&mut pool, &BlockContext::new(4, TS + 9)));
    assert_refused(&sell(&pool, true, one), "a sell at TS + 9");
}

/// The pool's fee lowers what the swapper receives for the same input of 1e18, in each
/// orientation and direction, by the amount the hook's rules give: a buy pays `ceil(A * f / 1e6)`
/// and the AMM swaps the rest, a sell without a fill gets the AMM's output less `ceil(out * f /
/// 1e6)`, and a sell filled in full gets the library's quote 48139751956 (block 8660000, either
/// orientation) less `floor(q * f / 1e6)`, which are the literals of the cases. The chain's pools
/// charge 1% and 2% (and up to 11%); the cases of 0 and 1 pip are edges the format holds, which
/// no observed pool charges: the hook's own bounds on a fee are unknown (its source is
/// unverified), so the model prices any fee below 100%. The limits of every direction settle at
/// each fee.
#[rstest]
#[case::no_fee(0, "48139751956")]
#[case::one_pip(1, "48139703817")]
#[case::one_percent(10_000, "47658354437")]
#[case::two_percent(20_000, "47176956917")]
fn test_the_fee_moves_the_quote_against_the_trader(
    #[values(true, false)] token_is_currency0: bool,
    #[case] fee_pips: u32,
    #[case] filled_sell: &str,
) {
    let one = "1000000000000000000";
    let no_inventory = funded_pool(token_is_currency0, fee_pips, "0");
    let funded = funded_pool(token_is_currency0, fee_pips, PLENTY);

    assert_eq!(
        buy(&no_inventory, token_is_currency0, one).unwrap(),
        buy_through_the_amm_on_the_net(&no_inventory, 10u128.pow(18))
    );
    assert_eq!(
        sell(&no_inventory, token_is_currency0, one).unwrap(),
        sell_through_the_amm_less_the_fee(&no_inventory, one)
    );
    assert_eq!(sell(&funded, token_is_currency0, one).unwrap(), big(filled_sell));
    for pool in [&no_inventory, &funded] {
        for quote_in in [false, true] {
            assert!(check_limit(pool, token_is_currency0, quote_in, "the fee table"));
        }
    }
}

/// A buy of 1e18 and a sell of 1e18 without a fill at the 1% fee, pinned as literals so that the
/// fee table's oracle, which runs the production AMM path, cannot drift with it. The sell is the
/// pool's AMM output less `ceil(out * 1%)`, a buy swaps the net 0.99e18.
#[rstest]
#[case::token0(true, "20148706015779155991173129", "47658354435")]
#[case::mirrored(false, "20148706015779155991173176", "47658354435")]
fn test_a_buy_and_a_sell_without_a_fill_quote_the_pinned_amounts(
    #[case] token_is_currency0: bool,
    #[case] buy_quote: &str,
    #[case] sell_quote: &str,
) {
    let pool = funded_pool(token_is_currency0, 10_000, "0");
    let one = "1000000000000000000";

    assert_eq!(buy(&pool, token_is_currency0, one).unwrap(), big(buy_quote));
    assert_eq!(sell(&pool, token_is_currency0, one).unwrap(), big(sell_quote));
}

/// The local round-down `getAmount0Delta` is `floor(floor(L << 96 * (b - a) / b) / a)`, as v4
/// computes it, in either order of the prices: it matches a big-integer reference on random
/// prices, and the shared function, which divides the rounded-up product, is never below it and
/// is one above it where that product's ceiling reaches a multiple of `a` (a = 3, b = 7, L = 4:
/// 60364314296582352452223962160 against the shared 60364314296582352452223962161). When the
/// shared function is fixed this test fails: delete `amount0_delta_down` then.
#[test]
fn test_amount0_delta_down_floors_the_product_as_v4_does() {
    use crate::evm::protocol::utils::uniswap::sqrt_price_math::get_amount0_delta;
    let reference = |a: U256, b: U256, liquidity: u128| {
        let (lower, upper) = (a.min(b), a.max(b));
        let product = (BigUint::from(liquidity) << 96usize) * u256_to_biguint(upper - lower) /
            u256_to_biguint(upper);
        product / u256_to_biguint(lower)
    };
    let mut rng = Rng(0xde17a);
    for _ in 0..200 {
        let a = u("17000000000000000000000000") + U256::from(rng.next()) * U256::from(rng.next());
        let b = a + U256::from(rng.next()) * U256::from(1u64 << 40);
        let liquidity = 200_000_000_000_000_000_000_000 + u128::from(rng.next());

        let local = amount0_delta_down(b, a, liquidity).unwrap();
        let shared = get_amount0_delta(a, b, liquidity, false).unwrap();

        assert_eq!(u256_to_biguint(local), reference(a, b, liquidity), "{a} {b} {liquidity}");
        assert!(shared == local || shared == local + U256::ONE, "{shared} against {local}");
    }

    let local = amount0_delta_down(U256::from(7), U256::from(3), 4).unwrap();
    assert_eq!(local, u("60364314296582352452223962160"));
    assert_eq!(
        get_amount0_delta(U256::from(3), U256::from(7), 4, false).unwrap(),
        u("60364314296582352452223962161")
    );
}

/// An EMA of 1 that one second moves a 601st of the way to the top of the clamp re-anchors the
/// discount at `2^32 - floor(kept * 1 / ema)` = `2^32`, which a `uint32` cannot hold.
#[test]
fn test_a_reanchored_discount_beyond_uint32_is_refused() {
    let curve = Curve::try_new(true, -168_600).unwrap();
    let before = observation(U256::from(1), TS - 1, 0, 1_000_000);

    let result = observe(&curve, &before, curve.far_end, 5, TS);

    assert!(
        matches!(&result, Err(SimulationError::InvalidInput(message, _))
            if message.contains("the re-anchored discount 4294967296 does not fit uint32")),
        "{result:?}"
    );
}

/// An EMA of 1e36 above the fill's end price makes the new discount `2^32 - floor(end * 2^32 /
/// ema)` = `2^32`, beyond `uint32`.
#[test]
fn test_a_fills_new_discount_beyond_uint32_is_refused() {
    let mut fixture = token0_fixture();
    fixture.spot = u("1000000000000000000000000000000000000");
    fixture.ema = fixture.spot;

    let result = size_fill(
        &fixture.curve,
        &FillRequest {
            spot: fixture.spot,
            fill_price: fixture.fill_price,
            ema: fixture.ema,
            amount_in: u(HUGE),
            inventory: u(HUGE),
            max_clip: u(HUGE),
            filled: U256::ZERO,
        },
    );

    assert!(
        matches!(&result, Err(SimulationError::InvalidInput(message, _))
            if message.contains("the fill's discount 4294967296 does not fit uint32")),
        "{result:?}"
    );
}

/// A whole-input fill leaves the pool nothing to swap: `PoolManager.swap` still runs, and v4-core's
/// `Pool.swap` returns at once on a zero amount (`if (params.amountSpecified == 0) return ...`
/// before its price checks and its loop), so the swap leg costs the 3,000 of a no-op. The route
/// around it (`base` 150,000 and `settlement` 30,000 of the Uniswap v4 costs) is charged as for
/// any swap, with the hook's two frames of a fill (171,820 and 10,475) and the PoolManager's
/// 1,689 for each of the two calls. The total of such a swap was not measured on chain.
#[test]
fn test_a_whole_input_fill_costs_the_route_the_noop_swap_and_the_hook_frames() {
    let pool = funded_pool(true, 10_000, PLENTY);
    let (launched, quote) = tokens(true);

    let result = pool
        .get_amount_out(big("1000000000000000000"), &launched, &quote)
        .unwrap();

    assert_eq!(result.amount, big("47658354437"));
    assert_eq!(result.gas, big("368673"));
}

// ---------------------------------------------------------------------------------------------
// Attributes, deltas, decoding and registration
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

/// The attributes of pool 0xa138ea3bc220ee819a81be441ad1fd95f84e5ff228a6c1d862376c66e60a01a5 as
/// its `PoolData` and `Bid` storage words at block 8365800 give them: fee 10000, tokenIs0 0,
/// start tick -168600, `maxClip` 11862044423356365469.
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
    assert!(!decoded.token_is_currency0());
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
#[case::ema_of_0(EMA_SQRT_PRICE_ATTRIBUTE, vec![0; 20])]
#[case::start_tick_without_a_curve(START_TICK_ATTRIBUTE, 0x007f_ffffi32.to_be_bytes().to_vec())]
#[case::start_tick_below_int24(START_TICK_ATTRIBUTE, 0xff7f_ffffu32.to_be_bytes().to_vec())]
#[case::observed_tick_below_int24(OBSERVED_TICK_ATTRIBUTE, 0xff7f_ffffu32.to_be_bytes().to_vec())]
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
    assert_eq!(handler.state, expected);
}

/// The curve depends on the start tick and the orientation alone, so a delta that leaves both
/// reuses the handler's curve; one that changes either rebuilds it. The state a delta of the
/// changed attributes gives equals the state decoded afresh from the full set, whose curve is
/// built without a base, and differs from the sample's.
#[rstest]
#[case::another_start_tick(true, -170_000)]
#[case::the_other_orientation(false, -168_500)]
fn test_a_delta_that_changes_the_start_tick_or_the_orientation_rebuilds_the_curve(
    #[case] token_is_currency0: bool,
    #[case] start_tick: i32,
) {
    let mut handler = AscendHookHandler::new(ASCEND_HOOK_ELYSIUM, sample_state(), TS);
    let changed: HashMap<String, Bytes> = [
        (TOKEN_IS_CURRENCY0_ATTRIBUTE.to_string(), Bytes::from(vec![u8::from(token_is_currency0)])),
        (START_TICK_ATTRIBUTE.to_string(), Bytes::from(start_tick.to_be_bytes().to_vec())),
    ]
    .into();
    let mut full = attributes(&sample_state());
    full.extend(changed.clone());

    transition(&mut handler, changed, &[]).unwrap();

    assert_eq!(handler.state, AscendPoolState::from_attributes(&full).unwrap());
    assert_ne!(handler.state.curve, sample_state().curve);
}

/// A cap that swallows the whole price at `x0` breaks the invariant `size_fill` keeps (its cap is
/// below the band's quote), which is a fault of the model and not a bad swap. At `x0 = 2^96` a
/// cap of the liquidity leaves a price move of 0 (token0) or an inverse price of 0 (mirrored).
#[rstest]
#[case::token0(true, "reaches a price of 0")]
#[case::mirrored(false, "reaches an infinite price")]
fn test_a_cap_beyond_the_band_is_a_fault_of_the_model(
    #[case] token_is_currency0: bool,
    #[case] reached: &str,
) {
    let curve = Curve::try_new(token_is_currency0, -168_600).unwrap();

    let result =
        tokens_for_capped_quote(&curve, U256::from(1) << 96usize, U256::from(curve.liquidity));

    assert!(
        matches!(&result, Err(SimulationError::FatalError(message)) if message.contains(reached)),
        "{result:?}"
    );
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

    assert_eq!(handler.state, sample_state());
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
    assert_eq!(handler.state, sample_state());
    assert_eq!(handler.block_timestamp, TS);
}

#[test]
fn test_the_creator_needs_the_blocks_timestamp() {
    let snapshot = BareSnapshot::with_attributes(attributes(&sample_state()));

    let no_time = AscendHookCreator.instantiate_hook_handler(snapshot.params(ASCEND_HOOK_ELYSIUM));

    assert!(matches!(no_time, Err(InvalidSnapshotError::ValueError(_))));
}

#[test]
fn test_the_creator_needs_every_attribute() {
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

/// The address bits of the hook grant `beforeSwap`, `afterSwap` and both return deltas
/// (0x28ec), which the generic permission check reads.
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

#[test]
fn test_the_attribute_names_are_the_ones_the_indexer_writes() {
    assert_eq!(
        POOL_ATTRIBUTES,
        [
            "ascend_fee_pips",
            "ascend_token_is_currency0",
            "ascend_start_tick",
            "ascend_ema_sqrt_price_x96",
            "ascend_ema_updated_at",
            "ascend_observed_tick",
            "ascend_discount",
            "ascend_bid_inventory",
            "ascend_bid_filled_in_window",
            "ascend_bid_window_start",
            "ascend_bid_max_clip",
        ]
    );
    let names: Vec<String> = attributes(&sample_state())
        .into_keys()
        .collect();
    assert_eq!(names.len(), POOL_ATTRIBUTES.len());
    assert!(POOL_ATTRIBUTES
        .iter()
        .all(|name| names.contains(&name.to_string())));
}

/// An `int24` takes its extremes, which a `uint32` or a wider integer would not tell apart.
#[rstest]
#[case::int24_min(-(1 << 23))]
#[case::int24_max((1 << 23) - 1)]
fn test_an_observed_tick_at_the_int24_extremes_decodes(#[case] observed: i32) {
    let mut attrs = attributes(&sample_state());
    attrs.insert(OBSERVED_TICK_ATTRIBUTE.into(), Bytes::from(observed.to_be_bytes().to_vec()));

    let decoded = AscendPoolState::from_attributes(&attrs).unwrap();

    assert_eq!(decoded.observation.tick, observed);
}

// ---------------------------------------------------------------------------------------------
// Fees, refusals and gas of each path
// ---------------------------------------------------------------------------------------------

/// A buy of 1 raw unit pays a fee of 1 (the ceiling) and leaves the pool nothing to swap, so the
/// swapper's output is 0 and the swap is refused.
#[test]
fn test_a_buy_that_leaves_the_swapper_nothing_is_refused() {
    let pool = funded_pool(true, 10_000, "0");

    assert_refused(&buy(&pool, true, "1"), "buy of 1");
}

/// A pool whose clip is spent by a window opened at `window_start`.
fn pool_with_a_spent_window(window_start: u64) -> UniswapV4State {
    let clip = 50_000_000_000_000u64;
    let spent = Bid {
        inventory: u(PLENTY),
        filled: U256::from(clip),
        window_start,
        max_clip: u128::from(clip),
    };
    let state = pool_state(
        true,
        10_000,
        observation(u("17418037835569639047771018"), TS, -168_500, 2_147_483),
        spent,
    );
    pool(state, u(SQRT_M168500), -168_500, TS)
}

/// A window whose 600 s have run out resets in the quote: with the clip spent and the window
/// opened 599 s ago the sell is the AMM's alone (47658354435 for 1e18), and with it opened 600 s
/// ago the sell fills and pays 47658354437 after its fee.
#[rstest]
#[case::still_open(TS - 599, "47658354435")]
#[case::ran_out(TS - 600, "47658354437")]
fn test_a_window_that_ran_out_resets_when_a_fill_uses_it(
    #[case] window_start: u64,
    #[case] expected: &str,
) {
    let pool = pool_with_a_spent_window(window_start);

    assert_eq!(sell(&pool, true, "1000000000000000000").unwrap(), big(expected));
}

/// The state a fill on a window that ran out returns opens a new window at the block with only
/// that fill in it.
#[test]
fn test_a_fill_on_a_window_that_ran_out_opens_a_new_one() {
    let ran_out = pool_with_a_spent_window(TS - 600);
    let (launched, quote) = tokens(true);
    let next = ran_out
        .get_amount_out(big("1000000000000000000"), &launched, &quote)
        .unwrap()
        .new_state;
    let hook = hook_of(
        next.as_any()
            .downcast_ref::<UniswapV4State>()
            .unwrap(),
    )
    .state;
    assert_eq!(hook.bid.window_start, TS);
    assert_eq!(hook.bid.filled, U256::from(48_139_751_956u64));
}

#[rstest]
fn test_the_hook_states_its_spot_fee_in_both_directions(#[values(true, false)] zero_for_one: bool) {
    let pool = funded_pool(true, 10_000, "0");

    let fees = hook_of(&pool)
        .spot_fees(zero_for_one)
        .unwrap()
        .expect("the hook states its fee");

    assert_eq!(fees.hook_fee_pips, 10_000);
    assert!(fees.lp_fee_override.is_none());
}

#[test]
fn test_the_hook_states_its_fee_as_a_fraction() {
    let pool = funded_pool(true, 10_000, "0");
    let hook = hook_of(&pool);
    let params = SwapParams {
        zero_for_one: true,
        amount_specified: I256::try_from(-1).unwrap(),
        sqrt_price_limit: U256::ZERO,
    };
    assert_eq!(hook.fee(&pool, params).unwrap(), 0.01);
}

fn after_swap_gas(pool: &UniswapV4State, zero_for_one: bool, amount: i128, amm_out: i128) -> u64 {
    let (spot, tick) = pool.price_and_tick();
    let (amount0, amount1) = if zero_for_one { (-amount, amm_out) } else { (amm_out, -amount) };
    let params = AfterSwapParameters {
        context: state_context(spot, tick),
        sender: Address::ZERO,
        swap_params: swap_params(zero_for_one, I256::try_from(-amount).unwrap()),
        delta: BalanceDelta::new(
            I128::try_from(amount0).unwrap(),
            I128::try_from(amount1).unwrap(),
        ),
        hook_data: Bytes::new(),
    };
    hook_of(pool)
        .after_swap(params, None, None)
        .expect("a hook output")
        .gas_estimate
}

/// The gas of each path as `debug_traceCall` measured it on Elysium (see the constants in
/// `hook_handler.rs` for the transactions and blocks): the hook's `beforeSwap` and `afterSwap`
/// frames of a buy, of a sell the hook does not consider filling, of one it considers and the
/// library leaves unfilled (a bid of 1), and of one it fills.
#[rstest]
#[case::buy("0", false, 50_203, 5_402)]
#[case::sell_without_inventory("0", true, 17_388, 20_319)]
#[case::sell_with_a_bid_of_1("1", true, 47_094, 20_319)]
#[case::sell_with_a_fill(PLENTY, true, 171_820, 10_475)]
fn test_each_path_costs_the_gas_measured_on_chain(
    #[case] inventory: &str,
    #[case] zero_for_one: bool,
    #[case] before: u64,
    #[case] after: u64,
) {
    let pool = funded_pool(true, 10_000, inventory);
    let amount = 1_000_000_000_000_000_000i128;

    let (_, _, before_gas) =
        before_swap_delta(&pool, zero_for_one, -amount, u(SQRT_M168500), -168_500);
    let after_gas = after_swap_gas(&pool, zero_for_one, amount, 1_000_000);

    assert_eq!((before_gas, after_gas), (before, after));
    // 1,689 is the lower median of the four transactions' own PoolManager overhead.
    assert_eq!(hook_of(&pool).pool_manager_call_overhead(), 1_689);
}
