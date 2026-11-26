use alloy::primitives::{Address, I128, I256};
use tycho_common::simulation::errors::SimulationError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HookOptions {
    AfterRemoveLiquidityReturnsDelta = 0,
    AfterAddLiquidityReturnsDelta = 1,
    AfterSwapReturnsDelta = 2,
    BeforeSwapReturnsDelta = 3,
    AfterDonate = 4,
    BeforeDonate = 5,
    AfterSwap = 6,
    BeforeSwap = 7,
    AfterRemoveLiquidity = 8,
    BeforeRemoveLiquidity = 9,
    AfterAddLiquidity = 10,
    BeforeAddLiquidity = 11,
    AfterInitialize = 12,
    BeforeInitialize = 13,
}

// from https://github.com/shuhuiluo/uniswap-v4-sdk-rs/blob/main/src/utils/hook.rs#L69
pub const fn has_permission(address: Address, hook_option: HookOptions) -> bool {
    let mask = ((address.0 .0[18] as u64) << 8) | (address.0 .0[19] as u64);
    let hook_flag_index = hook_option as u64;
    mask & (1 << hook_flag_index) != 0
}

/// Extracts I128 from the upper 128 bits of a 256 value.
pub fn get_upper_i128(value: I256) -> I128 {
    // Arithmetic right shift by 128 bits to get the upper 128 bits
    let shifted: I256 = value >> 128;
    // Convert back to I128 by extracting the lower 128 bits
    let bytes = shifted.to_be_bytes::<32>();
    I128::from_be_bytes([
        bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
        bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31],
    ])
}

/// Extracts I128 from the lower 128 bits of a 256 value.
pub fn get_lower_i128(value: I256) -> I128 {
    // Get the lower 128 bits and sign extend
    let bytes = value.to_be_bytes::<32>();
    I128::from_be_bytes([
        bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
        bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31],
    ])
}

