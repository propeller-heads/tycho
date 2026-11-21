//! Tests of the Montra hook handler on its own and through `UniswapV4State`. Expected values are
//! Elysium chain data: transactions and `eth_call` probes of the deployed hook named by block and
//! hash beside each test, and the recorded swaps, limits and V4Quoter quotes in
//! `tests/assets/hooks/montra/replay.json`.
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt::Display,
    fs,
    path::Path,
    str::FromStr,
};

use alloy::primitives::{aliases::U24, Address, I128, I256, U256};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use rstest::rstest;
use serde::Deserialize;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        token::Token,
        Chain, ChangeType,
    },
    simulation::{
        errors::SimulationError,
        protocol_sim::{
            Balances, BlockContext, Price, ProtocolSim, QueryPoolSwapParams, SwapConstraint,
        },
    },
    Bytes,
};

use super::{
    decode_paused,
    hook_handler::{
        input_absorbed_at_price, MontraHookHandler, MONTRA_BUY_AFTER_SWAP_GAS,
        MONTRA_BUY_BEFORE_SWAP_GAS, MONTRA_HOOK_DISPATCH_GAS, MONTRA_HOOK_ELYSIUM,
        MONTRA_SELL_AFTER_SWAP_GAS, MONTRA_SELL_BEFORE_SWAP_GAS,
    },
    MontraPoolConfig, EARLY_BUY_FEE_START_PIPS_ATTRIBUTE, EARLY_BUY_FEE_UNTIL_ATTRIBUTE,
    EARLY_BUY_FEE_WINDOW_SECONDS_ATTRIBUTE, LP_CROSSED_AT_ATTRIBUTE, LP_FEE_PIPS_ATTRIBUTE,
    LP_HELD_SECONDS_ATTRIBUTE, PAUSED_ATTRIBUTE, QUOTE_IS_CURRENCY0_ATTRIBUTE,
    SEED_TICK_LOWER_ATTRIBUTE, SEED_TICK_UPPER_ATTRIBUTE, STAGE_ATTRIBUTE,
    TOTAL_FEE_PIPS_ATTRIBUTE,
};
use crate::{
    evm::protocol::{
        uniswap_v4::{
            hooks::{
                hook_handler::HookHandler,
                hook_handler_creator::{
                    initialize_hook_handlers, instantiate_hook_handler,
                    test_fixtures::BareSnapshot, HookCreationParams,
                },
                models::{
                    AfterSwapParameters, BalanceDelta, BeforeSwapParameters, StateContext,
                    SwapOutcome, SwapParams,
                },
            },
            state::{UniswapV4Fees, UniswapV4State},
        },
        utils::uniswap::{
            lp_fee::{DYNAMIC_FEE_FLAG, OVERRIDE_FEE_FLAG},
            tick_list::TickInfo,
            tick_math::{get_sqrt_ratio_at_tick, MAX_TICK},
        },
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

/// The terms of one pool, written the way the Elysium manifest of the with-hooks package emits
/// them, and read from the recording under its field names.
#[derive(Clone, Copy, Deserialize)]
struct Terms {
    #[serde(rename = "total_fee_pips")]
    total: u32,
    #[serde(rename = "lp_fee_pips")]
    lp: u32,
    #[serde(deserialize_with = "flag_from_u8")]
    quote_is_currency0: bool,
    #[serde(rename = "early_buy_fee_until")]
    early_until: u64,
    #[serde(rename = "early_buy_fee_start_pips")]
    early_start: u32,
    #[serde(rename = "early_buy_fee_window")]
    early_window: u64,
    stage: u8,
    lp_crossed_at: u64,
    #[serde(rename = "lp_held_seconds")]
    lp_held: u64,
    #[serde(rename = "seed_tick_lower")]
    seed_lower: i32,
    #[serde(rename = "seed_tick_upper")]
    seed_upper: i32,
}

/// The recording writes `quoteIsCurrency0` as 0 or 1.
fn flag_from_u8<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    match u8::deserialize(deserializer)? {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(serde::de::Error::custom(format!("{other} is not a 0 or 1 flag"))),
    }
}

/// A pool after graduation with no anti-snipe window: 1% total, 0.1% LP fee, quote = currency0,
/// seed [-887200, 169800] as on pool 0x850fd929....
const PLAIN: Terms = Terms {
    total: 10_000,
    lp: 1_000,
    quote_is_currency0: true,
    early_until: 0,
    early_start: 0,
    early_window: 0,
    stage: 2,
    lp_crossed_at: 0,
    lp_held: 0,
    seed_lower: -887_200,
    seed_upper: 169_800,
};

fn attributes(terms: Terms) -> HashMap<String, Bytes> {
    let u64_attr = |v: u64| Bytes::from(v.to_be_bytes().to_vec());
    let tick_attr = |v: i32| Bytes::from(v.to_be_bytes().to_vec());
    [
        (TOTAL_FEE_PIPS_ATTRIBUTE, u64_attr(terms.total.into())),
        (LP_FEE_PIPS_ATTRIBUTE, u64_attr(terms.lp.into())),
        (QUOTE_IS_CURRENCY0_ATTRIBUTE, Bytes::from(vec![u8::from(terms.quote_is_currency0)])),
        (EARLY_BUY_FEE_UNTIL_ATTRIBUTE, u64_attr(terms.early_until)),
        (EARLY_BUY_FEE_START_PIPS_ATTRIBUTE, u64_attr(terms.early_start.into())),
        (EARLY_BUY_FEE_WINDOW_SECONDS_ATTRIBUTE, u64_attr(terms.early_window)),
        (STAGE_ATTRIBUTE, Bytes::from(vec![terms.stage])),
        (LP_CROSSED_AT_ATTRIBUTE, u64_attr(terms.lp_crossed_at)),
        (LP_HELD_SECONDS_ATTRIBUTE, u64_attr(terms.lp_held)),
        (SEED_TICK_LOWER_ATTRIBUTE, tick_attr(terms.seed_lower)),
        (SEED_TICK_UPPER_ATTRIBUTE, tick_attr(terms.seed_upper)),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value))
    .collect()
}

fn config(terms: Terms) -> MontraPoolConfig {
    MontraPoolConfig::from_attributes(&attributes(terms)).expect("valid terms decode")
}

fn handler(terms: Terms, timestamp: u64) -> MontraHookHandler {
    MontraHookHandler::new(MONTRA_HOOK_ELYSIUM, config(terms), false, timestamp)
}

fn swap_params(zero_for_one: bool, amount_specified: i128) -> SwapParams {
    SwapParams {
        zero_for_one,
        amount_specified: I256::try_from(amount_specified).expect("i128 fits I256"),
        sqrt_price_limit: U256::ZERO,
    }
}

fn context() -> StateContext {
    StateContext {
        currency_0: Address::ZERO,
        currency_1: Address::repeat_byte(1),
        fees: UniswapV4Fees::new(0, 0, DYNAMIC_FEE_FLAG),
        tick_spacing: 200,
        sqrt_price_x96: U256::ZERO,
        tick: 0,
    }
}

fn before_swap_params(zero_for_one: bool, amount_specified: i128) -> BeforeSwapParameters {
    BeforeSwapParameters {
        context: context(),
        sender: Address::ZERO,
        swap_params: swap_params(zero_for_one, amount_specified),
        hook_data: Bytes::new(),
    }
}

/// Montra's handler installs no new hook after a swap: its stage machine advances from the
/// indexer's `PoolConfig` writes.
#[test]
fn test_handler_after_swap_keeps_the_hook() {
    let next = handler(PLAIN, 0)
        .handler_after_swap(&context(), &swap_params(true, -1_000))
        .unwrap();

    assert!(next.is_none());
}

/// `currentBuyFeeRate` of pool 0x8dba98f9... at block 6001266 with only the block time
/// overridden: start rate 990000 pips, total fee 20000, a 10-second window starting at
/// 1794584110.
#[rstest]
#[case::before_window(-5, 990_000)]
#[case::window_start(0, 990_000)]
#[case::one_second_in(1, 893_000)]
#[case::three_seconds_in(3, 699_000)]
#[case::five_seconds_in(5, 505_000)]
#[case::seven_seconds_in(7, 311_000)]
#[case::nine_seconds_in(9, 117_000)]
#[case::window_end(10, 20_000)]
#[case::after_window(11, 20_000)]
#[case::long_after(100, 20_000)]
fn test_buy_fee_pips_matches_current_buy_fee_rate_on_a_ten_second_window(
    #[case] seconds_after_start: i64,
    #[case] expected: u32,
) {
    let t0 = 1_794_584_110_i64;
    let terms = Terms {
        total: 20_000,
        lp: 0,
        early_until: 1_794_584_120,
        early_start: 990_000,
        early_window: 10,
        stage: 0,
        ..PLAIN
    };
    let timestamp = u64::try_from(t0 + seconds_after_start).expect("positive");
    assert_eq!(
        config(terms)
            .buy_fee_pips(timestamp)
            .expect("in range"),
        expected
    );
}

/// The same probe on pool 0xa4d4ff7a... at block 2681095 (start rate 990000 pips, total fee
/// 10000, a 1800-second window starting at 1794171257): the decay is floored, so the rate rounds
/// up.
#[rstest]
#[case::one(1, 989_456)]
#[case::seven(7, 986_189)]
#[case::thirteen(13, 982_923)]
#[case::four_fifty_one(451, 744_456)]
#[case::one_second_before_the_end(1_799, 10_545)]
fn test_buy_fee_pips_rounds_the_rate_up_on_a_thirty_minute_window(
    #[case] seconds_after_start: u64,
    #[case] expected: u32,
) {
    let terms = Terms {
        total: 10_000,
        lp: 0,
        early_until: 1_794_171_257 + 1_800,
        early_start: 990_000,
        early_window: 1_800,
        stage: 0,
        ..PLAIN
    };
    let rate = config(terms)
        .buy_fee_pips(1_794_171_257 + seconds_after_start)
        .expect("in range");
    assert_eq!(rate, expected);
}

