use alloy::primitives::U256;
use rstest::rstest;

use super::*;
use crate::evm::protocol::utils::slipstreams::observations::Observation;

const TICK_SPACING_FEE: u32 = 500;
const OTHER_HOOK: [u8; 20] = [0x77; 20];

fn observation(index: i32, block_timestamp: u32, tick_cumulative: i64) -> Observation {
    Observation {
        block_timestamp,
        tick_cumulative,
        seconds_per_liquidity_cumulative_x128: U256::ZERO,
        initialized: true,
        index,
    }
}

/// Two observations 600s apart whose tick cumulative fell by 7: a TWAP tick of -7/600, which
/// floors to -1 (truncation would give 0).
fn observations() -> Observations {
    Observations::new(vec![observation(0, 1_000, 0), observation(1, 1_600, -7)])
}

fn oracle(observations: &Observations, tick: i32) -> PoolOracle<'_> {
    PoolOracle {
        tick,
        liquidity: 1,
        observation_index: 1,
        observation_cardinality: 2,
        observations,
    }
}

fn execution(block: u64) -> Execution {
    Execution { block, timestamp: 1_600, first_in_block: false }
}

fn hook() -> AeroHookFee {
    AeroHookFee {
        default_swap_hook: Bytes::from(DYNAMIC_SWAP_FEE_HOOK.as_slice()),
        pool_swap_hook: Bytes::from([0u8; 20]),
        default_fee_cap: 50_000,
        seconds_ago: 600,
        ..Default::default()
    }
}

fn configured(base_fee: u32, fee_cap: u32, scaling_factor: u64) -> AeroHookFee {
    AeroHookFee { base_fee, fee_cap, scaling_factor, ..hook() }
}

fn fee_of(hook: &AeroHookFee, tick: i32) -> HookFee {
    let observations = observations();
    hook.resolve(&oracle(&observations, tick), TICK_SPACING_FEE, execution(10))
        .unwrap()
}

#[test]
fn twap_tick_rounds_towards_negative_infinity() {
    // |10 - (-1)| * 1e6 / 1e6 = 11; a truncated TWAP tick of 0 would give 10.
    let fee = fee_of(&configured(300, 5_000, 1_000_000), 10);

    assert_eq!(fee, HookFee { fee: 311, block_fee_to_store: Some(311), observed_twap: true });
}

#[test]
fn dynamic_fee_is_capped() {
    assert_eq!(fee_of(&configured(300, 305, 1_000_000), 10).fee, 305);
}

#[test]
fn zero_scaling_factor_uses_the_hook_defaults() {
    // The pool's own cap of 100 is ignored along with its zero scaling factor.
    let mut hook = configured(300, 100, 0);
    hook.default_scaling_factor = 2_000_000;

    assert_eq!(fee_of(&hook, 10).fee, 300 + 22);
}

#[test]
fn zero_base_fee_uses_the_tick_spacing_fee() {
    assert_eq!(fee_of(&configured(0, 5_000, 0), 10).fee, TICK_SPACING_FEE);
}

#[test]
fn zero_fee_indicator_charges_the_tick_spacing_fee_without_twap() {
    let fee = fee_of(&configured(ZERO_FEE_INDICATOR, 0, 1_000_000), 10);

    assert_eq!(
        fee,
        HookFee {
            fee: TICK_SPACING_FEE,
            block_fee_to_store: Some(TICK_SPACING_FEE),
            observed_twap: false
        }
    );
}

#[test]
fn zero_hook_fee_falls_back_to_the_tick_spacing_fee() {
    // A zero cap makes the hook return 0, which SwapHookLib replaces with the tick spacing fee.
    let fee = fee_of(&configured(300, 0, 1_000_000), 10);

    assert_eq!(fee.fee, TICK_SPACING_FEE);
    assert_eq!(fee.block_fee_to_store, None);
}

#[test]
fn hook_fees_are_capped_at_ten_percent() {
    let mut hook = configured(300, 5_000, 1_000_000);
    hook.initial_fee_enabled = true;
    hook.initial_fee = 200_000;

    assert_eq!(fee_of(&hook, 10).fee, 100_000);
}

#[test]
fn oracle_younger_than_the_window_adds_no_dynamic_fee() {
    let mut hook = configured(300, 5_000, 1_000_000);
    hook.seconds_ago = 601;

    assert_eq!(
        fee_of(&hook, 10),
        HookFee { fee: 300, block_fee_to_store: Some(300), observed_twap: false }
    );
}

