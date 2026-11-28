//! LP fee library for Uniswap V4
//!
//! This module provides constants and utilities for handling LP fees in Uniswap V4 pools,
//! including special flag bits for dynamic fees and fee overrides.
//!
//! See: <https://github.com/Uniswap/v4-core/blob/main/src/libraries/LPFeeLibrary.sol>

/// Maximum LP fee in pips (1,000,000 = 100%)
pub const MAX_LP_FEE: u32 = 1_000_000;

/// Dynamic fee flag (bit 23: 0x800000)
/// When set in pool key, indicates the pool uses dynamic fees via hooks
pub const DYNAMIC_FEE_FLAG: u32 = 0x800000;

/// Override flag (bit 22: 0x400000) a hook sets on the fee `beforeSwap` returns to make the pool
/// charge that fee for the swap instead of its stored LP fee (`LPFeeLibrary.OVERRIDE_FEE_FLAG`).
pub const OVERRIDE_FEE_FLAG: u32 = 0x400000;

/// Mask to remove the override flag (bit 22: 0x400000)
const REMOVE_OVERRIDE_MASK: u32 = 0xBFFFFF;

/// Removes the override flag from a fee value
///
/// The second bit (bit 22) of the fee returned by beforeSwap is used to signal
/// if the stored LP fee should be overridden in this swap. This function clears
/// that bit to get the actual fee value.
#[inline]
pub fn remove_override_flag(fee: u32) -> u32 {
    fee & REMOVE_OVERRIDE_MASK
}

/// Checks if a fee value represents a dynamic fee pool
///
/// Returns `true` if the fee equals DYNAMIC_FEE_FLAG (0x800000)
#[inline]
pub fn is_dynamic(fee: u32) -> bool {
    fee == DYNAMIC_FEE_FLAG
}

/// Validates that a fee doesn't exceed MAX_LP_FEE
///
/// Returns `true` if the fee is valid (≤ 1,000,000 pips / 100%)
#[inline]
pub fn is_valid(fee: u32) -> bool {
    fee <= MAX_LP_FEE
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    // Tests inspired by https://github.com/Uniswap/v4-core/blob/main/test/libraries/LPFeeLibrary.t.sol

    #[rstest]
    #[case::with_override_flag(3000u32 | OVERRIDE_FEE_FLAG, 3000u32)]
    #[case::without_flag(3000u32, 3000u32)]
    #[case::with_dynamic_flag(DYNAMIC_FEE_FLAG, DYNAMIC_FEE_FLAG)]
    #[case::real_world_override(4_197_304, 3000)] // 3000 + 0x400000
    fn test_remove_override_flag(#[case] input: u32, #[case] expected: u32) {
        assert_eq!(remove_override_flag(input), expected);
    }

    #[rstest]
    #[case::zero_fee(0, true)]
    #[case::mid_range_fee(500_000, true)] // 50%
    #[case::max_fee(MAX_LP_FEE, true)] // 100%
    #[case::exceeds_max_by_one(MAX_LP_FEE + 1, false)]
    #[case::exceeds_max_significantly(2_000_000, false)]
    fn test_is_valid(#[case] fee: u32, #[case] expected: bool) {
        assert_eq!(is_valid(fee), expected);
    }

    #[test]
    fn test_constants_match_uniswap() {
        // Verify our constants match Uniswap V4's values
        assert_eq!(MAX_LP_FEE, 1_000_000);
        assert_eq!(REMOVE_OVERRIDE_MASK, 0xBFFFFF);
        // Verify bit positions
        assert_eq!(OVERRIDE_FEE_FLAG, 1u32 << 22);
        assert_eq!(DYNAMIC_FEE_FLAG, 1u32 << 23);
    }

    #[test]
    fn test_combined_flags() {
        // Test fee with both override and dynamic flags
        let fee_with_both_flags = OVERRIDE_FEE_FLAG | DYNAMIC_FEE_FLAG;
        let cleaned = remove_override_flag(fee_with_both_flags);
        // Should only remove override flag, leaving dynamic flag
        assert_eq!(cleaned, DYNAMIC_FEE_FLAG);
    }
}