/// V4Quoter on pool 0x61a799e0... at block 713191 (stage 3, lpCrossedAt 1793853345, held
/// 28): the LP fee switches on at exactly 600 s held, and stage 4 always charges it.
#[rstest]
#[case::held_598(3, 598, None)]
#[case::held_599(3, 599, None)]
#[case::held_600(3, 600, Some(1_000))]
#[case::held_601(3, 601, Some(1_000))]
#[case::lp_active(4, 0, Some(1_000))]
#[case::graduated(2, 10_000, None)]
#[case::launched(0, 10_000, None)]
fn test_lp_fee_in_force_switches_on_at_six_hundred_seconds_held(
    #[case] stage: u8,
    #[case] held_total: u64,
    #[case] expected: Option<u32>,
) {
    let crossed_at = 1_793_853_345;
    let terms = Terms { stage, lp_crossed_at: crossed_at, lp_held: 28, ..PLAIN };
    assert_eq!(config(terms).lp_fee_in_force_pips(crossed_at + held_total - 28), expected);
}

/// An LP fee of 0 at `LpActive` is no LP fee in force: the swap runs at the pool's own slot0
/// fee, which is 0 too.
#[test]
fn test_a_zero_lp_fee_is_no_lp_fee_in_force() {
    let terms = Terms { lp: 0, stage: 4, ..PLAIN };
    assert_eq!(config(terms).lp_fee_in_force_pips(1_000), None);
}

#[test]
fn test_apply_block_reports_only_a_quoting_change() {
    let window = Terms {
        total: 10_000,
        lp: 0,
        early_until: 2_000,
        early_start: 990_000,
        early_window: 100,
        stage: 0,
        ..PLAIN
    };
    let mut in_window = handler(window, 1_950);
    assert!(in_window.apply_block(&BlockContext::new(2, 1_951)), "the rate decays every second");
    assert!(!in_window.apply_block(&BlockContext::new(2, 1_951)), "a repeated block is a no-op");
    let mut after_window = handler(window, 2_000);
    assert!(!after_window.apply_block(&BlockContext::new(3, 2_500)), "the rate stays at T");

    let holding = Terms { stage: 3, lp_crossed_at: 1_000, lp_held: 0, ..PLAIN };
    let mut crossing = handler(holding, 1_598);
    assert!(!crossing.apply_block(&BlockContext::new(4, 1_599)), "held 599 s");
    assert!(crossing.apply_block(&BlockContext::new(5, 1_600)), "held 600 s: the LP fee turns on");
    assert!(!crossing.apply_block(&BlockContext::new(6, 1_700)), "and stays on");

    for stage in [0, 4] {
        let mut settled = handler(Terms { stage, ..PLAIN }, 1_000);
        assert!(!settled.apply_block(&BlockContext::new(7, 9_000)), "stage {stage}");
    }
}

/// The trace of tx 0x02e351ec... (block 713192): a buy of 198000000000000000 on a total-10000,
/// lp-1000 pool, the swap that switches its LP fee on, makes `beforeSwap` return a specified
/// delta of 1782000000000000 (h = 9000 pips), and the pool charges the LP fee of 1000.
#[test]
fn test_before_swap_on_a_buy_with_the_lp_fee_on() {
    let terms = Terms { stage: 3, lp_crossed_at: 1_000, lp_held: 600, ..PLAIN };
    let out = handler(terms, 1_000)
        .before_swap(before_swap_params(true, -198_000_000_000_000_000), None, None)
        .expect("a buy settles");

    assert_eq!(
        out.result
            .amount_delta
            .get_specified_delta(),
        I128::try_from(1_782_000_000_000_000_i128).expect("fits")
    );
    assert_eq!(
        out.result
            .amount_delta
            .get_unspecified_delta(),
        I128::ZERO
    );
    assert_eq!(out.result.fee, U24::from(OVERRIDE_FEE_FLAG | 1_000), "LP fee 1000, overriding");
    assert_eq!(out.gas_estimate, MONTRA_BUY_BEFORE_SWAP_GAS);
}

#[test]
fn test_before_swap_on_a_sell_with_the_lp_fee_off() {
    let out = handler(PLAIN, 1_000)
        .before_swap(before_swap_params(false, -1_000_000), None, None)
        .expect("a sell settles");

    assert_eq!(out.result.amount_delta.as_i256(), I256::ZERO);
    assert_eq!(out.result.fee, U24::ZERO);
    assert_eq!(out.gas_estimate, MONTRA_SELL_BEFORE_SWAP_GAS);
}

#[rstest]
#[case::zero(I256::ZERO, "SwapAmountCannotBeZero")]
#[case::exact_output(I256::try_from(1_000).unwrap(), "exact-output")]
#[case::one_past_int128_max(-(I256::try_from(i128::MAX).unwrap() + I256::ONE), "int128")]
fn test_before_swap_refuses_what_the_pool_reverts(#[case] amount: I256, #[case] reason: &str) {
    let mut params = before_swap_params(true, 0);
    params.swap_params.amount_specified = amount;

    let err = handler(PLAIN, 1_000)
        .before_swap(params, None, None)
        .expect_err("refused");

    let SimulationError::InvalidInput(message, None) = err else { panic!("{err:?}") };
    assert!(message.contains(reason), "{message}");
}

#[test]
fn test_before_swap_on_a_paused_pool() {
    let paused = MontraHookHandler::new(MONTRA_HOOK_ELYSIUM, config(PLAIN), true, 1_000);
    assert!(!paused.swaps_enabled());
    let err = paused
        .before_swap(before_swap_params(true, -1_000), None, None)
        .expect_err("paused");
    assert!(matches!(err, SimulationError::InvalidInput(ref m, None) if m.contains("paused")));
    assert!(handler(PLAIN, 1_000).swaps_enabled());
}

/// Any non-empty value other than `[0]` pauses, whichever reason a pausing component encodes.
#[rstest]
#[case::empty(vec![], false)]
#[case::zero(vec![0], false)]
#[case::one(vec![1], true)]
#[case::two(vec![2], true)]
#[case::three(vec![3], true)]
#[case::two_bytes(vec![0, 0], true)]
fn test_decode_paused(#[case] value: Vec<u8>, #[case] paused: bool) {
    assert_eq!(decode_paused(&Bytes::from(value)), paused);
}

/// Tx 0xe221ea27... (block 5985683): a sell on a 1% pool with no LP fee grossed
/// 283517864799935707 and the hook took 2835178647999357 in `afterSwap`.
#[test]
fn test_after_swap_takes_a_sells_fee_from_the_output() {
    let terms = Terms { lp: 0, ..PLAIN };
    let params = AfterSwapParameters {
        context: context(),
        sender: Address::ZERO,
        swap_params: swap_params(false, -6_662_372_754_642_133_047_879_487),
        delta: BalanceDelta::new(
            I128::try_from(283_517_864_799_935_707_i128).expect("fits"),
            I128::try_from(-6_662_372_754_642_133_047_879_487_i128).expect("fits"),
        ),
        hook_data: Bytes::new(),
    };

    let out = handler(terms, 1_000)
        .after_swap(params, None, None)
        .expect("a sell settles");

    assert_eq!(out.result, I128::try_from(2_835_178_647_999_357_i128).expect("fits"));
    assert_eq!(out.gas_estimate, MONTRA_SELL_AFTER_SWAP_GAS);
}

#[test]
fn test_after_swap_takes_nothing_from_a_buys_output() {
    let params = AfterSwapParameters {
        context: context(),
        sender: Address::ZERO,
        swap_params: swap_params(true, -1_000_000),
        delta: BalanceDelta::new(
            I128::try_from(-990_000_i128).expect("fits"),
            I128::try_from(5_000_000_i128).expect("fits"),
        ),
        hook_data: Bytes::new(),
    };
    let out = handler(PLAIN, 1_000)
        .after_swap(params, None, None)
        .expect("a buy settles");
    assert_eq!(out.result, I128::ZERO);
    assert_eq!(out.gas_estimate, MONTRA_BUY_AFTER_SWAP_GAS);
}

/// A swap whose output is 0 is refused, while one unit of output, on which a sell's fee
/// floors to 0, settles. Quote = currency0 in `PLAIN`, so zero-for-one is a buy.
#[rstest]
#[case::buy_of_nothing(true, 0, false)]
#[case::sell_of_nothing(false, 0, false)]
#[case::buy_of_one_unit(true, 1, true)]
#[case::sell_of_one_unit(false, 1, true)]
fn test_after_swap_refuses_an_output_of_zero(
    #[case] zero_for_one: bool,
    #[case] output: i128,
    #[case] settles: bool,
) {
    let output = I128::try_from(output).expect("fits");
    let input = I128::try_from(-1_000_i128).expect("fits");
    let params = AfterSwapParameters {
        context: context(),
        sender: Address::ZERO,
        swap_params: swap_params(zero_for_one, -1_000),
        delta: if zero_for_one {
            BalanceDelta::new(input, output)
        } else {
            BalanceDelta::new(output, input)
        },
        hook_data: Bytes::new(),
    };

    let result = handler(PLAIN, 1_000).after_swap(params, None, None);

    if settles {
        assert_eq!(result.expect("one unit settles").result, I128::ZERO);
    } else {
        let err = result.expect_err("an output of 0");
        assert!(
            matches!(&err, SimulationError::InvalidInput(m, None) if m.contains("output is 0")),
            "{err:?}"
        );
    }
}

