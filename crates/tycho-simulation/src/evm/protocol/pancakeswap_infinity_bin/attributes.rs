//! Decoding for the attributes the `pancakeswap_infinity_bin` substreams package emits.
//!
//! Shared by `state::delta_transition` and `decoder`; [`AttributeError`] converts into the error
//! type each of them returns.
//!
//! Every helper checks the length first. The `Bytes` conversions right-align into a fixed buffer
//! and panic on anything longer, and these values come from the indexer.

use tycho_common::{simulation::errors::TransitionError, Bytes};

use crate::protocol::errors::InvalidSnapshotError;

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
pub fn decode_u32(name: &str, value: &Bytes) -> Result<u32, AttributeError> {
    if value.len() > 4 {
        return Err(AttributeError(format!(
            "{name}: expected at most 4 bytes, got {}",
            value.len()
        )));
    }
    Ok(u32::from(value.clone()))
}

/// Decodes a bin id, bounded to the u24 the chain uses. See [`parse_bin_id`].
pub fn decode_bin_id(name: &str, value: &Bytes) -> Result<u32, AttributeError> {
    let id = decode_u32(name, value)?;
    if id >= 1 << 24 {
        return Err(AttributeError(format!("{name}: bin id is wider than the u24 on chain")));
    }

    Ok(id)
}

/// Decodes an unsigned value of at most 2 bytes, big-endian. Protocol fees are 12-bit halves.
pub fn decode_u16(name: &str, value: &Bytes) -> Result<u16, AttributeError> {
    if value.len() > 2 {
        return Err(AttributeError(format!(
            "{name}: expected at most 2 bytes, got {}",
            value.len()
        )));
    }
    Ok(u16::from(value.clone()))
}

/// Splits a raw `reserveOfBin` word into `(reserve_x, reserve_y)`.
///
/// `PackedUint128Math` keeps x in the low 128 bits, so big-endian puts **y first**. Reading them
/// the other way round decodes cleanly and silently swaps the pool's two balances.
pub fn decode_reserves(name: &str, value: &Bytes) -> Result<(u128, u128), AttributeError> {
    let word: [u8; 32] = value
        .as_ref()
        .try_into()
        .map_err(|_| AttributeError(format!("{name}: expected 32 bytes, got {}", value.len())))?;
    let y = u128::from_be_bytes(word[..16].try_into().expect("16 bytes"));
    let x = u128::from_be_bytes(word[16..].try_into().expect("16 bytes"));

    Ok((x, y))
}

/// Parses the bin id out of a `bins/{id}` attribute key.
///
/// Bounded to a u24, as on chain: `get_price_from_id` subtracts `2^23` into an `i32` and the swap
/// walk takes `id + 1`, so a wider id would overflow rather than price a bin that cannot exist.
pub fn parse_bin_id(key: &str) -> Result<u32, AttributeError> {
    let id: u32 = key
        .strip_prefix("bins/")
        .ok_or_else(|| AttributeError(format!("{key}: not a bins/ attribute")))?
        .parse()
        .map_err(|_| AttributeError(format!("{key}: bin id is not a u32")))?;
    if id >= 1 << 24 {
        return Err(AttributeError(format!("{key}: bin id is wider than the u24 on chain")));
    }

    Ok(id)
}

/// Packs reserves the way the indexer emits them, the inverse of [`decode_reserves`]. Test-only.
#[cfg(test)]
pub fn reserve_word(x: u128, y: u128) -> Bytes {
    let mut word = [0u8; 32];
    word[..16].copy_from_slice(&y.to_be_bytes());
    word[16..].copy_from_slice(&x.to_be_bytes());

    Bytes::from(word.to_vec())
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
        assert_eq!(decode_u32("active_id", &Bytes::from(bytes)).unwrap(), expected);
    }

    #[test]
    fn test_decode_u32_rejects_oversized_value() {
        assert!(
            decode_u32("active_id", &Bytes::from(vec![0u8; 5])).is_err(),
            "5 bytes would panic the right-aligning conversion, so it must error first"
        );
    }

    /// Y occupies the first 16 bytes, x the last.
    #[test]
    fn test_decode_reserves_reads_y_first() {
        let mut word = [0u8; 32];
        word[15] = 7;
        word[31] = 3;

        assert_eq!(decode_reserves("bins/1", &Bytes::from(word.to_vec())).unwrap(), (3, 7));
    }

    #[test]
    fn test_decode_reserves_rejects_short_word() {
        assert!(
            decode_reserves("bins/1", &Bytes::from(vec![0u8; 16])).is_err(),
            "half a reserve word must not decode as one"
        );
    }

    /// Ids wider than the chain's u24 would overflow `get_price_from_id`'s exponent and the swap
    /// walk's `id + 1`.
    #[rstest]
    #[case::active_id("active_id", vec![0x01, 0x00, 0x00, 0x00])]
    #[case::bin_word("bins/1", vec![0xff, 0xff, 0xff, 0xff])]
    fn test_decode_bin_id_rejects_ids_above_u24(#[case] name: &str, #[case] bytes: Vec<u8>) {
        assert!(
            decode_bin_id(name, &Bytes::from(bytes)).is_err(),
            "{name}: a bin id cannot be wider than u24"
        );
    }

    #[test]
    fn test_parse_bin_id() {
        assert_eq!(parse_bin_id("bins/8388608").unwrap(), 8_388_608);
        assert!(parse_bin_id("bins/-1").is_err(), "bin ids are unsigned");
        assert!(parse_bin_id("fee").is_err(), "only bins/ keys carry a bin id");
        assert!(
            parse_bin_id(&format!("bins/{}", 1u32 << 24)).is_err(),
            "a bin id cannot be wider than u24"
        );
    }
}
