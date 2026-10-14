//! Decoding of `PoolKey.parameters` and storage-slot arithmetic for PancakeSwap Infinity.
//!
//! Layout, bit offsets from the least significant bit of the bytes32:
//! [ParametersHelper](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/libraries/math/ParametersHelper.sol#L12-L29),
//! [CLPoolParametersHelper](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/libraries/CLPoolParametersHelper.sol#L19-L32).
//!
//! ```text
//! [0, 16)   hooks registration bitmap (bit index = the ICLHooks offsets below)
//! [16, 40)  tickSpacing, int24
//! [40, 256) must be zero
//! ```

use tiny_keccak::{Hasher, Keccak};

/// Hook permission bit offsets, not v4's address bits where `beforeSwap` is 7 and `afterSwap` 6.
/// [ICLHooks](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/interfaces/ICLHooks.sol#L11-L24).
pub const HOOKS_BEFORE_SWAP_OFFSET: u8 = 6;
pub const HOOKS_AFTER_SWAP_OFFSET: u8 = 7;
pub const HOOKS_BEFORE_SWAP_RETURNS_DELTA_OFFSET: u8 = 10;
pub const HOOKS_AFTER_SWAP_RETURNS_DELTA_OFFSET: u8 = 11;

/// `DYNAMIC_FEE_FLAG`: the pool's LP fee comes from its hook.
/// [LPFeeLibrary](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/libraries/LPFeeLibrary.sol#L19).
pub const DYNAMIC_FEE_FLAG: u32 = 0x800000;

/// Storage slot of `CLPoolManager.pools`; slots 0-3 are Ownable/Pausable/ProtocolFees.
///
/// A wrong slot returns plausible numbers rather than an error, so the value is pinned to a live
/// read in the tests below, not to this comment.
/// [CLPoolManager](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/CLPoolManager.sol#L39).
pub const CL_POOLS_MAPPING_SLOT: u64 = 4;

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

/// `int24` tick spacing from bits [16, 40). Chain enforces `1..=i16::MAX`, so the sign extension
/// is for completeness. [TickMath](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/libraries/TickMath.sol#L22-L24).
pub fn tick_spacing(parameters: &[u8; 32]) -> i32 {
    i32::from_be_bytes([parameters[27], parameters[28], parameters[29], 0]) >> 8
}

pub fn is_dynamic_fee(fee: u32) -> bool {
    fee == DYNAMIC_FEE_FLAG
}

/// `keccak256(pool_id ++ uint256(mapping_slot))`: the slot of `pools[pool_id].slot0`, packed as
/// `[0,160) sqrtPriceX96 | [160,184) tick | [184,208) protocolFee | [208,232) lpFee`.
/// [CLSlot0](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/types/CLSlot0.sol#L8-L9).
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