/// The opening is seedTickUpper (169800) for quote = currency0, where a sell is one-for-zero and
/// raises the price; seedTickLower for quote = currency1, where a sell lowers it.
#[rstest]
#[case::quote0_sell_at_opening(true, false, 0, None)]
#[case::quote0_sell_past_opening(true, false, 1, Some("PastOpening"))]
#[case::quote0_sell_short_of_opening(true, false, -1, None)]
#[case::quote1_sell_at_opening(false, true, 0, None)]
#[case::quote1_sell_past_opening(false, true, -1, Some("PastOpening"))]
#[case::quote0_buy_anywhere(true, true, 1, None)]
fn test_check_swap_outcome_refuses_a_sell_past_the_opening(
    #[case] quote_is_currency0: bool,
    #[case] zero_for_one: bool,
    #[case] offset: i64,
    #[case] reason: Option<&str>,
) {
    let terms = Terms { quote_is_currency0, seed_lower: -116_400, seed_upper: 169_800, ..PLAIN };
    let opening = if quote_is_currency0 { 169_800 } else { -116_400 };
    let at = get_sqrt_ratio_at_tick(opening).expect("in range");
    let sqrt_price = match offset {
        1 => at + U256::from(1),
        -1 => at - U256::from(1),
        _ => at,
    };
    let result = handler(terms, 1_000).check_swap_outcome(
        zero_for_one,
        &SwapOutcome { sqrt_price_x96: sqrt_price, amount_unfilled: U256::ZERO },
    );
    match reason {
        None => result.expect("settles"),
        Some(reason) => {
            let err = result.expect_err("reverts");
            assert!(
                matches!(err, SimulationError::InvalidInput(ref m, None) if m.contains(reason))
            );
        }
    }
}

#[test]
fn test_check_swap_outcome_refuses_a_partial_fill() {
    let err = handler(PLAIN, 1_000)
        .check_swap_outcome(
            true,
            &SwapOutcome { sqrt_price_x96: U256::from(1) << 96, amount_unfilled: U256::from(7) },
        )
        .expect_err("PartialFillRejected");
    assert!(matches!(err, SimulationError::InvalidInput(ref m, None) if m.contains("PartialFill")));
}

#[test]
fn test_spot_fees_net_of_the_lp_fee() {
    let terms = Terms {
        total: 20_000,
        lp: 3_000,
        early_until: 2_000,
        early_start: 990_000,
        early_window: 100,
        stage: 4,
        ..PLAIN
    };
    // quote = currency0, so zero-for-one is a buy: rate 990000 before the window starts.
    let buy = handler(terms, 1_900)
        .spot_fees(true)
        .unwrap()
        .unwrap();
    assert_eq!((buy.lp_fee_override, buy.hook_fee_pips), (Some(3_000), 987_000));
    let sell = handler(terms, 1_900)
        .spot_fees(false)
        .unwrap()
        .unwrap();
    assert_eq!((sell.lp_fee_override, sell.hook_fee_pips), (Some(3_000), 17_000));
}

/// Creation params for the Montra hook over `snapshot`, at the block `timestamp` if any.
fn creation_params(snapshot: &BareSnapshot, timestamp: Option<u64>) -> HookCreationParams<'_> {
    let params = snapshot.params(MONTRA_HOOK_ELYSIUM);
    match timestamp {
        Some(t) => params.with_block_timestamp(t),
        None => params,
    }
}

#[test]
fn test_the_creator_is_registered_for_montra_on_elysium() {
    initialize_hook_handlers().expect("registration succeeds");
    let mut attributes = attributes(PLAIN);
    attributes.insert("paused".to_string(), Bytes::from(vec![1]));
    let snapshot = BareSnapshot::with_attributes(attributes);

    let handler = instantiate_hook_handler(
        Chain::Elysium,
        &MONTRA_HOOK_ELYSIUM,
        creation_params(&snapshot, Some(5)),
    )
    .expect("the montra creator serves Elysium");

    let expected = MontraHookHandler::new(MONTRA_HOOK_ELYSIUM, config(PLAIN), true, 5);
    assert!(handler.is_equal(&expected));
}

#[test]
fn test_the_creator_does_not_serve_the_hook_address_on_another_chain() {
    initialize_hook_handlers().expect("registration succeeds");
    let snapshot = BareSnapshot::with_attributes(attributes(PLAIN));
    let Err(err) = instantiate_hook_handler(
        Chain::Ethereum,
        &MONTRA_HOOK_ELYSIUM,
        creation_params(&snapshot, Some(5)),
    ) else {
        panic!("the generic VM creator wants its own attributes")
    };
    assert!(matches!(err, InvalidSnapshotError::MissingAttribute(ref n) if n == "balance_owner"));
}

#[test]
fn test_the_creator_requires_the_block_timestamp() {
    initialize_hook_handlers().expect("registration succeeds");
    let snapshot = BareSnapshot::with_attributes(attributes(PLAIN));
    let Err(err) = instantiate_hook_handler(
        Chain::Elysium,
        &MONTRA_HOOK_ELYSIUM,
        creation_params(&snapshot, None),
    ) else {
        panic!("no timestamp, no quote")
    };
    assert!(matches!(err, InvalidSnapshotError::ValueError(ref m) if m.contains("timestamp")));
}

#[rstest]
#[case::missing("montra_stage", None, "missing")]
#[case::short_u64("montra_total_fee_pips", Some(vec![0, 1]), "8-byte")]
#[case::past_uint24("montra_lp_fee_pips", Some((1u64 << 24).to_be_bytes().to_vec()), "uint24")]
#[case::bad_flag("montra_quote_is_currency0", Some(vec![2]), "0 or 1")]
#[case::bad_stage("montra_stage", Some(vec![5]), "naming a stage")]
#[case::short_tick("montra_seed_tick_lower", Some(vec![0, 0, 1]), "4-byte")]
fn test_from_attributes_rejects_malformed_terms(
    #[case] name: &str,
    #[case] value: Option<Vec<u8>>,
    #[case] reason: &str,
) {
    let mut attributes = attributes(PLAIN);
    match value {
        Some(value) => attributes.insert(name.to_string(), Bytes::from(value)),
        None => attributes.remove(name),
    };
    let err = MontraPoolConfig::from_attributes(&attributes).expect_err("rejected");
    match err {
        InvalidSnapshotError::MissingAttribute(missing) => {
            assert_eq!((missing.as_str(), reason), (name, "missing"))
        }
        InvalidSnapshotError::ValueError(message) => {
            assert!(message.contains(name) && message.contains(reason), "{message}")
        }
        other @ InvalidSnapshotError::VMError(_) => panic!("{other:?}"),
    }
}

/// Terms the hook's `register` and `lowerTotalFee` refuse decode, and `check_terms` refuses them
/// naming the attribute. The caps are the hook's `MAX_TOTAL_FEE_PIPS()` (100000),
/// `MAX_EARLY_BUY_FEE_PIPS()` (990000) and `MAX_EARLY_BUY_FEE_WINDOW()` (1800), read from
/// `MONTRA_HOOK_ELYSIUM` at block 6002000; `check_terms` accepts each cap itself. It refuses an
/// anti-snipe start at or below the total, as `register` does: on a fork of Elysium at block
/// 7964700 it reverted `InvalidFeeConfig()` for a start of 20000, 19999, 10000 and 0 against a
/// total of 20000 with a 600 s window, and accepted 20001. It refuses a window longer than
/// `earlyBuyFeeUntil` too; with no window it checks neither the start nor the window.
#[rstest]
#[case::total_above_cap(Terms { total: 100_001, ..PLAIN }, Some(TOTAL_FEE_PIPS_ATTRIBUTE))]
#[case::total_at_cap(Terms { total: 100_000, ..PLAIN }, None)]
#[case::lp_above_total(Terms { lp: 10_001, ..PLAIN }, Some(LP_FEE_PIPS_ATTRIBUTE))]
#[case::lp_at_total(Terms { lp: 10_000, ..PLAIN }, None)]
#[case::start_below_total(
    Terms { early_until: 2_000, early_start: 5_000, early_window: 100, ..PLAIN },
    Some(EARLY_BUY_FEE_START_PIPS_ATTRIBUTE)
)]
#[case::start_at_total(
    Terms { early_until: 2_000, early_start: 10_000, early_window: 100, ..PLAIN },
    Some(EARLY_BUY_FEE_START_PIPS_ATTRIBUTE)
)]
#[case::start_zero_with_a_window(
    Terms { early_until: 2_000, early_start: 0, early_window: 100, ..PLAIN },
    Some(EARLY_BUY_FEE_START_PIPS_ATTRIBUTE)
)]
#[case::start_above_total(
    Terms { early_until: 2_000, early_start: 10_001, early_window: 100, ..PLAIN },
    None
)]
#[case::window_longer_than_until(
    Terms { early_until: 1_000, early_start: 990_000, early_window: 1_500, ..PLAIN },
    Some(EARLY_BUY_FEE_UNTIL_ATTRIBUTE)
)]
#[case::start_above_cap(
    Terms { early_until: 2_000, early_start: 990_001, early_window: 100, ..PLAIN },
    Some(EARLY_BUY_FEE_START_PIPS_ATTRIBUTE)
)]
#[case::start_at_cap(
    Terms { early_until: 2_000, early_start: 990_000, early_window: 100, ..PLAIN },
    None
)]
#[case::window_above_cap(
    Terms { early_until: 2_000, early_start: 990_000, early_window: 1_801, ..PLAIN },
    Some(EARLY_BUY_FEE_WINDOW_SECONDS_ATTRIBUTE)
)]
#[case::window_at_cap(
    Terms { early_until: 2_000, early_start: 990_000, early_window: 1_800, ..PLAIN },
    None
)]
#[case::no_window(
    Terms { early_until: 0, early_start: 990_001, early_window: 1_801, ..PLAIN },
    None
)]
fn test_check_terms(#[case] terms: Terms, #[case] refused: Option<&str>) {
    let config = MontraPoolConfig::from_attributes(&attributes(terms)).expect("terms decode");
    match (config.check_terms(), refused) {
        (Ok(()), None) => {}
        (Err(SimulationError::InvalidInput(message, None)), Some(name)) => {
            assert!(message.contains(name), "{message}")
        }
        (result, _) => panic!("expected {refused:?}, got {result:?}"),
    }
    // Every quote of a pool with refused terms is refused the same way, at any time, inside
    // the window and long after it.
    for timestamp in [1_950, 1_000_000] {
        match (config.buy_fee_pips(timestamp), refused) {
            (Ok(_), None) | (Err(SimulationError::InvalidInput(..)), Some(_)) => {}
            (result, _) => panic!("buy_fee_pips at {timestamp}: {result:?}"),
        }
    }
}

