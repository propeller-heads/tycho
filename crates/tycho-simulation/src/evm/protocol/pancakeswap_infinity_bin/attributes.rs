//! Decoding for the state attributes the `base-pancakeswap-infinity-bin` substreams package
//! emits, as `8_map_protocol_changes.rs` encodes them:
//!
//! - `active_id`: `to_signed_bytes_be` of the u24 id, so 3 bytes below `0x800000` and 4 with a
//!   leading `0x00` at or above. Decoded length-tolerantly
//! - `active_in_tree`: one byte, `0` or `1`, emitted with every write to the active bin
//! - `protocol_fees/zero2one`, `protocol_fees/one2zero`: 12-bit, big-endian
//! - `bins/{id}/reserve_x`, `bins/{id}/reserve_y`: 16-byte big-endian `uint128`, one pair per bin
//!   with reserves a swap can reach. A bin at `(0, 0)` holds nothing and is dropped; a deletion of
//!   either key drops the bin
//!
//! Shared by `state::delta_transition` and `decoder`; [`AttributeError`] converts into the error
//! type each of them returns.
//!
//! Every helper checks the length first. The `Bytes` conversions right-align into a fixed buffer
//! and panic on anything longer, and these values come from the indexer.

use std::collections::BTreeMap;

use alloy::primitives::aliases::{U128, U16, U32};
use tycho_common::{simulation::errors::TransitionError, Bytes};

use crate::{
    evm::protocol::pancakeswap_infinity_bin::math::constants::BIN_ID_COUNT,
    protocol::errors::InvalidSnapshotError,
};

/// A malformed attribute value.
#[derive(Debug, PartialEq, Eq)]
pub struct AttributeError(String);

impl From<AttributeError> for TransitionError {
    fn from(error: AttributeError) -> Self {
        TransitionError::DecodeError(error.0)
    }
}

impl From<AttributeError> for InvalidSnapshotError {
    fn from(error: AttributeError) -> Self {
        InvalidSnapshotError::ValueError(error.0)
    }
}

/// Decodes an unsigned value of at most 4 bytes, big-endian.
///
/// `active_id` arrives as `BigInt::to_signed_bytes_be` of a u24, so 3 bytes below `0x800000` and
/// 4 with a leading `0x00` at or above it. Both decode to the same number here.
pub fn decode_u32_attribute(name: &str, value: &Bytes) -> Result<u32, AttributeError> {
    U32::try_from_be_slice(value)
        .map(|v| v.to::<u32>())
        .ok_or_else(|| too_wide(name, value, 4))
}

/// Decodes a bin id, bounded to the u24 the chain uses.
///
/// Not `U24::try_from_be_slice`: it refuses the 4-byte encoding of ids at or above `0x800000`,
/// which is where the indexed pools sit.
pub fn decode_bin_id_attribute(name: &str, value: &Bytes) -> Result<u32, AttributeError> {
    let id = decode_u32_attribute(name, value)?;
    if id >= BIN_ID_COUNT {
        return Err(AttributeError(format!("{name}: bin id is wider than the u24 on chain")));
    }

    Ok(id)
}

fn too_wide(name: &str, value: &Bytes, bytes: usize) -> AttributeError {
    AttributeError(format!("{name}: expected at most {bytes} bytes, got {}", value.len()))
}

/// Decodes an unsigned value of at most 2 bytes, big-endian. Protocol fees are 12-bit halves.
pub fn decode_u16_attribute(name: &str, value: &Bytes) -> Result<u16, AttributeError> {
    U16::try_from_be_slice(value)
        .map(|v| v.to::<u16>())
        .ok_or_else(|| too_wide(name, value, 2))
}

/// Decodes a one-byte flag, `0` or `1`.
pub fn decode_bool_attribute(name: &str, value: &Bytes) -> Result<bool, AttributeError> {
    match value.as_ref() {
        [0] => Ok(false),
        [1] => Ok(true),
        _ => Err(AttributeError(format!("{name}: expected a single 0 or 1 byte, got {value}"))),
    }
}

/// Decodes an unsigned value of at most 16 bytes, big-endian. Bin reserves are `uint128`.
pub fn decode_u128_attribute(name: &str, value: &Bytes) -> Result<u128, AttributeError> {
    U128::try_from_be_slice(value)
        .map(|v| v.to::<u128>())
        .ok_or_else(|| too_wide(name, value, 16))
}

/// The half of a bin's reserves a `bins/{id}/reserve_x` or `bins/{id}/reserve_y` key names.
/// `x` is `currency0`, `y` is `currency1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reserve {
    X,
    Y,
}

/// Parses a `bins/{id}/reserve_x` or `bins/{id}/reserve_y` key into the bin id and the side.
///
/// The id is bounded to a u24, as on chain: `get_price_from_id` subtracts `2^23` into an `i32`
/// and the swap walk takes `id + 1`, so a wider id would overflow rather than price a bin that
/// cannot exist.
pub fn parse_bin_key(key: &str) -> Result<(u32, Reserve), AttributeError> {
    let (id, side) = key
        .strip_prefix("bins/")
        .and_then(|rest| rest.split_once('/'))
        .ok_or_else(|| AttributeError(format!("{key}: not a bins/{{id}}/reserve_* attribute")))?;
    let id: u32 = id
        .parse()
        .map_err(|_| AttributeError(format!("{key}: bin id is not a u32")))?;
    if id >= BIN_ID_COUNT {
        return Err(AttributeError(format!("{key}: bin id is wider than the u24 on chain")));
    }
    let side = match side {
        "reserve_x" => Reserve::X,
        "reserve_y" => Reserve::Y,
        _ => return Err(AttributeError(format!("{key}: expected reserve_x or reserve_y"))),
    };

    Ok((id, side))
}