/// Protocol fee (uint24) out of a raw CL slot0 word.
pub fn protocol_fee_from_slot0(slot0: &[u8; 32]) -> u32 {
    // bits [184, 208) from the LSB = bytes 6..9 from the MSB, where byte 8 holds bits 184..191.
    let word = u32::from_be_bytes([0, slot0[6], slot0[7], slot0[8]]);
    word & 0xff_ffff
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// `PoolKey.parameters` with a hook bitmap in bits [0, 16) and a tick spacing in [16, 40).
    pub fn pool_key_parameters(hook_bitmap: u16, tick_spacing: u32) -> [u8; 32] {
        let value = ((tick_spacing as u128) << 16) | hook_bitmap as u128;
        let mut out = [0u8; 32];
        out[16..].copy_from_slice(&value.to_be_bytes());
        out
    }

    /// A CL slot0 word holding only the packed protocol fee at bits [184, 208).
    pub fn slot0_with_protocol_fee(zero_for_one: u32, one_for_zero: u32) -> [u8; 32] {
        let packed = (one_for_zero << 12) | zero_for_one;
        let mut slot0 = [0u8; 32];
        slot0[6..9].copy_from_slice(&packed.to_be_bytes()[1..]);
        slot0
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::{fixtures::*, *};

    /// The hook bitmap is all ones in every case, so a spacing that reads a neighbouring bit
    /// fails.
    #[rstest]
    #[case::min(1, 1)]
    #[case::typical(60, 60)]
    #[case::max(i16::MAX as u32, i16::MAX as i32)]
    // `int24(uint24)` in `getTickSpacing`: the top bit is the sign. No live pool can set it.
    #[case::sign_extends(0xff_ffff, -1)]
    fn tick_spacing_reads_bits_16_to_40(#[case] raw: u32, #[case] expected: i32) {
        assert_eq!(tick_spacing(&pool_key_parameters(u16::MAX, raw)), expected);
    }

    #[test]
    fn hooks_bitmap_reads_low_16_bits() {
        assert_eq!(hooks_registration_bitmap(&pool_key_parameters(0xc0, 60)), 0xc0);

        let spacing_only = pool_key_parameters(0, 0xff_ffff);
        assert_eq!(hooks_registration_bitmap(&spacing_only), 0);
        assert!(!has_swap_hooks(&spacing_only), "tick spacing leaked into the hook bitmap");
    }

    /// Any of the four swap callbacks puts the pool out of scope.
    #[rstest]
    #[case::before_swap(HOOKS_BEFORE_SWAP_OFFSET)]
    #[case::after_swap(HOOKS_AFTER_SWAP_OFFSET)]
    #[case::before_swap_returns_delta(HOOKS_BEFORE_SWAP_RETURNS_DELTA_OFFSET)]
    #[case::after_swap_returns_delta(HOOKS_AFTER_SWAP_RETURNS_DELTA_OFFSET)]
    fn swap_callbacks_are_swap_hooks(#[case] bit: u8) {
        assert!(has_swap_hooks(&pool_key_parameters(1 << bit, 60)), "bit {bit} is a swap hook");
    }

    /// Other callbacks leave swap amounts and fees alone. Treating them as hooked would drop
    /// pools `UniswapV4State` can simulate.
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
            !has_swap_hooks(&pool_key_parameters(1 << bit, 60)),
            "bit {bit} is not a swap hook, so the pool stays in scope"
        );
    }

    #[test]
    fn dynamic_fee_flag() {
        assert!(is_dynamic_fee(DYNAMIC_FEE_FLAG), "the flag itself is dynamic");
        assert!(!is_dynamic_fee(3000), "a plain LP fee is static");
        assert!(!is_dynamic_fee(DYNAMIC_FEE_FLAG - 1), "one below the flag is still static");
    }

    #[test]
    fn pool_base_slot_matches_reference_keccak() {
        // keccak256(0x11 * 32 ++ uint256(4)), computed with an independent keccak implementation.
        let expected =
            hex::decode("3408821a6ee18ded1824589351edb99fa12abb821b004c34389e3ac2e3b22506")
                .unwrap();
        assert_eq!(pool_state_base_slot(&[0x11; 32], CL_POOLS_MAPPING_SLOT).to_vec(), expected);
    }

    /// The USDC/USDT pool the integration test indexes, read off Base. Pins the slot constant:
    /// slot 3 and 5 decode to junk, only 4 yields the pool's real lpFee and a price near 1.
    #[test]
    fn pool_base_slot_matches_live_pool() {
        let pool_id: [u8; 32] =
            hex::decode("9ed2b133457a9debb64997f932b15a0a81f61718e8a267a4b36fab7b960d788a")
                .unwrap()
                .try_into()
                .unwrap();
        assert_eq!(
            hex::encode(pool_state_base_slot(&pool_id, CL_POOLS_MAPPING_SLOT)),
            "70f756cf3c9ac7fa14e5e8a3107f7535afcfdd01b74ff8fac0e93ec343d37bc8"
        );

        // The word that slot held.
        let slot0: [u8; 32] =
            hex::decode("00000000000500200200000a00000000000000010021133c4df3a45cdd3057a3")
                .unwrap()
                .try_into()
                .unwrap();
        // lpFee 5 matches the component's key_lp_fee in integration_test.tycho.yaml.
        assert_eq!(u32::from_be_bytes([0, slot0[3], slot0[4], slot0[5]]) & 0xff_ffff, 5);
        assert_eq!(split_protocol_fee(protocol_fee_from_slot0(&slot0)), (2, 2));
    }

    #[test]
    fn splits_and_reads_protocol_fee() {
        let slot0 = slot0_with_protocol_fee(200, 300);
        assert_eq!(protocol_fee_from_slot0(&slot0), (300 << 12) | 200);
        assert_eq!(split_protocol_fee(protocol_fee_from_slot0(&slot0)), (200, 300));
    }
}