/// Terms refused at decode are checked again after an update: lowering the start to the total
/// refuses a pool that was accepted, and raising it back accepts it again.
#[test]
fn test_with_updates_rechecks_the_terms() {
    let window = Terms { early_until: 2_000, early_start: 10_001, early_window: 100, ..PLAIN };
    let accepted = config(window);
    assert!(accepted.check_terms().is_ok());

    let at_total = accepted
        .with_updates(&attributes(Terms { early_start: 10_000, ..window }))
        .expect("decodes");
    assert!(matches!(at_total.check_terms(), Err(SimulationError::InvalidInput(..))));

    let back = at_total
        .with_updates(&attributes(window))
        .expect("decodes");
    assert_eq!(back, accepted);
}

/// `from_attributes` requires every name `POOL_CONFIG_ATTRIBUTES` lists, which is also the list
/// `delta_transition` refuses to see deleted, so the two cannot drift apart.
#[test]
fn test_every_pool_config_attribute_is_required() {
    for name in super::POOL_CONFIG_ATTRIBUTES {
        let mut attributes = attributes(PLAIN);
        assert!(attributes.remove(name).is_some(), "{name} is not in the test map");
        match MontraPoolConfig::from_attributes(&attributes) {
            Err(InvalidSnapshotError::MissingAttribute(missing)) => assert_eq!(missing, name),
            other => panic!("without {name}: {other:?}"),
        }
    }
    assert_eq!(attributes(PLAIN).len(), super::POOL_CONFIG_ATTRIBUTES.len());
}

fn delta(updated: Vec<(&str, Bytes)>, deleted: &[&str]) -> ProtocolStateDelta {
    ProtocolStateDelta {
        component_id: "pool".into(),
        updated_attributes: updated
            .into_iter()
            .map(|(n, v)| (n.to_string(), v))
            .collect(),
        deleted_attributes: deleted
            .iter()
            .map(|n| n.to_string())
            .collect::<HashSet<_>>(),
    }
}

#[test]
fn test_delta_transition_applies_updates() {
    let mut hook = MontraHookHandler::new(MONTRA_HOOK_ELYSIUM, config(PLAIN), true, 1_000);
    hook.delta_transition(
        delta(
            vec![
                ("montra_total_fee_pips", Bytes::from(25_000u64.to_be_bytes().to_vec())),
                ("montra_stage", Bytes::from(vec![4])),
                ("liquidity", Bytes::from(vec![1])),
            ],
            &["paused", "ticks/200/net-liquidity"],
        ),
        &HashMap::new(),
        &Balances::default(),
    )
    .expect("valid delta");

    let expected = config(Terms { total: 25_000, stage: 4, ..PLAIN });
    assert_eq!(hook, MontraHookHandler::new(MONTRA_HOOK_ELYSIUM, expected, false, 1_000));
}

#[rstest]
#[case::malformed(delta(vec![("montra_stage", Bytes::from(vec![9]))], &[]))]
#[case::deleted_term(delta(vec![], &["montra_total_fee_pips"]))]
fn test_delta_transition_rejects_a_bad_delta(#[case] bad: ProtocolStateDelta) {
    let mut hook = handler(PLAIN, 1_000);
    hook.delta_transition(bad, &HashMap::new(), &Balances::default())
        .expect_err("rejected");
    assert_eq!(hook, handler(PLAIN, 1_000));
}

/// A delta without a pause entry leaves a paused pool paused, whatever else it updates: only a
/// pause value or the deletion of `paused` changes it.
#[test]
fn test_delta_transition_without_a_pause_entry_keeps_the_pool_paused() {
    let mut hook = MontraHookHandler::new(MONTRA_HOOK_ELYSIUM, config(PLAIN), true, 1_000);
    hook.delta_transition(
        delta(vec![(TOTAL_FEE_PIPS_ATTRIBUTE, Bytes::from(5_000u64.to_be_bytes().to_vec()))], &[]),
        &HashMap::new(),
        &Balances::default(),
    )
    .expect("valid delta");

    let expected = config(Terms { total: 5_000, lp: 1_000, ..PLAIN });
    assert_eq!(hook, MontraHookHandler::new(MONTRA_HOOK_ELYSIUM, expected, true, 1_000));
}

/// A pause value other than `[0]` and `[1]` applies and pauses the pool.
#[test]
fn test_delta_transition_applies_any_pause_value() {
    let mut hook = handler(PLAIN, 1_000);
    hook.delta_transition(
        delta(vec![(PAUSED_ATTRIBUTE, Bytes::from(vec![7]))], &[]),
        &HashMap::new(),
        &Balances::default(),
    )
    .expect("any pause value applies");
    assert_eq!(hook, MontraHookHandler::new(MONTRA_HOOK_ELYSIUM, config(PLAIN), true, 1_000));
}

/// The hook's input absorbed without moving the price, at prices and liquidities worked by
/// hand. With no fee, a one-for-zero input of `less` moves `sqrtP` by `floor(less * 2^96 / L)`,
/// so `L = 2^100` absorbs 15 and `L = 2^110` absorbs 16383, the largest sells the V4Quoter
/// settled from the opening of pool 0x850fd929... with that liquidity past it (the recording's
/// opening probes: `eth_call` at Elysium block 6024400 with the pool's `slot0` and liquidity
/// overridden). A zero-for-one input stays put while `L * 2^96 > (sqrtP - 1) * sqrtP * less`: at
/// `sqrtP = 2^96` and `L = 2^100` that is `less < 2^100 / (2^96 - 1)`, so 16, and at
/// `sqrtP = 2^97` it is 4. A 3000-pip fee swaps `floor(input * 0.997)`, so 17 and 16 gross keep
/// 16 and 15 net.
#[rstest]
#[case::one_for_zero_2_100(1, 1u128 << 100, false, 0, 15)]
#[case::one_for_zero_2_110(1, 1u128 << 110, false, 0, 16_383)]
#[case::one_for_zero_below_2_96(1, (1u128 << 96) - 1, false, 0, 0)]
#[case::one_for_zero_with_fee(1, 1u128 << 100, false, 3_000, 16)]
#[case::zero_for_one_2_100(1, 1u128 << 100, true, 0, 16)]
#[case::zero_for_one_at_a_higher_price(2, 1u128 << 100, true, 0, 4)]
#[case::zero_for_one_with_fee(1, 1u128 << 100, true, 3_000, 17)]
#[case::no_liquidity(1, 0, true, 0, 0)]
fn test_input_absorbed_at_price(
    #[case] sqrt_price_over_2_96: u64,
    #[case] liquidity: u128,
    #[case] zero_for_one: bool,
    #[case] fee_pips: u32,
    #[case] expected: u64,
) {
    let sqrt_price = U256::from(sqrt_price_over_2_96) << 96;
    let absorbed =
        input_absorbed_at_price(sqrt_price, liquidity, zero_for_one, fee_pips).expect("fits");
    assert_eq!(absorbed, U256::from(expected));
}

// ---- Through `UniswapV4State`, against recorded Elysium data ----

/// The hook's deployment block, where the recording's eras start: `HOOK_DEPLOYMENT_BLOCK` of
/// `tests/assets/hooks/montra/record_replay.py`, which `replay.json` does not carry.
const HOOK_DEPLOYMENT_BLOCK: u64 = 588_510;
/// The length of one era of the recording, in blocks: `ERA_BLOCKS` of the same script.
const ERA_BLOCKS: u64 = 500_000;

#[derive(Deserialize)]
struct Recording {
    head: u64,
    swaps: Vec<RecordedSwap>,
    hookless_swap_gas: Vec<u64>,
    limits: Vec<RecordedLimit>,
    spot_probes: Vec<RecordedSpot>,
}

#[derive(Clone, Deserialize)]
struct RecordedPool {
    pool_id: Bytes,
    currency0: Bytes,
    currency1: Bytes,
    decimals0: u32,
    decimals1: u32,
    tick_spacing: i32,
    #[serde(deserialize_with = "decimal")]
    sqrt_price_x96: U256,
    tick: i32,
    protocol_fee: u32,
    #[serde(deserialize_with = "decimal")]
    liquidity: u128,
    #[serde(deserialize_with = "decimal_ticks")]
    ticks: Vec<(i32, i128)>,
    config: Terms,
    timestamp: u64,
}

impl RecordedPool {
    /// The address and decimals of the pool's quote asset, the currency the hook charges.
    fn quote(&self) -> (&Bytes, u32) {
        if self.config.quote_is_currency0 {
            (&self.currency0, self.decimals0)
        } else {
            (&self.currency1, self.decimals1)
        }
    }
}

#[derive(Deserialize)]
struct RecordedSwap {
    #[serde(flatten)]
    pool: RecordedPool,
    tx: String,
    block: u64,
    zero_for_one: bool,
    #[serde(deserialize_with = "decimal")]
    amount_in: BigUint,
    #[serde(deserialize_with = "decimal")]
    amount_out: BigUint,
    gas: RecordedGas,
}

impl RecordedSwap {
    fn is_buy(&self) -> bool {
        self.zero_for_one == self.pool.config.quote_is_currency0
    }
}

/// The `gasUsed` of a swap's call frames, from `debug_traceTransaction`.
#[derive(Deserialize)]
struct RecordedGas {
    /// The PoolManager's `swap` frame, the hook's frames included.
    swap: u64,
    before_swap: u64,
    after_swap: u64,
    /// Whether this is the transaction's first swap through the hook.
    first_in_tx: bool,
}

