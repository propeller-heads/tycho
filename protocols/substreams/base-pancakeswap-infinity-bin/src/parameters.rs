//! Decoding of `PoolKey.parameters` and storage-slot arithmetic for PancakeSwap Infinity Bin.
//!
//! Sibling of `../../base-pancakeswap-infinity-cl/src/parameters.rs`. Same hook bitmap; bits
//! [16, 32) hold a `uint16` bin step instead of an `int24` tick spacing, and the pools mapping
//! slot differs.
//!
//! Layout (`src/libraries/math/ParametersHelper.sol:12-29`,
//! `src/pool-bin/libraries/BinPoolParametersHelper.sol`), bits counted from the least significant
//! bit of the bytes32:
//!
//! ```text
//! [0, 16)   hooks registration bitmap (bit index = IBinHooks.sol offsets)
//! [16, 32)  binStep, uint16
//! [32, 256) must be zero
//! ```

use tiny_keccak::{Hasher, Keccak};

/// Hook permission bit offsets (`src/pool-bin/interfaces/IBinHooks.sol:8-21`). Not v4's address
/// bits, where `beforeSwap` is 7 and `afterSwap` 6. These four match `ICLHooks.sol` but the enums
/// differ elsewhere, so read them from IBinHooks.
pub const HOOKS_BEFORE_SWAP_OFFSET: u8 = 6;
pub const HOOKS_AFTER_SWAP_OFFSET: u8 = 7;
pub const HOOKS_BEFORE_SWAP_RETURNS_DELTA_OFFSET: u8 = 10;
pub const HOOKS_AFTER_SWAP_RETURNS_DELTA_OFFSET: u8 = 11;

/// `LPFeeLibrary.DYNAMIC_FEE_FLAG` (`src/libraries/LPFeeLibrary.sol:19`): the pool's LP fee comes
/// from its hook.
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

/// Storage slot of `BinPoolManager.pools`. CL is 4; Bin adds `minBinShareForDonate` on top of the
/// Ownable/Pausable/ProtocolFees state, so it is 5.
///
/// A wrong slot returns plausible numbers, never an error. The chain-pinned tests below are what
/// catches it.
pub const BIN_POOLS_MAPPING_SLOT: u8 = 5;

/// `uint16` bin step from bits [16, 32): price ratio between adjacent bins in basis points, so
/// `price(id) = (1 + bin_step / 10_000) ^ (id - 2^23)`. Where CL has a 24-bit signed tick spacing,
/// so two bytes and no sign extension.
///
/// Byte `i` holds bits `[8*(31-i), 8*(31-i)+8)`, putting [16, 32) in bytes 28 and 29.
pub fn bin_step(parameters: &[u8; 32]) -> u16 {
    u16::from_be_bytes([parameters[28], parameters[29]])
}

/// `keccak256(pool_id ++ uint256(mapping_slot))`: the slot of `pools[pool_id].slot0`.
///
/// Struct layout from there (`src/pool-bin/libraries/BinPool.sol:54-68`): `+0` slot0,
/// `+1` reserveOfBin, `+2` shareOfBin, `+3` positions, `+4` the bin tree. slot0 packs
/// `[0,24) activeId | [24,48) protocolFee | [48,72) lpFee` (`BinSlot0.sol:9`).
pub fn pool_state_base_slot(pool_id: &[u8; 32], mapping_slot: u8) -> [u8; 32] {
    let mut hasher = Keccak::v256();
    hasher.update(pool_id);
    // Slot index goes in the last byte. CL writes `slot[24..]` because its parameter is u64;
    // with a u8 that panics on a length mismatch.
    let mut slot = [0u8; 32];
    slot[31] = mapping_slot;
    hasher.update(&slot);
    let mut out = [0u8; 32];
    hasher.finalize(&mut out);
    out
}

