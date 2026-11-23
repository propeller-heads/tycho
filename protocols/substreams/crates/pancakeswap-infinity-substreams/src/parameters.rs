//! The parts of `PoolKey.parameters`, the pool manager storage layout and the fee encoding that
//! CL and Bin pools share.
//!
//! `parameters` is a bytes32 whose low 16 bits are the hook registration bitmap; the bits above
//! hold the pool type's own setting (`int24 tickSpacing` for CL, `uint16 binStep` for Bin), which
//! each package reads itself.
//! [ParametersHelper](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/libraries/math/ParametersHelper.sol#L12-L29).

use tiny_keccak::{Hasher, Keccak};

/// Hook permission bit offsets, not v4's address bits where `beforeSwap` is 7 and `afterSwap` 6.
/// The four swap offsets are the same in
/// [ICLHooks](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/interfaces/ICLHooks.sol#L11-L24)
/// and [IBinHooks](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-bin/interfaces/IBinHooks.sol#L10-L23);
/// the liquidity ones differ in name only.
pub const HOOKS_BEFORE_SWAP_OFFSET: u8 = 6;
pub const HOOKS_AFTER_SWAP_OFFSET: u8 = 7;
pub const HOOKS_BEFORE_SWAP_RETURNS_DELTA_OFFSET: u8 = 10;
pub const HOOKS_AFTER_SWAP_RETURNS_DELTA_OFFSET: u8 = 11;

/// `DYNAMIC_FEE_FLAG`: the pool's LP fee comes from its hook.
/// [LPFeeLibrary](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/libraries/LPFeeLibrary.sol#L19).
pub const DYNAMIC_FEE_FLAG: u32 = 0x800000;

/// Low 16 bits of `parameters`.
pub fn hooks_registration_bitmap(parameters: &[u8; 32]) -> u16 {
    u16::from_be_bytes([parameters[30], parameters[31]])
}

/// True if any hook callback that can change swap amounts or fees is registered:
/// beforeSwap, afterSwap, or either returns-delta variant.
pub fn has_swap_hooks(parameters: &[u8; 32]) -> bool {
    let mask = (1u16 << HOOKS_BEFORE_SWAP_OFFSET) |
        (1u16 << HOOKS_AFTER_SWAP_OFFSET) |
        (1u16 << HOOKS_BEFORE_SWAP_RETURNS_DELTA_OFFSET) |
        (1u16 << HOOKS_AFTER_SWAP_RETURNS_DELTA_OFFSET);
    hooks_registration_bitmap(parameters) & mask != 0
}

pub fn is_dynamic_fee(fee: u32) -> bool {
    fee == DYNAMIC_FEE_FLAG
}

/// `keccak256(pool_id ++ uint256(mapping_slot))`: the base slot of `pools[pool_id]`, which holds
/// the pool's slot0. `mapping_slot` is the pool manager's `pools` slot, which each package pins.
pub fn pool_state_base_slot(pool_id: &[u8; 32], mapping_slot: u64) -> [u8; 32] {
    let mut hasher = Keccak::v256();
    hasher.update(pool_id);
    let mut slot = [0u8; 32];
    slot[24..].copy_from_slice(&mapping_slot.to_be_bytes());
    hasher.update(&slot);
    let mut out = [0u8; 32];
    hasher.finalize(&mut out);
    out
}