#[derive(Deserialize)]
struct RecordedLimit {
    #[serde(flatten)]
    pool: RecordedPool,
    block: u64,
    zero_for_one: bool,
    #[serde(deserialize_with = "decimal")]
    limit_in: BigUint,
    #[serde(deserialize_with = "optional_decimal")]
    out_at_limit: Option<BigUint>,
}

#[derive(Deserialize)]
struct RecordedSpot {
    #[serde(flatten)]
    pool: RecordedPool,
    #[serde(deserialize_with = "decimal")]
    buy_amount_in: BigUint,
    #[serde(deserialize_with = "decimal")]
    buy_amount_out: BigUint,
    #[serde(deserialize_with = "decimal")]
    sell_amount_in: BigUint,
    #[serde(deserialize_with = "decimal")]
    sell_amount_out: BigUint,
}

/// `text`, a number the recording writes as a decimal string since JSON numbers lose precision
/// past 2^53, parsed as a `T`.
fn parse_decimal<T, E>(text: &str) -> Result<T, E>
where
    T: FromStr,
    T::Err: Display,
    E: serde::de::Error,
{
    text.parse()
        .map_err(|e| E::custom(format!("{text} is not a decimal amount: {e}")))
}

fn decimal<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: FromStr,
    T::Err: Display,
{
    parse_decimal(&String::deserialize(deserializer)?)
}

fn optional_decimal<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: FromStr,
    T::Err: Display,
{
    Option::<String>::deserialize(deserializer)?
        .map(|text| parse_decimal(&text))
        .transpose()
}

/// The recording's `[tick, liquidityNet]` pairs, each net a decimal string.
fn decimal_ticks<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<(i32, i128)>, D::Error> {
    Vec::<(i32, String)>::deserialize(deserializer)?
        .into_iter()
        .map(|(tick, net)| Ok((tick, parse_decimal(&net)?)))
        .collect()
}

fn recording() -> Recording {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/assets/hooks/montra/replay.json");
    let raw =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("the montra recording parses")
}

fn token(address: &Bytes, decimals: u32) -> Token {
    Token::new(address, "T", decimals, 0, &[Some(10_000)], Chain::Elysium, 100)
}

/// The pool as the Elysium manifest of the with-hooks package publishes it, a
/// `uniswap_v4_hooks` component in the indexer's attribute formats.
fn component(pool: &RecordedPool) -> ComponentWithState {
    let mut attributes = attributes(pool.config);
    attributes.insert("liquidity".into(), Bytes::from(pool.liquidity.to_be_bytes().to_vec()));
    attributes.insert(
        "sqrt_price_x96".into(),
        Bytes::from(
            pool.sqrt_price_x96
                .to_be_bytes::<32>()
                .to_vec(),
        ),
    );
    attributes.insert("tick".into(), Bytes::from(pool.tick.to_be_bytes()[1..].to_vec()));
    // slot0 packs two 12-bit protocol fees, zero-for-one in the low 12 bits.
    for (direction, fee) in [
        ("protocol_fees/zero2one", pool.protocol_fee & 0xfff),
        ("protocol_fees/one2zero", pool.protocol_fee >> 12),
    ] {
        attributes.insert(direction.into(), Bytes::from(fee.to_be_bytes().to_vec()));
    }
    for (index, net) in &pool.ticks {
        if *net != 0 {
            attributes.insert(
                format!("ticks/{index}/net-liquidity"),
                Bytes::from(net.to_be_bytes().to_vec()),
            );
        }
    }
    let component = ProtocolComponent {
        id: pool.pool_id.to_string(),
        protocol_system: "uniswap_v4_hooks".into(),
        protocol_type_name: "uniswap_v4_pool".into(),
        chain: Chain::Elysium,
        tokens: vec![pool.currency0.clone(), pool.currency1.clone()],
        contract_addresses: vec![],
        static_attributes: HashMap::from([
            ("tick_spacing".into(), Bytes::from(pool.tick_spacing.to_be_bytes().to_vec())),
            ("key_lp_fee".into(), Bytes::from(DYNAMIC_FEE_FLAG.to_be_bytes().to_vec())),
            ("hooks".into(), Bytes::from(MONTRA_HOOK_ELYSIUM.to_vec())),
            ("pool_id".into(), pool.pool_id.clone()),
        ]),
        change: ChangeType::Creation,
        creation_tx: Bytes::from(vec![0u8; 32]),
        created_at: Default::default(),
    };
    ComponentWithState {
        state: ProtocolComponentState {
            component_id: pool.pool_id.to_string(),
            attributes,
            balances: HashMap::new(),
        },
        component,
        component_tvl: None,
        entrypoints: vec![],
    }
}

fn decode(pool: &RecordedPool) -> (UniswapV4State, Token, Token) {
    initialize_hook_handlers().expect("registration succeeds");
    let token0 = token(&pool.currency0, pool.decimals0);
    let token1 = token(&pool.currency1, pool.decimals1);
    let tokens = HashMap::from([
        (token0.address.clone(), token0.clone()),
        (token1.address.clone(), token1.clone()),
    ]);
    let header = BlockHeader { timestamp: pool.timestamp, ..Default::default() };
    let state = tokio_test::block_on(UniswapV4State::try_from_with_header(
        component(pool),
        header,
        &HashMap::new(),
        &tokens,
        &DecoderContext::new().chain(Chain::Elysium),
    ))
    .unwrap_or_else(|e| panic!("pool {} decodes: {e:?}", pool.pool_id));
    (state, token0, token1)
}

fn in_out(token0: Token, token1: Token, zero_for_one: bool) -> (Token, Token) {
    if zero_for_one {
        (token0, token1)
    } else {
        (token1, token0)
    }
}