/// Slot of `mapping[key]` for the mapping stored `offset` slots after `base`.
///
/// `base` is a hash, so the offset can carry out of the low byte. Solidity stores `mapping[k]` at
/// `keccak256(pad32(k) ++ pad32(slot))`:
/// <https://docs.soliditylang.org/en/latest/internals/layout_in_storage.html#mappings-and-dynamic-arrays>
fn mapping_slot(base: &[u8; 32], offset: u8, key: u32) -> [u8; 32] {
    let mut slot = *base;
    let mut carry = offset;
    for byte in slot.iter_mut().rev() {
        let (next, overflowed) = byte.overflowing_add(carry);
        *byte = next;
        if !overflowed {
            break;
        }
        carry = 1;
    }
    let mut padded = [0u8; 32];
    padded[28..].copy_from_slice(&key.to_be_bytes());

    let mut hasher = Keccak::v256();
    hasher.update(&padded);
    hasher.update(&slot);
    let mut out = [0u8; 32];
    hasher.finalize(&mut out);
    out
}

/// Slot holding `reserveOfBin[bin_id]`, the mapping at `base + 1`.
pub fn bin_reserve_slot(base: &[u8; 32], bin_id: u32) -> [u8; 32] {
    mapping_slot(base, 1, bin_id)
}

/// Slot holding `level2[segment]`, the bin tree leaf word at `base + 6`. `segment` is
/// `bin_id >> 8`; bit `bin_id & 0xff` of the word says whether the bin is in the tree.
pub fn tree_level2_slot(base: &[u8; 32], segment: u32) -> [u8; 32] {
    mapping_slot(base, 6, segment)
}

/// Splits a packed `reserveOfBin` word into `(reserve_x, reserve_y)`.
///
/// `PackedUint128Math` puts x in bits [0,128) and y in [128,256) (`decodeX` is
/// `and(z, MASK_128)`, `decodeY` is `shr(128, z)`). Big endian, so y is the FIRST 16 bytes and x
/// the LAST: reversed from reading order, and swapping them swaps the two token balances.
pub fn unpack_reserves(packed: &[u8; 32]) -> (u128, u128) {
    let mut y = [0u8; 16];
    let mut x = [0u8; 16];
    y.copy_from_slice(&packed[..16]);
    x.copy_from_slice(&packed[16..]);
    (u128::from_be_bytes(x), u128::from_be_bytes(y))
}

/// `(zero_for_one, one_for_zero)` from a packed uint24
/// (`src/libraries/ProtocolFeeLibrary.sol:22-31`).
pub fn split_protocol_fee(protocol_fee: u32) -> (u32, u32) {
    (protocol_fee & 0xfff, (protocol_fee >> 12) & 0xfff)
}