/// `value` as an `int128`, as v4-core's `SafeCast.toInt128` casts an amount a swap moves. Past
/// `int128` that cast reverts `SafeCastOverflow` and the swap with it, so this refuses with
/// `InvalidInput`, naming `what` the value is.
pub fn to_int128(value: I256, what: &str) -> Result<I128, SimulationError> {
    i128::try_from(value)
        .ok()
        .and_then(|value| I128::try_from(value).ok())
        .ok_or_else(|| {
            SimulationError::InvalidInput(
                format!("{what} {value} does not fit an int128: the pool manager reverts"),
                None,
            )
        })
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use alloy::primitives::U160;

    use super::*;

    fn construct_hook_address(hook_options: Vec<HookOptions>) -> Address {
        let mut hook_flags = U160::ZERO;
        let one = U160::from_limbs([1, 0, 0]);
        for hook_option in hook_options {
            hook_flags |= one << (hook_option as u8);
        }
        Address::from(hook_flags)
    }

    const EMPTY_HOOK_ADDRESS: Address = Address::ZERO;
    static HOOK_BEFORE_INITIALIZE: LazyLock<Address> =
        LazyLock::new(|| construct_hook_address(vec![HookOptions::BeforeInitialize]));
    static HOOK_AFTER_INITIALIZE: LazyLock<Address> =
        LazyLock::new(|| construct_hook_address(vec![HookOptions::AfterInitialize]));
    static HOOK_BEFORE_ADD_LIQUIDITY: LazyLock<Address> =
        LazyLock::new(|| construct_hook_address(vec![HookOptions::BeforeAddLiquidity]));
    static HOOK_AFTER_ADD_LIQUIDITY: LazyLock<Address> =
        LazyLock::new(|| construct_hook_address(vec![HookOptions::AfterAddLiquidity]));
    static HOOK_BEFORE_REMOVE_LIQUIDITY: LazyLock<Address> =
        LazyLock::new(|| construct_hook_address(vec![HookOptions::BeforeRemoveLiquidity]));
    static HOOK_AFTER_REMOVE_LIQUIDITY: LazyLock<Address> =
        LazyLock::new(|| construct_hook_address(vec![HookOptions::AfterRemoveLiquidity]));
    static HOOK_BEFORE_SWAP: LazyLock<Address> =
        LazyLock::new(|| construct_hook_address(vec![HookOptions::BeforeSwap]));
    static HOOK_AFTER_SWAP: LazyLock<Address> =
        LazyLock::new(|| construct_hook_address(vec![HookOptions::AfterSwap]));
    static HOOK_BEFORE_DONATE: LazyLock<Address> =
        LazyLock::new(|| construct_hook_address(vec![HookOptions::BeforeDonate]));
    static HOOK_AFTER_DONATE: LazyLock<Address> =
        LazyLock::new(|| construct_hook_address(vec![HookOptions::AfterDonate]));
    static HOOK_BEFORE_SWAP_RETURNS_DELTA: LazyLock<Address> =
        LazyLock::new(|| construct_hook_address(vec![HookOptions::BeforeSwapReturnsDelta]));
    static HOOK_AFTER_SWAP_RETURNS_DELTA: LazyLock<Address> =
        LazyLock::new(|| construct_hook_address(vec![HookOptions::AfterSwapReturnsDelta]));
    static HOOK_AFTER_ADD_LIQUIDITY_RETURNS_DELTA: LazyLock<Address> =
        LazyLock::new(|| construct_hook_address(vec![HookOptions::AfterAddLiquidityReturnsDelta]));
    static HOOK_AFTER_REMOVE_LIQUIDITY_RETURNS_DELTA: LazyLock<Address> = LazyLock::new(|| {
        construct_hook_address(vec![HookOptions::AfterRemoveLiquidityReturnsDelta])
    });

    mod has_permission {
        use super::*;

        #[test]
        fn before_initialize() {
            assert!(has_permission(*HOOK_BEFORE_INITIALIZE, HookOptions::BeforeInitialize));
            assert!(!has_permission(EMPTY_HOOK_ADDRESS, HookOptions::BeforeInitialize));
        }

        #[test]
        fn after_initialize() {
            assert!(has_permission(*HOOK_AFTER_INITIALIZE, HookOptions::AfterInitialize));
            assert!(!has_permission(EMPTY_HOOK_ADDRESS, HookOptions::AfterInitialize));
        }

        #[test]
        fn before_add_liquidity() {
            assert!(has_permission(*HOOK_BEFORE_ADD_LIQUIDITY, HookOptions::BeforeAddLiquidity));
            assert!(!has_permission(EMPTY_HOOK_ADDRESS, HookOptions::BeforeAddLiquidity));
        }

        #[test]
        fn after_add_liquidity() {
            assert!(has_permission(*HOOK_AFTER_ADD_LIQUIDITY, HookOptions::AfterAddLiquidity));
            assert!(!has_permission(EMPTY_HOOK_ADDRESS, HookOptions::AfterAddLiquidity));
        }

        #[test]
        fn before_remove_liquidity() {
            assert!(has_permission(
                *HOOK_BEFORE_REMOVE_LIQUIDITY,
                HookOptions::BeforeRemoveLiquidity
            ));
            assert!(!has_permission(EMPTY_HOOK_ADDRESS, HookOptions::BeforeRemoveLiquidity));
        }

        #[test]
        fn after_remove_liquidity() {
            assert!(has_permission(
                *HOOK_AFTER_REMOVE_LIQUIDITY,
                HookOptions::AfterRemoveLiquidity
            ));
            assert!(!has_permission(EMPTY_HOOK_ADDRESS, HookOptions::AfterRemoveLiquidity));
        }

        #[test]
        fn before_swap() {
            assert!(has_permission(*HOOK_BEFORE_SWAP, HookOptions::BeforeSwap));
            assert!(!has_permission(EMPTY_HOOK_ADDRESS, HookOptions::BeforeSwap));
        }

        #[test]
        fn after_swap() {
            assert!(has_permission(*HOOK_AFTER_SWAP, HookOptions::AfterSwap));
            assert!(!has_permission(EMPTY_HOOK_ADDRESS, HookOptions::AfterSwap));
        }

        #[test]
        fn before_donate() {
            assert!(has_permission(*HOOK_BEFORE_DONATE, HookOptions::BeforeDonate));
            assert!(!has_permission(EMPTY_HOOK_ADDRESS, HookOptions::BeforeDonate));
        }

        #[test]
        fn after_donate() {
            assert!(has_permission(*HOOK_AFTER_DONATE, HookOptions::AfterDonate));
            assert!(!has_permission(EMPTY_HOOK_ADDRESS, HookOptions::AfterDonate));
        }

        #[test]
        fn before_swap_returns_delta() {
            assert!(has_permission(
                *HOOK_BEFORE_SWAP_RETURNS_DELTA,
                HookOptions::BeforeSwapReturnsDelta
            ));
            assert!(!has_permission(EMPTY_HOOK_ADDRESS, HookOptions::BeforeSwapReturnsDelta));
        }

        #[test]
        fn after_swap_returns_delta() {
            assert!(has_permission(
                *HOOK_AFTER_SWAP_RETURNS_DELTA,
                HookOptions::AfterSwapReturnsDelta
            ));
            assert!(!has_permission(EMPTY_HOOK_ADDRESS, HookOptions::AfterSwapReturnsDelta));
        }

        #[test]
        fn after_add_liquidity_returns_delta() {
            assert!(has_permission(
                *HOOK_AFTER_ADD_LIQUIDITY_RETURNS_DELTA,
                HookOptions::AfterAddLiquidityReturnsDelta
            ));
            assert!(!has_permission(
                EMPTY_HOOK_ADDRESS,
                HookOptions::AfterAddLiquidityReturnsDelta
            ));
        }

        #[test]
        fn after_remove_liquidity_returns_delta() {
            assert!(has_permission(
                *HOOK_AFTER_REMOVE_LIQUIDITY_RETURNS_DELTA,
                HookOptions::AfterRemoveLiquidityReturnsDelta
            ));
            assert!(!has_permission(
                EMPTY_HOOK_ADDRESS,
                HookOptions::AfterRemoveLiquidityReturnsDelta
            ));
        }
    }
}