/// Every recorded exact-input Montra swap, quoted from the pool state the swap started from
/// at its block's timestamp, returns what the swapper received on chain, to the wei.
#[test]
fn test_replay_reproduces_every_recorded_swap() {
    let swaps = recording().swaps;
    assert_eq!(swaps.len(), RECORDED_SWAPS, "the committed recording's swap count");
    let mut mismatches = vec![];
    for swap in &swaps {
        let (state, token0, token1) = decode(&swap.pool);
        let (token_in, token_out) = in_out(token0, token1, swap.zero_for_one);
        match state.get_amount_out(swap.amount_in.clone(), &token_in, &token_out) {
            Ok(out) if out.amount == swap.amount_out => {}
            other => mismatches.push(format!("{}: {other:?} vs {}", swap.tx, swap.amount_out)),
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {}:\n{}",
        mismatches.len(),
        swaps.len(),
        mismatches.join("\n")
    );
}

/// The number of swaps in the committed recording.
const RECORDED_SWAPS: usize = 393;

/// The recording covers both directions, the LP fee on and off and anti-snipe buys.
#[test]
fn test_replay_covers_the_fee_regimes() {
    let swaps = recording().swaps;
    let regime = |swap: &RecordedSwap| {
        let config = config(swap.pool.config);
        let lp_on = config
            .lp_fee_in_force_pips(swap.pool.timestamp)
            .is_some();
        let buy_rate = config
            .buy_fee_pips(swap.pool.timestamp)
            .expect("in range");
        let in_window = swap.is_buy() && buy_rate > swap.pool.config.total;
        (swap.is_buy(), lp_on, in_window)
    };
    let seen: HashSet<_> = swaps.iter().map(regime).collect();
    for wanted in [
        (true, false, false),
        (true, true, false),
        (false, false, false),
        (false, true, false),
        (true, false, true),
    ] {
        assert!(
            seen.contains(&wanted),
            "no recorded swap with (buy, lp on, in window) = {wanted:?}"
        );
    }
}

/// The recording holds swaps from every 500,000-block era between the hook's
/// deployment and the recorded head, at every stage, and buys and sells in each quote kind and
/// orientation: the native quote (always `currency0`), ERC-20 quotes as `currency0` and as
/// `currency1`, and 8-decimal quotes as both.
#[test]
fn test_replay_covers_eras_stages_and_quote_kinds() {
    let recording = recording();
    let swaps = &recording.swaps;

    let eras: BTreeSet<u64> = swaps
        .iter()
        .map(|swap| (swap.block - HOOK_DEPLOYMENT_BLOCK) / ERA_BLOCKS)
        .collect();
    let last_era = (recording.head - HOOK_DEPLOYMENT_BLOCK) / ERA_BLOCKS;
    assert_eq!(eras, (0..=last_era).collect::<BTreeSet<_>>(), "eras");

    let stages: BTreeSet<u8> = swaps
        .iter()
        .map(|swap| swap.pool.config.stage)
        .collect();
    assert_eq!(stages, BTreeSet::from([0, 1, 2, 3, 4]), "stages");

    // (quote is native, quote decimals, quote is currency0, buy)
    let kinds: HashSet<(bool, u32, bool, bool)> = swaps
        .iter()
        .map(|swap| {
            let (quote, decimals) = swap.pool.quote();
            let native = *quote == Bytes::from([0u8; 20]);
            (native, decimals, swap.pool.config.quote_is_currency0, swap.is_buy())
        })
        .collect();
    for buy in [true, false] {
        for wanted in [
            (true, 18, true, buy),
            (false, 18, true, buy),
            (false, 18, false, buy),
            (false, 8, true, buy),
            (false, 8, false, buy),
        ] {
            assert!(
                kinds.contains(&wanted),
                "no recorded swap with (native, decimals, quote is currency0, buy) = {wanted:?}"
            );
        }
    }
}

/// The lower median of `values`.
fn median(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    values[(values.len() - 1) / 2]
}

/// Each hook gas constant is the median of its call frame over the recording's swaps that are
/// their transaction's first through the hook, and the dispatch overhead is half the median
/// excess of those swaps' `swap` frames, net of both hook frames, over the median `swap` frame of
/// hookless Elysium swaps.
#[test]
fn test_hook_gas_constants_are_the_medians_of_the_recorded_swaps() {
    let recording = recording();
    let cold: Vec<&RecordedSwap> = recording
        .swaps
        .iter()
        .filter(|swap| swap.gas.first_in_tx)
        .collect();
    let frames = |buy: bool, frame: fn(&RecordedGas) -> u64| {
        median(
            cold.iter()
                .filter(|swap| swap.is_buy() == buy)
                .map(|swap| frame(&swap.gas))
                .collect(),
        )
    };
    let pool_manager_side = median(
        cold.iter()
            .map(|swap| swap.gas.swap - swap.gas.before_swap - swap.gas.after_swap)
            .collect(),
    );
    let hookless = median(recording.hookless_swap_gas);

    assert_eq!(MONTRA_BUY_BEFORE_SWAP_GAS, frames(true, |gas| gas.before_swap));
    assert_eq!(MONTRA_BUY_AFTER_SWAP_GAS, frames(true, |gas| gas.after_swap));
    assert_eq!(MONTRA_SELL_BEFORE_SWAP_GAS, frames(false, |gas| gas.before_swap));
    assert_eq!(MONTRA_SELL_AFTER_SWAP_GAS, frames(false, |gas| gas.after_swap));
    assert_eq!(MONTRA_HOOK_DISPATCH_GAS, (pool_manager_side - hookless) / 2);
}

/// The largest inputs the V4Quoter settles at a recorded block are `get_limits` exactly, and the
/// hook refuses one more unit.
///
/// Where the V4Quoter's output at its largest settling input is 0, every sell up to that input
/// pays out nothing, so the limit is 0 and both one unit and that input are refused. That holds
/// for the recording's dust sells and for its two opening probes on the currency0-quoted pool
/// 0x850fd929..., whose sells from the opening settle `floor((L - 1) / 2^96)` units with no
/// output; `test_input_absorbed_at_price` checks that amount.
///
/// The recording's third opening probe is a zero-for-one sell on the currency1-quoted pool
/// 0xff8294a2... from its opening tick -405400, with 2^100 liquidity past it. It settles
/// 45147744934076655418 for an output of 96: the walk down to the opening, plus what the
/// opening absorbs of a token0 input without moving the price.
#[test]
fn test_limits_match_the_bisected_chain_bounds() {
    let mut zero_output_bounds = 0;
    for limit in recording().limits {
        let (state, token0, token1) = decode(&limit.pool);
        let (token_in, token_out) = in_out(token0, token1, limit.zero_for_one);
        let label = format!("{} at {}", limit.pool.pool_id, limit.block);

        let (max_in, max_out) = state
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .unwrap_or_else(|e| panic!("{label}: {e:?}"));

        if limit.out_at_limit == Some(BigUint::ZERO) {
            zero_output_bounds += 1;
            assert_eq!((&max_in, &max_out), (&BigUint::ZERO, &BigUint::ZERO), "{label}");
            for amount in [BigUint::from(1u32), limit.limit_in] {
                let quote = state.get_amount_out(amount.clone(), &token_in, &token_out);
                assert!(
                    matches!(&quote, Err(SimulationError::InvalidInput(msg, None)) if msg.contains("output is 0")),
                    "{label}, {amount}: {quote:?}"
                );
            }
            continue;
        }
        assert_eq!(max_in, limit.limit_in, "{label}");
        if let Some(expected) = &limit.out_at_limit {
            assert_eq!(&max_out, expected, "{label}");
        }
        let past = state.get_amount_out(max_in + 1u32, &token_in, &token_out);
        assert_hook_refusal(&past, &label);
    }
    // The recording's dust sells on both pools and its two currency0-quoted opening probes.
    assert_eq!(zero_output_bounds, 4);
}

/// The spot price equals the V4Quoter's price of a small buy and of a small sell (the
/// sell's reciprocal), with the LP fee on and inside an anti-snipe window, and the fees raise it
/// above the fee-free pool price on both sides.
#[test]
fn test_spot_price_matches_small_v4_quoter_quotes() {
    for probe in recording().spot_probes {
        let (state, token0, token1) = decode(&probe.pool);
        let quote_is_currency0 = probe.pool.config.quote_is_currency0;
        let (quote, launch) = if quote_is_currency0 { (token0, token1) } else { (token1, token0) };
        let scale = |amount: &BigUint, decimals: u32| {
            amount.to_f64().expect("f64") /
                10f64.powi(i32::try_from(decimals).expect("decimals fit i32"))
        };
        let buy_price = scale(&probe.buy_amount_in, quote.decimals) /
            scale(&probe.buy_amount_out, launch.decimals);
        let sell_price = scale(&probe.sell_amount_in, launch.decimals) /
            scale(&probe.sell_amount_out, quote.decimals);

        let spot_buy = state
            .spot_price(&launch, &quote)
            .expect("buy side prices");
        let spot_sell = state
            .spot_price(&quote, &launch)
            .expect("sell side prices");
        // The quotes are of small amounts in raw units, so their ratio carries the rounding of
        // one raw output unit in about 1e7; 1e-6 relative leaves room for that and nothing for
        // a fee that is off by a single pip (1e-6 of the price).
        let close = |a: f64, b: f64| ((a - b) / b).abs() < 1e-6;
        assert!(
            close(spot_buy, buy_price),
            "{}: buy {spot_buy} vs {buy_price}",
            probe.pool.pool_id
        );
        assert!(
            close(spot_sell, sell_price),
            "{}: sell {spot_sell} vs {sell_price}",
            probe.pool.pool_id
        );

        let mut feeless = state;
        feeless.hook = None;
        assert!(
            spot_buy >
                feeless
                    .spot_price(&launch, &quote)
                    .expect("prices")
        );
        assert!(
            spot_sell >
                feeless
                    .spot_price(&quote, &launch)
                    .expect("prices")
        );
    }
}

/// On every recorded pool state, with the LP fee off and forced on and with outside
/// liquidity spanning the opening, `get_limits` is the exact edge of `get_amount_out` in both
/// directions, at fixed-seed random fractions of the limit too. A zero limit refuses one unit.
#[test]
fn test_limits_are_the_edge_of_quotes_on_recorded_states() {
    // A fixed-seed xorshift keeps the property coverage reproducible.
    let mut seed = 0x6d6f_6e74_7261_u64;
    let mut random = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let swaps = recording().swaps;
    let mut edges_checked = 0;
    let mut zero_limits = 0;
    // Every 7th swap (57 of 393) covers each era, stage and quote kind the recording holds
    // (test_replay_covers_eras_stages_and_quote_kinds) while keeping the exact-edge checks, about
    // 50 quotes per state, to a few seconds.
    for swap in swaps.iter().step_by(7) {
        for (stage, outside) in [(swap.pool.config.stage, false), (4, false), (4, true)] {
            let mut pool = swap.pool.clone();
            pool.config.stage = stage;
            if outside {
                // A full-range position of the pool's own liquidity, as outside LPs add on chain.
                let spacing = pool.tick_spacing;
                let extreme = MAX_TICK / spacing * spacing;
                let liquidity = i128::try_from(pool.liquidity).expect("fits");
                pool.liquidity *= 2;
                add_net(&mut pool, -extreme, liquidity);
                add_net(&mut pool, extreme, -liquidity);
            }
            let (state, token0, token1) = decode(&pool);
            for zero_for_one in [true, false] {
                let (token_in, token_out) = in_out(token0.clone(), token1.clone(), zero_for_one);
                let (max_in, _) = state
                    .get_limits(token_in.address.clone(), token_out.address.clone())
                    .expect("limits");
                if max_in == BigUint::ZERO {
                    let one = state.get_amount_out(BigUint::from(1u32), &token_in, &token_out);
                    assert_hook_refusal(&one, &format!("{} at a zero limit", swap.tx));
                    zero_limits += 1;
                    continue;
                }
                let fraction = BigUint::from(random() % 1_000_000 + 1);
                let inside = (&max_in * &fraction / 1_000_000u32).max(BigUint::from(1u32));
                state
                    .get_amount_out(inside.clone(), &token_in, &token_out)
                    .unwrap_or_else(|e| panic!("{} inside at {inside}: {e:?}", swap.tx));
                let past = state.get_amount_out(&max_in + 1u32, &token_in, &token_out);
                assert_hook_refusal(&past, &format!("{} one past {max_in}", swap.tx));
                edges_checked += 1;
            }
        }
    }
    assert!(
        edges_checked >= 100,
        "only {edges_checked} directions had a non-zero limit to check ({zero_limits} zero limits)"
    );
}

/// The stream path on Elysium: a recorded Montra pool arrives as a `uniswap_v4_hooks` snapshot,
/// is emitted
/// as a `UniswapV4State` carrying the native Montra handler, and quotes its recorded swap.
#[tokio::test]
async fn test_a_montra_pool_decodes_through_the_stream_decoder_on_elysium() {
    use tycho_client::feed::{
        synchronizer::{Snapshot, StateSyncMessage},
        FeedMessage,
    };

    use crate::evm::decoder::TychoStreamDecoder;

    initialize_hook_handlers().expect("registration succeeds");
    let swap = recording()
        .swaps
        .into_iter()
        .next()
        .expect("a recorded swap");
    let token0 = token(&swap.pool.currency0, swap.pool.decimals0);
    let token1 = token(&swap.pool.currency1, swap.pool.decimals1);
    let mut decoder = TychoStreamDecoder::new(Chain::Elysium);
    decoder.register_decoder::<UniswapV4State>("uniswap_v4_hooks");
    decoder
        .set_tokens(HashMap::from([
            (token0.address.clone(), token0.clone()),
            (token1.address.clone(), token1.clone()),
        ]))
        .await;
    let message = FeedMessage {
        state_msgs: HashMap::from([(
            "uniswap_v4_hooks".to_string(),
            StateSyncMessage {
                header: BlockHeader {
                    number: 1,
                    timestamp: swap.pool.timestamp,
                    ..Default::default()
                },
                snapshots: Snapshot {
                    states: HashMap::from([(swap.pool.pool_id.to_string(), component(&swap.pool))]),
                    vm_storage: HashMap::new(),
                },
                deltas: None,
                removed_components: HashMap::new(),
            },
        )]),
        sync_states: HashMap::new(),
    };

    let update = decoder
        .decode(&message)
        .await
        .expect("the message decodes");

    let state = update
        .states
        .get(&swap.pool.pool_id.to_string())
        .expect("the Montra pool reaches consumers")
        .as_any()
        .downcast_ref::<UniswapV4State>()
        .expect("a Montra component decodes into a UniswapV4State");
    assert!(state
        .hook
        .as_ref()
        .expect("a hook handler")
        .as_any()
        .is::<MontraHookHandler>());
    let (token_in, token_out) = in_out(token0, token1, swap.zero_for_one);
    let out = state
        .get_amount_out(swap.amount_in.clone(), &token_in, &token_out)
        .expect("quotes");
    assert_eq!(out.amount, swap.amount_out);
}

/// Asserts that `result` is the hook's own refusal of a swap at or past its bound: a reverting
/// `PartialFillRejected` or `PastOpening`, an output of 0, or an input past `type(int128).max`.
/// The pool's swap math answers `InvalidInput` for other causes too, which would hide a wrong
/// limit.
fn assert_hook_refusal<T: std::fmt::Debug>(result: &Result<T, SimulationError>, label: &str) {
    let refused = matches!(
        result,
        Err(SimulationError::InvalidInput(message, None))
            if message.starts_with("montra hook ") &&
                ["PartialFillRejected", "PastOpening", "output is 0", "int128"]
                    .iter()
                    .any(|reason| message.contains(reason))
    );
    assert!(refused, "{label}: {result:?}");
}

fn add_net(pool: &mut RecordedPool, index: i32, net: i128) {
    match pool
        .ticks
        .iter_mut()
        .find(|(i, _)| *i == index)
    {
        Some((_, value)) => *value += net,
        None => pool.ticks.push((index, net)),
    }
}

/// A pool launched on an anvil fork of Elysium at block 6024400: total 20000 pips,
/// LP fee 3000 not yet in force (stage 0), seed-only liquidity `seed_liquidity` on `seed`.
/// `quote_is_currency0` picks the quote: native HYPE (18 decimals) as currency0, or EQTSLA
/// (0x7559191f..., 8 decimals) as currency1. The launch token has 18 decimals.
fn fork_pool(
    tick_spacing: i32,
    quote_is_currency0: bool,
    (sqrt_price_x96, tick, liquidity): (&str, i32, u128),
    seed: (i32, i32),
    seed_liquidity: i128,
) -> RecordedPool {
    let address = |hex: &str| Bytes::from_str(hex).expect("an address");
    let (currency0, currency1, decimals1) = if quote_is_currency0 {
        (
            "0x0000000000000000000000000000000000000000",
            "0x00000000000000000000000000000000a4d40065",
            18,
        )
    } else {
        (
            "0x0000000000000000000000000000000000a4d4c9",
            "0x7559191f12413d122493fe43de6d0b4544c6579f",
            8,
        )
    };
    RecordedPool {
        pool_id: Bytes::from_str(
            "0x4561eb2a0b8241fa8863e5cf01bf34f49f6bbe2d68f0578543c0e7747d703519",
        )
        .expect("a pool id"),
        currency0: address(currency0),
        currency1: address(currency1),
        decimals0: 18,
        decimals1,
        tick_spacing,
        sqrt_price_x96: U256::from_str(sqrt_price_x96).expect("a sqrt price"),
        tick,
        protocol_fee: 0,
        liquidity,
        ticks: vec![(seed.0, seed_liquidity), (seed.1, -seed_liquidity)],
        config: Terms {
            total: 20_000,
            lp: 3_000,
            quote_is_currency0,
            early_until: 0,
            early_start: 0,
            early_window: 0,
            stage: 0,
            lp_crossed_at: 0,
            lp_held: 0,
            seed_lower: seed.0,
            seed_upper: seed.1,
        },
        timestamp: 1_794_600_000,
    }
}

/// The tickSpacing-60 fork pool, quoted in EQTSLA as currency1, seed [-415620, 887220] with
/// liquidity 94488204, at the given slot0.
fn spacing_60_pool(slot0: (&str, i32, u128)) -> RecordedPool {
    fork_pool(60, false, slot0, (-415_620, 887_220), 94_488_204)
}

/// tickSpacing 60, quote currency1, at its opening: tick -415620, seed [-415620, 887220].
const TS60_AT_OPENING: (&str, i32, u128) = ("74861267998464756638", -415_620, 94_488_204);
/// The same pool after a 1000000 buy through LaunchRouter.swapExactIn.
const TS60_AFTER_BUY: (&str, i32, u128) = ("821718038609514327151033264", -91_379, 94_488_204);
/// tickSpacing 1, quote currency0, at its opening: tick 169800 on the seed's upper edge with no
/// active liquidity, seed [-887272, 169800].
const TS1_AT_OPENING: (&str, i32, u128) = ("385350023123796245667272766987274", 169_800, 0);

/// Limits at tick spacings other than 200 are the V4Quoter's bisected bounds on the fork pools of
/// `fork_pool`. At spacing 60 the buy walk
/// ends at the seed's own extreme 887220, not at 887200; at spacing 1 the seed reaches
/// TickMath's MIN_TICK, and the buy walk stops at the executor's price limit
/// MIN_SQRT_PRICE + 1. One more unit than the limit is refused.
#[rstest]
#[case::spacing_60_quote1_buy_from_the_opening(
    spacing_60_pool(TS60_AT_OPENING),
    false,
    "1773886413929755962268062677"
)]
#[case::spacing_60_quote1_sell_after_a_buy(
    spacing_60_pool(TS60_AFTER_BUY),
    true,
    "99999990653848409"
)]
#[case::spacing_1_quote0_buy_clamped_at_the_price_limit(
    fork_pool(1, true, TS1_AT_OPENING, (-887_272, 169_800), 20_560_051_319_579),
    true,
    "386991580712988759160188230334647"
)]
fn test_limits_match_the_fork_bounds_at_other_tick_spacings(
    #[case] pool: RecordedPool,
    #[case] zero_for_one: bool,
    #[case] expected: &str,
) {
    let (state, token0, token1) = decode(&pool);
    let (token_in, token_out) = in_out(token0, token1, zero_for_one);

    let (max_in, _) = state
        .get_limits(token_in.address.clone(), token_out.address.clone())
        .expect("limits");

    assert_eq!(max_in, BigUint::from_str(expected).expect("a decimal amount"));
    let past = state.get_amount_out(max_in + 1u32, &token_in, &token_out);
    assert_hook_refusal(&past, "one past the limit");
}