#[test]
fn uninitialized_next_slot_reads_the_oldest_observation_at_index_zero() {
    let observations = Observations::new(vec![observation(0, 1_000, 0), observation(1, 1_600, -7)]);
    // Cardinality 3: slot 2 was never written, so the oldest observation is slot 0.
    let pool = PoolOracle { observation_cardinality: 3, ..oracle(&observations, 10) };

    let fee = configured(300, 5_000, 1_000_000)
        .resolve(&pool, TICK_SPACING_FEE, execution(10))
        .unwrap();

    assert!(fee.observed_twap);
}

#[test]
fn later_swaps_in_a_traded_block_pay_its_cached_fee() {
    let mut hook = configured(300, 5_000, 1_000_000);
    hook.store_block_fee(10, 450);
    let observations = observations();

    let same_block = hook
        .resolve(&oracle(&observations, 10), TICK_SPACING_FEE, execution(10))
        .unwrap();
    let next_block = hook
        .resolve(&oracle(&observations, 10), TICK_SPACING_FEE, execution(11))
        .unwrap();

    assert_eq!(same_block, HookFee { fee: 450, block_fee_to_store: None, observed_twap: false });
    assert_eq!(next_block.fee, 311);
}

#[rstest]
#[case::first_in_block(true, 25)]
#[case::position_unknown(false, 311)]
fn initial_fee_depends_on_the_assumed_position(#[case] first_in_block: bool, #[case] fee: u32) {
    let mut hook = configured(300, 5_000, 1_000_000);
    hook.initial_fee_enabled = true;
    hook.initial_fee = 25;
    let observations = observations();

    let resolved = hook
        .resolve(
            &oracle(&observations, 10),
            TICK_SPACING_FEE,
            Execution { first_in_block, ..execution(10) },
        )
        .unwrap();

    assert_eq!(resolved.fee, fee);
    // The block stores the dynamic fee, which later transactions pay.
    assert_eq!(resolved.block_fee_to_store, Some(311));
}

#[test]
fn no_hook_charges_the_tick_spacing_fee() {
    let hook =
        AeroHookFee { default_swap_hook: Bytes::from([0u8; 20]), ..configured(300, 5_000, 0) };

    assert_eq!(fee_of(&hook, 10).fee, TICK_SPACING_FEE);
}

#[test]
fn unknown_hooks_are_not_quoted() {
    let observations = observations();
    let mut hook = configured(300, 5_000, 0);
    hook.pool_swap_hook = Bytes::from(OTHER_HOOK);

    let err = hook
        .resolve(&oracle(&observations, 10), TICK_SPACING_FEE, execution(10))
        .unwrap_err();

    assert!(matches!(err, SimulationError::FatalError(_)), "{err:?}");
}

#[test]
fn pool_override_takes_precedence_over_the_default_hook() {
    let mut hook = configured(300, 5_000, 0);
    hook.default_swap_hook = Bytes::from(OTHER_HOOK);
    hook.pool_swap_hook = Bytes::from(DYNAMIC_SWAP_FEE_HOOK.as_slice());

    assert_eq!(fee_of(&hook, 10).fee, 300);
}

#[test]
fn decodes_and_updates_attributes() {
    let mut attributes: HashMap<String, Bytes> = [
        ("default_swap_hook", DYNAMIC_SWAP_FEE_HOOK.to_vec()),
        ("pool_swap_hook", vec![0u8; 20]),
        ("dfc_baseFee", vec![0x00, 0xfa]),
        ("dfc_feeCap", vec![0x15, 0x18]),
        ("dfc_scalingFactor", vec![0x05, 0xf5, 0xe1, 0x00]),
        ("dfc_initialFeeEnabled", vec![0x00]),
        ("dfc_initialFee", vec![0x00]),
        ("hook_default_scaling_factor", vec![0x00]),
        ("hook_default_fee_cap", vec![0x00, 0xc3, 0x50]),
        ("hook_seconds_ago", vec![0x02, 0x58]),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), Bytes::from(value)))
    .collect();

    let mut hook = AeroHookFee::from_attributes(&attributes).unwrap();
    hook.update_from_attributes(&HashMap::from([
        ("block_fee".to_string(), Bytes::from(vec![0x01, 0xc2])),
        ("block_fee_block".to_string(), Bytes::from(vec![0x03, 0x2a, 0x76, 0x47])),
    ]))
    .unwrap();

    assert_eq!((hook.base_fee, hook.fee_cap, hook.scaling_factor), (250, 5_400, 100_000_000));
    assert_eq!((hook.default_fee_cap, hook.seconds_ago), (50_000, 600));
    assert_eq!(hook.block_fee, Some((53_114_439, 450)));
    attributes.remove("hook_seconds_ago");
    assert!(AeroHookFee::from_attributes(&attributes).is_err());
}