pub fn protocol_fee_from_slot0(slot0: &[u8; 32]) -> u32 {
    // bits [24, 48) from the LSB = bytes 26..29 (BinSlot0.sol:9). CL's [184, 208) is bytes 6..9.
    u32::from_be_bytes([0, slot0[26], slot0[27], slot0[28]]) & 0xff_ffff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parameters_with_bin_step(bin_step: u16) -> [u8; 32] {
        let mut parameters = [0u8; 32];
        parameters[28..30].copy_from_slice(&bin_step.to_be_bytes());
        parameters
    }

    #[rstest::rstest]
    #[case::one(1)]
    #[case::twenty_five(25)]
    #[case::hundred(100)]
    #[case::max(u16::MAX)]
    fn test_bin_step_round_trip(#[case] step: u16) {
        assert_eq!(bin_step(&parameters_with_bin_step(step)), step);
    }

    /// Bin step 25 in bits [16, 32) ends the word 0x00190000.
    #[test]
    fn test_bin_step_byte_position() {
        let parameters = parameters_with_bin_step(25);
        assert_eq!(hex::encode(&parameters[24..]), "0000000000190000");
        assert_eq!(bin_step(&parameters), 25);
    }

    /// Hook bitmap sits below the bin step and must not leak into it.
    #[test]
    fn test_bin_step_ignores_hook_bitmap() {
        let mut parameters = parameters_with_bin_step(25);
        parameters[30..32].copy_from_slice(&u16::MAX.to_be_bytes());
        assert_eq!(bin_step(&parameters), 25);
    }

    // Real BNB pool. These constants come off chain, so the slot arithmetic and the reserve
    // packing are both pinned to values the contract actually produced.
    const POOL_ID: &str = "a859b22e97f32d4c7b1d9788044697712cb7183d294ed7aa1832799c1739e5cf";
    const BASE_SLOT: &str = "b075359a75fd55c0c6a55183f5e3bfe94a1f686b63f50d5f5d4d48f8147e5763";
    const ACTIVE_ID: u32 = 8_388_604;

    fn pool_id() -> [u8; 32] {
        hex::decode(POOL_ID)
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn base_slot() -> [u8; 32] {
        hex::decode(BASE_SLOT)
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn word(hex_str: &str) -> [u8; 32] {
        hex::decode(hex_str)
            .unwrap()
            .try_into()
            .unwrap()
    }

    /// Pool struct base. This slot holds the pool's BinSlot0.
    #[test]
    fn test_pool_state_base_slot_matches_chain() {
        assert_eq!(
            hex::encode(pool_state_base_slot(&pool_id(), BIN_POOLS_MAPPING_SLOT)),
            BASE_SLOT
        );
    }

    /// reserveOfBin slots either side of the active bin.
    #[rstest::rstest]
    #[case::below(ACTIVE_ID - 1, "fa6d19decff2d31eed3716dee26ae5367bddaa32bfe3978db24eecab5092dced")]
    #[case::active(ACTIVE_ID, "f7a087347bd2e8ef207ace8ffbc84e8a04e98f2b90fc502e2df5e5d49dcb93f1")]
    #[case::above(ACTIVE_ID + 1, "57f1ce7c1719cfe09156165c1e7de5293f9b9a2d22f5837ec9d15c7bf9ff0234")]
    fn test_bin_reserve_slot_matches_chain(#[case] bin_id: u32, #[case] expected: &str) {
        assert_eq!(hex::encode(bin_reserve_slot(&base_slot(), bin_id)), expected);
    }

    /// `level2[ACTIVE_ID >> 8]` for the same pool. At this slot BinPoolManager on BNB holds
    /// `0xfffffffe00..00`, bits 225..=255 set, and `ACTIVE_ID & 0xff` is 252.
    #[test]
    fn test_tree_level2_slot_matches_chain() {
        assert_eq!(
            hex::encode(tree_level2_slot(&base_slot(), ACTIVE_ID >> 8)),
            "bdbb39840772b2cdf39b8a51a1907990363daef629b456e80b44d55770e4e309"
        );
    }

    /// The offset is added once, then only a carry of one propagates.
    #[test]
    fn test_mapping_slot_carries_offset_once() {
        let mut base = [0u8; 32];
        base[31] = 0xfb;
        let mut expected = [0u8; 32];
        expected[31] = 0x01;
        expected[30] = 0x01;
        // Same hash input as a base that already sits at the carried value with offset 0.
        assert_eq!(mapping_slot(&base, 6, 7), mapping_slot(&expected, 0, 7));
    }

    /// Only Y below the active bin, only X above. Pins which half is which.
    #[test]
    fn test_unpack_reserves_orientation() {
        // Below active: Y only.
        let (x, y) = unpack_reserves(&word(
            "00000000000000127029f43511857cfb00000000000000000000000000000000",
        ));
        assert_eq!(x, 0, "bins below the active one hold no X");
        assert_eq!(y, 340_123_652_841_829_399_803);

        // Above active: X only.
        let (x, y) = unpack_reserves(&word(
            "00000000000000000000000000000000000000000000003383e6b4683281721f",
        ));
        assert_eq!(y, 0, "bins above the active one hold no Y");
        assert_eq!(x, 950_288_430_182_416_085_535);

        // Active bin: both.
        let (x, y) = unpack_reserves(&word(
            "0000000000000021d68696d9b4df1a7300000000000000004d10a11a3704cc10",
        ));
        assert_eq!(x, 5_553_115_474_512_104_464);
        assert_eq!(y, 624_200_763_065_197_599_347);
    }

    fn parameters_with_hook_bits(bits: &[u8]) -> [u8; 32] {
        let mut parameters = [0u8; 32];
        let bitmap = bits
            .iter()
            .fold(0u16, |acc, bit| acc | (1u16 << bit));
        parameters[30..32].copy_from_slice(&bitmap.to_be_bytes());
        parameters
    }

    /// Any of the four swap callbacks puts the pool out of scope.
    #[rstest::rstest]
    #[case::before_swap(HOOKS_BEFORE_SWAP_OFFSET)]
    #[case::after_swap(HOOKS_AFTER_SWAP_OFFSET)]
    #[case::before_swap_returns_delta(HOOKS_BEFORE_SWAP_RETURNS_DELTA_OFFSET)]
    #[case::after_swap_returns_delta(HOOKS_AFTER_SWAP_RETURNS_DELTA_OFFSET)]
    fn test_has_swap_hooks_true(#[case] bit: u8) {
        assert!(has_swap_hooks(&parameters_with_hook_bits(&[bit])), "bit {bit} is a swap hook");
    }

    /// Other callbacks leave swap amounts and fees alone. Treating them as hooked would drop
    /// pools we can simulate.
    #[rstest::rstest]
    #[case::before_initialize(0)]
    #[case::after_initialize(1)]
    #[case::before_mint(2)]
    #[case::after_mint(3)]
    #[case::before_burn(4)]
    #[case::after_burn(5)]
    #[case::before_donate(8)]
    #[case::after_donate(9)]
    #[case::after_mint_returns_delta(12)]
    #[case::after_burn_returns_delta(13)]
    fn test_has_swap_hooks_false(#[case] bit: u8) {
        assert!(
            !has_swap_hooks(&parameters_with_hook_bits(&[bit])),
            "bit {bit} is not a swap hook, so the pool stays in scope"
        );
    }

    #[test]
    fn test_has_swap_hooks_no_hooks_at_all() {
        assert!(!has_swap_hooks(&[0u8; 32]), "a hookless pool has no swap hooks");
    }

    /// One swap bit among non-swap bits still counts.
    #[test]
    fn test_has_swap_hooks_mixed_bits() {
        assert!(
            !has_swap_hooks(&parameters_with_hook_bits(&[0, 2, 3, 8])),
            "non-swap bits alone leave the pool in scope"
        );
        assert!(
            has_swap_hooks(&parameters_with_hook_bits(&[0, 2, 3, 8, HOOKS_AFTER_SWAP_OFFSET])),
            "one swap bit among non-swap bits still counts"
        );
    }

    /// Bin step sits directly above the bitmap. Off-by-a-byte would read it as hooks.
    #[test]
    fn test_has_swap_hooks_ignores_bin_step() {
        for step in [1u16, 25, 100, u16::MAX] {
            let parameters = parameters_with_bin_step(step);
            assert!(!has_swap_hooks(&parameters), "bin step {step} leaked into the hook bitmap");
            assert_eq!(hooks_registration_bitmap(&parameters), 0);
        }
    }

    #[test]
    fn test_is_dynamic_fee() {
        assert!(is_dynamic_fee(DYNAMIC_FEE_FLAG), "the flag itself is dynamic");
        assert!(!is_dynamic_fee(67), "the live pool's static LP fee");
        assert!(!is_dynamic_fee(0), "a zero fee is static");
        assert!(!is_dynamic_fee(DYNAMIC_FEE_FLAG - 1), "one below the flag is still static");
    }

    /// Real slot0 word: activeId 8388604, protocolFee 32/32, lpFee 67. CL reads protocolFee from
    /// bytes 6..9, which on a Bin word is zero.
    #[test]
    fn test_protocol_fee_from_slot0() {
        let slot0 = word("00000000000000000000000000000000000000000000000000430200207ffffc");
        let packed = protocol_fee_from_slot0(&slot0);
        assert_eq!(split_protocol_fee(packed), (32, 32));
        assert_eq!(
            u32::from_be_bytes([0, slot0[6], slot0[7], slot0[8]]),
            0,
            "CL offsets read zero"
        );
    }

    #[test]
    fn test_split_protocol_fee_halves_are_independent() {
        // zero-for-one in the low 12 bits, one-for-zero in the high 12.
        assert_eq!(split_protocol_fee(0x00_1005), (5, 1));
        assert_eq!(split_protocol_fee(0), (0, 0));
    }

    /// Carry out of the low byte must propagate.
    #[test]
    fn test_bin_reserve_slot_carries() {
        let mut base = [0u8; 32];
        base[31] = 0xff;
        base[30] = 0x01;
        // base + 1 is 0x...0200, not 0x...0100.
        let mut naive = base;
        naive[31] = naive[31].wrapping_add(1);
        assert_ne!(bin_reserve_slot(&base, 1), bin_reserve_slot(&naive, 1));
    }
}