/// The V4Quoter's quotes on the fork's spacing-60 pool: a 1000000 buy from the opening receives
/// 99999990653848384, and a 10000000000000 sell after it 959509.
#[rstest]
#[case::buy_from_the_opening(TS60_AT_OPENING, false, 1_000_000, 99_999_990_653_848_384)]
#[case::sell_after_a_buy(TS60_AFTER_BUY, true, 10_000_000_000_000, 959_509)]
fn test_quotes_match_the_fork_at_tick_spacing_60(
    #[case] slot0: (&str, i32, u128),
    #[case] zero_for_one: bool,
    #[case] amount_in: u64,
    #[case] expected_out: u64,
) {
    let (state, token0, token1) = decode(&spacing_60_pool(slot0));
    let (token_in, token_out) = in_out(token0, token1, zero_for_one);

    let out = state
        .get_amount_out(BigUint::from(amount_in), &token_in, &token_out)
        .expect("quotes");

    assert_eq!(out.amount, BigUint::from(expected_out));
}

/// A pool quoted in currency1 still at its opening (tick = seedTickLower, the seed active)
/// has no liquidity past the opening, so a sell's first step crosses the opening tick into zero
/// liquidity and every sell reverts. The sell limit is zero, and a sell of one unit is refused
/// by both `get_limits` and `get_amount_out`.
#[test]
fn test_a_sell_at_the_opening_with_no_liquidity_past_it_has_a_zero_limit() {
    let (state, token0, token1) = decode(&spacing_60_pool(TS60_AT_OPENING));
    let (token_in, token_out) = in_out(token0, token1, true);

    let limits = state
        .get_limits(token_in.address.clone(), token_out.address.clone())
        .expect("limits");
    let one = state.get_amount_out(BigUint::from(1u32), &token_in, &token_out);

    assert_eq!(limits, (BigUint::ZERO, BigUint::ZERO));
    assert_hook_refusal(&one, "one unit");
}

/// A paused pool keeps its spot price, while its swaps are refused and its limits are 0.
#[test]
fn test_paused_pool_through_the_state() {
    let (state, token0, token1) = decode(&spacing_60_pool(TS60_AFTER_BUY));
    let mut paused = state.clone();
    paused
        .delta_transition(
            delta(vec![(PAUSED_ATTRIBUTE, Bytes::from(vec![2]))], &[]),
            &HashMap::new(),
            &Balances::default(),
        )
        .expect("any pause value applies");

    assert_eq!(
        paused
            .spot_price(&token1, &token0)
            .expect("prices"),
        state
            .spot_price(&token1, &token0)
            .expect("prices")
    );
    let quote = paused.get_amount_out(BigUint::from(1_000u32), &token0, &token1);
    assert!(
        matches!(&quote, Err(SimulationError::InvalidInput(m, None)) if m.contains("paused")),
        "{quote:?}"
    );
    assert_eq!(
        paused
            .get_limits(token0.address, token1.address)
            .expect("limits"),
        (BigUint::ZERO, BigUint::ZERO)
    );
}

