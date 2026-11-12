//! Decoding of `PoolKey.parameters` and storage-slot arithmetic for PancakeSwap Infinity CL.
//!
//! The hook bitmap, the dynamic fee flag, the pool slot hash and the protocol fee split are
//! `pancakeswap_infinity_substreams::parameters`, re-exported here. CL's own are the `int24` tick
//! spacing, the `pools` mapping slot and the slot0 layout.
//!
//! Layout, bit offsets from the least significant bit of the bytes32:
//! [ParametersHelper](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/libraries/math/ParametersHelper.sol#L12-L29),
//! [CLPoolParametersHelper](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/libraries/CLPoolParametersHelper.sol#L19-L32).
//!
//! ```text
//! [0, 16)   hooks registration bitmap (bit index = the ICLHooks offsets)
//! [16, 40)  tickSpacing, int24
//! [40, 256) must be zero
//! ```

pub use pancakeswap_infinity_substreams::parameters::*;

/// Storage slot of `CLPoolManager.pools`; slots 0-3 are Ownable/Pausable/ProtocolFees.
///
/// A wrong slot returns plausible numbers rather than an error, so the value is pinned to a live
/// read in the tests below, not to this comment.
/// [CLPoolManager](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/CLPoolManager.sol#L39).
pub const CL_POOLS_MAPPING_SLOT: u64 = 4;

/// `int24` tick spacing from bits [16, 40). Chain enforces `1..=i16::MAX`, so the sign extension
/// is for completeness. [TickMath](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/libraries/TickMath.sol#L22-L24).
pub fn tick_spacing(parameters: &[u8; 32]) -> i32 {
    i32::from_be_bytes([parameters[27], parameters[28], parameters[29], 0]) >> 8
}

/// Protocol fee (uint24) out of a raw CL slot0 word, packed as
/// `[0,160) sqrtPriceX96 | [160,184) tick | [184,208) protocolFee | [208,232) lpFee`.
/// [CLSlot0](https://github.com/pancakeswap/infinity-core/blob/7c04695f/src/pool-cl/types/CLSlot0.sol#L8-L9).
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