/// `(zero_for_one, one_for_zero)` from a packed uint24
/// [ProtocolFeeLibrary](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/libraries/ProtocolFeeLibrary.sol#L22-L31).
pub fn split_protocol_fee(protocol_fee: u32) -> (u32, u32) {
    (protocol_fee & 0xfff, (protocol_fee >> 12) & 0xfff)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// `parameters` with a hook bitmap in bits [0, 16) and `above` from bit 16 up, where a tick
    /// spacing or a bin step sits.
    fn pool_key_parameters(hook_bitmap: u16, above: u32) -> [u8; 32] {
        let value = ((above as u128) << 16) | hook_bitmap as u128;
        let mut out = [0u8; 32];
        out[16..].copy_from_slice(&value.to_be_bytes());
        out
    }

    fn hook_bits(bits: &[u8]) -> [u8; 32] {
        let bitmap = bits
            .iter()
            .fold(0u16, |acc, bit| acc | (1u16 << bit));
        pool_key_parameters(bitmap, 0)
    }

    #[test]
    fn hooks_bitmap_reads_low_16_bits() {
        assert_eq!(hooks_registration_bitmap(&pool_key_parameters(0xc0, 60)), 0xc0);

        let above_only = pool_key_parameters(0, 0xff_ffff);
        assert_eq!(hooks_registration_bitmap(&above_only), 0);
        assert!(!has_swap_hooks(&above_only), "bits above 16 leaked into the hook bitmap");
    }

    /// Any of the four swap callbacks puts the pool out of scope.
    #[rstest]
    #[case::before_swap(HOOKS_BEFORE_SWAP_OFFSET)]
    #[case::after_swap(HOOKS_AFTER_SWAP_OFFSET)]
    #[case::before_swap_returns_delta(HOOKS_BEFORE_SWAP_RETURNS_DELTA_OFFSET)]
    #[case::after_swap_returns_delta(HOOKS_AFTER_SWAP_RETURNS_DELTA_OFFSET)]
    fn swap_callbacks_are_swap_hooks(#[case] bit: u8) {
        assert!(has_swap_hooks(&hook_bits(&[bit])), "bit {bit} is a swap hook");
    }

    /// Other callbacks leave swap amounts and fees alone. Treating them as hooked would drop
    /// pools the simulation can price. CL calls bits 2 to 5 add/remove liquidity, Bin mint/burn.
    #[rstest]
    #[case::before_initialize(0)]
    #[case::after_initialize(1)]
    #[case::before_add_liquidity(2)]
    #[case::after_add_liquidity(3)]
    #[case::before_remove_liquidity(4)]
    #[case::after_remove_liquidity(5)]
    #[case::before_donate(8)]
    #[case::after_donate(9)]
    #[case::after_add_liquidity_returns_delta(12)]
    #[case::after_remove_liquidity_returns_delta(13)]
    fn other_callbacks_are_not_swap_hooks(#[case] bit: u8) {
        assert!(
            !has_swap_hooks(&hook_bits(&[bit])),
            "bit {bit} is not a swap hook, so the pool stays in scope"
        );
    }

    #[test]
    fn swap_hooks_among_other_bits() {
        assert!(!has_swap_hooks(&[0u8; 32]), "a hookless pool has no swap hooks");
        assert!(!has_swap_hooks(&hook_bits(&[0, 2, 3, 8])), "non-swap bits alone stay in scope");
        assert!(
            has_swap_hooks(&hook_bits(&[0, 2, 3, 8, HOOKS_AFTER_SWAP_OFFSET])),
            "one swap bit among non-swap bits still counts"
        );
    }

    #[test]
    fn dynamic_fee_flag() {
        assert!(is_dynamic_fee(DYNAMIC_FEE_FLAG), "the flag itself is dynamic");
        assert!(!is_dynamic_fee(3000), "a plain LP fee is static");
        assert!(!is_dynamic_fee(0), "a zero fee is static");
        assert!(!is_dynamic_fee(DYNAMIC_FEE_FLAG - 1), "one below the flag is still static");
    }

    #[test]
    fn pool_base_slot_matches_reference_keccak() {
        // keccak256(0x11 * 32 ++ uint256(4)), computed with an independent keccak implementation.
        assert_eq!(
            hex::encode(pool_state_base_slot(&[0x11; 32], 4)),
            "3408821a6ee18ded1824589351edb99fa12abb821b004c34389e3ac2e3b22506"
        );
    }

    #[test]
    fn split_protocol_fee_halves_are_independent() {
        // zero-for-one in the low 12 bits, one-for-zero in the high 12.
        assert_eq!(split_protocol_fee(0x00_1005), (5, 1));
        assert_eq!(split_protocol_fee((300 << 12) | 200), (200, 300));
        assert_eq!(split_protocol_fee(0), (0, 0));
    }
}