/// Terms the hook would refuse decode from a snapshot and apply through a delta. The pool's spot
/// price, quotes and limits then refuse them in both directions with `InvalidInput` naming the
/// attribute, also while the pool is paused. A total of 1000000 pips would take a sell's whole
/// output. The pool's own terms are those of `fork_pool`, at its timestamp 1794600000.
#[rstest]
#[case::lp_above_total(|terms| Terms { lp: 30_000, ..terms }, LP_FEE_PIPS_ATTRIBUTE)]
#[case::total_above_cap(|terms| Terms { total: 1_000_000, lp: 0, ..terms }, TOTAL_FEE_PIPS_ATTRIBUTE)]
#[case::start_above_cap(
    |terms| Terms { early_until: 1_794_600_100, early_start: 990_001, early_window: 100, ..terms },
    EARLY_BUY_FEE_START_PIPS_ATTRIBUTE
)]
#[case::window_above_cap(
    |terms| Terms { early_until: 1_794_600_100, early_start: 990_000, early_window: 1_801, ..terms },
    EARLY_BUY_FEE_WINDOW_SECONDS_ATTRIBUTE
)]
fn test_refused_terms_through_the_state(
    #[case] refused: fn(Terms) -> Terms,
    #[case] attribute: &str,
    #[values(false, true)] from_snapshot: bool,
    #[values(false, true)] paused: bool,
) {
    let pool = spacing_60_pool(TS60_AFTER_BUY);
    let terms = refused(pool.config);
    let (mut state, token0, token1) = if from_snapshot {
        decode(&RecordedPool { config: terms, ..pool })
    } else {
        let (mut state, token0, token1) = decode(&pool);
        let update =
            ProtocolStateDelta { updated_attributes: attributes(terms), ..delta(vec![], &[]) };
        state
            .delta_transition(update, &HashMap::new(), &Balances::default())
            .expect("refused terms apply");
        (state, token0, token1)
    };
    if paused {
        state
            .delta_transition(
                delta(vec![(PAUSED_ATTRIBUTE, Bytes::from(vec![1]))], &[]),
                &HashMap::new(),
                &Balances::default(),
            )
            .expect("a pause applies");
    }

    for (token_in, token_out) in [(&token0, &token1), (&token1, &token0)] {
        let results = [
            (
                "spot_price",
                state
                    .spot_price(token_in, token_out)
                    .map(drop),
            ),
            (
                "get_amount_out",
                state
                    .get_amount_out(BigUint::from(1_000u32), token_in, token_out)
                    .map(drop),
            ),
            (
                "get_limits",
                state
                    .get_limits(token_in.address.clone(), token_out.address.clone())
                    .map(|limits| {
                        // A paused pool cannot trade, so its limits are (0, 0) whatever its terms.
                        assert!(paused, "limits {limits:?} of a pool the hook refuses");
                        assert_eq!(limits, (BigUint::ZERO, BigUint::ZERO));
                    }),
            ),
        ];
        for (method, result) in results {
            if paused && method == "get_limits" {
                assert!(result.is_ok(), "{result:?}");
                continue;
            }
            assert!(
                matches!(&result, Err(SimulationError::InvalidInput(m, None)) if m.contains(attribute)),
                "{method} {} -> {}: {result:?}",
                token_in.address,
                token_out.address
            );
        }
    }
}

/// `query_pool_swap` runs the pool's swap math without the hook, which would leave out the hook's
/// fee and the LP fee it sets, so it refuses a Montra pool in both directions with
/// `InvalidInput` rather than answer amounts `get_amount_out` would not.
#[test]
fn test_query_pool_swap_refuses_a_montra_pool() {
    let (state, token0, token1) = decode(&spacing_60_pool(TS60_AFTER_BUY));
    for (token_in, token_out) in [(&token0, &token1), (&token1, &token0)] {
        let result = state.query_pool_swap(&QueryPoolSwapParams::new(
            token_in.clone(),
            token_out.clone(),
            SwapConstraint::PoolTargetPrice {
                target: Price::new(BigUint::from(1u32), BigUint::from(1u32)),
                tolerance: 0f64,
                min_amount_in: None,
                max_amount_in: None,
            },
        ));
        assert!(
            matches!(&result, Err(SimulationError::InvalidInput(m, None)) if m.contains("sets the swap's fees")),
            "{} -> {}: {result:?}",
            token_in.address,
            token_out.address
        );
    }
}

/// `max_gross_buy` is the largest gross buy whose net of the hook's fee fits: it fits and one
/// more unit does not, at fees across the hook's range and at nets around fee-rounding edges.
#[rstest]
fn test_max_gross_buy_is_the_largest_fitting_input(
    #[values(0, 1, 9_999, 10_000, 100_000, 989_999, 990_000)] hook_fee_pips: u32,
    #[values(0, 1, 99, 1_000_000, 123_456_789_012_345_678_901, u128::MAX)] max_net: u128,
) {
    let max_net = U256::from(max_net);
    let net = |gross: U256| gross - gross * U256::from(hook_fee_pips) / U256::from(1_000_000u32);
    let gross = super::hook_handler::max_gross_buy(max_net, hook_fee_pips).expect("fits");
    assert!(net(gross) <= max_net, "{gross} at {hook_fee_pips}");
    assert!(net(gross + U256::from(1)) > max_net, "{gross} + 1 at {hook_fee_pips}");
}

/// A pool quoted in `currency0` whose seed holds `liquidity` from the seed extreme of `terms` up
/// to its opening, plus `extra` ticks, with the price at `tick`: at the opening (no liquidity
/// active there), as a freshly seeded pool sits, or inside the seed.
fn seeded_state(liquidity: u128, extra: &[(i32, i128)], terms: Terms, tick: i32) -> UniswapV4State {
    let net = i128::try_from(liquidity).expect("fits");
    let mut ticks = vec![
        TickInfo::new(terms.seed_lower, net).unwrap(),
        TickInfo::new(terms.seed_upper, -net).unwrap(),
    ];
    ticks.extend(
        extra
            .iter()
            .map(|(index, net)| TickInfo::new(*index, *net).unwrap()),
    );
    ticks.sort_by_key(|tick| tick.index);
    let active = if tick < terms.seed_upper { liquidity } else { 0 };
    let mut state = UniswapV4State::new(
        active,
        get_sqrt_ratio_at_tick(tick).unwrap(),
        UniswapV4Fees::new(0, 0, DYNAMIC_FEE_FLAG),
        tick,
        200,
        ticks,
    )
    .unwrap();
    state.set_hook_handler(Box::new(handler(terms, 1_000)));
    state
}

/// The buy limit is capped at `type(int128).max`, the largest input a v4 swap delta carries: a
/// seed of 2^100 liquidity takes about 2^164 of the quote to walk to the seed extreme, and the
/// limit reports the cap instead, which `before_swap` accepts and one more unit it refuses.
#[test]
fn test_the_buy_limit_is_capped_at_int128_max() {
    let state = seeded_state(1 << 100, &[], PLAIN, PLAIN.seed_upper);
    let target = get_sqrt_ratio_at_tick(PLAIN.seed_lower).unwrap();
    let walk = state
        .amount_in_to_price(true, target, None)
        .expect("walks to the seed extreme");
    let int128_max = U256::from(u128::MAX >> 1);
    assert!(walk.amount_in > int128_max, "{}", walk.amount_in);

    let hook = handler(PLAIN, 1_000);
    let limit = hook
        .max_amount_in(&state, true)
        .expect("a limit")
        .expect("Montra answers");
    assert_eq!(limit, int128_max);

    let at_cap = -I256::from_raw(int128_max);
    for (amount, accepted) in [(at_cap, true), (at_cap - I256::ONE, false)] {
        let mut params = before_swap_params(true, 0);
        params.swap_params.amount_specified = amount;
        assert_eq!(
            hook.before_swap(params, None, None)
                .is_ok(),
            accepted,
            "{amount}"
        );
    }
}

/// v4 keeps a tick whose net liquidity is 0 in its bitmap while its gross liquidity is not, and
/// a swap step ends there; the indexed tick list drops it. A sell limit computed without such a
/// tick is at most 2 raw units below the limit with it (one from the extra step's input rounded
/// up, one from its LP fee rounded up), never above it, and a quote at it settles on the state
/// that has the tick.
#[rstest]
fn test_a_net_zero_tick_lowers_the_sell_limit_by_at_most_two_units(
    #[values(Terms { stage: 2, ..PLAIN }, Terms { stage: 4, ..PLAIN })] terms: Terms,
) {
    // A pool after some buys: the price at tick 100000 inside the seed, a net-0 tick at 120000
    // between it and the opening at 169800.
    let liquidity: u128 = 1 << 80;
    let indexed = seeded_state(liquidity, &[], terms, 100_000);
    let on_chain = seeded_state(liquidity, &[(120_000, 0)], terms, 100_000);

    let hook = handler(terms, 1_000);
    let sell = false;
    let limit = |state: &UniswapV4State| {
        hook.max_amount_in(state, sell)
            .expect("a limit")
            .expect("Montra answers")
    };
    let (indexed_limit, chain_limit) = (limit(&indexed), limit(&on_chain));
    assert!(indexed_limit <= chain_limit, "{indexed_limit} > {chain_limit}");
    assert!(chain_limit - indexed_limit <= U256::from(2), "{indexed_limit} vs {chain_limit}");

    let token0 = token(&Bytes::from(Address::ZERO.to_vec()), 18);
    let token1 = token(&Bytes::from(Address::repeat_byte(1).to_vec()), 18);
    on_chain
        .get_amount_out(
            crate::evm::protocol::u256_num::u256_to_biguint(indexed_limit),
            &token1,
            &token0,
        )
        .expect("the indexed limit settles on the chain's tick set");
}