/// Applies one `bins/{id}/reserve_*` attribute to the bin map, creating the bin if needed. The
/// other half is untouched, so a bin whose halves arrive in separate deltas still ends up whole.
///
/// Leaves a bin at `(0, 0)` in place; dropping such bins is a separate step.
pub fn apply_reserve(
    bins: &mut BTreeMap<u32, (u128, u128)>,
    key: &str,
    value: &Bytes,
) -> Result<(), AttributeError> {
    let (id, side) = parse_bin_key(key)?;
    let amount = decode_u128_attribute(key, value)?;
    let bin = bins.entry(id).or_insert((0, 0));
    match side {
        Reserve::X => bin.0 = amount,
        Reserve::Y => bin.1 = amount,
    }

    Ok(())
}

/// A bin's two reserve attributes as the indexer emits them. Test-only.
#[cfg(test)]
pub fn reserve_attributes(id: u32, x: u128, y: u128) -> [(String, Bytes); 2] {
    [
        (format!("bins/{id}/reserve_x"), Bytes::from(x.to_be_bytes().to_vec())),
        (format!("bins/{id}/reserve_y"), Bytes::from(y.to_be_bytes().to_vec())),
    ]
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// Ids below and at the sign boundary, where the emitted length changes from 3 bytes to 4.
    #[rstest]
    #[case::three_bytes(vec![0x7f, 0xff, 0xff], 0x7fffff)]
    #[case::four_bytes_with_sign_padding(vec![0x00, 0x80, 0x00, 0x00], 0x800000)]
    fn test_decode_u32_tolerates_both_lengths(#[case] bytes: Vec<u8>, #[case] expected: u32) {
        assert_eq!(decode_u32_attribute("active_id", &Bytes::from(bytes)).unwrap(), expected);
    }

    #[test]
    fn test_decode_u32_rejects_oversized_value() {
        assert!(
            decode_u32_attribute("active_id", &Bytes::from(vec![0u8; 5])).is_err(),
            "5 bytes would panic the right-aligning conversion, so it must error first"
        );
    }

    #[rstest]
    #[case::zero(vec![0], Some(false))]
    #[case::one(vec![1], Some(true))]
    #[case::two(vec![2], None)]
    #[case::empty(vec![], None)]
    #[case::padded(vec![0, 1], None)]
    fn test_decode_bool(#[case] bytes: Vec<u8>, #[case] expected: Option<bool>) {
        assert_eq!(decode_bool_attribute("active_in_tree", &Bytes::from(bytes)).ok(), expected);
    }

    #[rstest]
    #[case::one_byte(vec![7], Some(7))]
    #[case::sixteen_bytes(u128::MAX.to_be_bytes().to_vec(), Some(u128::MAX))]
    #[case::seventeen_bytes(vec![0u8; 17], None)]
    fn test_decode_u128(#[case] bytes: Vec<u8>, #[case] expected: Option<u128>) {
        assert_eq!(decode_u128_attribute("bins/1/reserve_x", &Bytes::from(bytes)).ok(), expected);
    }

    #[rstest]
    #[case::x("bins/8388608/reserve_x", Some((8_388_608, Reserve::X)))]
    #[case::y("bins/7/reserve_y", Some((7, Reserve::Y)))]
    #[case::no_side("bins/7", None)]
    #[case::unknown_side("bins/7/reserve_z", None)]
    #[case::negative_id("bins/-1/reserve_x", None)]
    #[case::wider_than_u24("bins/16777216/reserve_x", None)]
    #[case::not_a_bin("fee", None)]
    fn test_parse_bin_key(#[case] key: &str, #[case] expected: Option<(u32, Reserve)>) {
        assert_eq!(parse_bin_key(key).ok(), expected);
    }

    /// Each half lands on its side and leaves the other alone.
    #[test]
    fn test_apply_reserve_sets_one_half() {
        let mut bins = BTreeMap::from([(7, (1, 2))]);

        apply_reserve(&mut bins, "bins/7/reserve_y", &Bytes::from(vec![9])).unwrap();
        apply_reserve(&mut bins, "bins/8/reserve_x", &Bytes::from(vec![3])).unwrap();

        assert_eq!(bins, BTreeMap::from([(7, (1, 9)), (8, (3, 0))]));
    }

    /// Ids wider than the chain's u24 would overflow `get_price_from_id`'s exponent and the swap
    /// walk's `id + 1`.
    #[rstest]
    #[case::just_above_u24("active_id", vec![0x01, 0x00, 0x00, 0x00])]
    #[case::max_u32("active_id", vec![0xff, 0xff, 0xff, 0xff])]
    fn test_decode_bin_id_rejects_ids_above_u24(#[case] name: &str, #[case] bytes: Vec<u8>) {
        assert!(
            decode_bin_id_attribute(name, &Bytes::from(bytes)).is_err(),
            "{name}: a bin id cannot be wider than u24"
        );
    }
}
