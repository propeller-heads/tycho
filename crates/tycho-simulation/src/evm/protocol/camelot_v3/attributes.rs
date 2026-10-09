//! Names and encodings of the state attributes the `arbitrum-camelot-v3` substreams package
//! emits, shared by the snapshot decoder and `delta_transition`.
//!
//! Values are big-endian and fixed-width at the source. A value may still arrive wider than
//! its field, padded with sign bytes, which every parser here accepts; anything else is an
//! error rather than a silent truncation.

use alloy::primitives::U256;

pub(super) const LIQUIDITY: &str = "liquidity";
pub(super) const SQRT_PRICE_X96: &str = "sqrt_price_x96";
pub(super) const TICK: &str = "tick";
pub(super) const FEE_ZTO: &str = "fee_zto";
pub(super) const FEE_OTZ: &str = "fee_otz";
pub(super) const TIMEPOINT_INDEX: &str = "timepoint_index";
pub(super) const VOLUME_PER_LIQUIDITY_IN_BLOCK: &str = "volume_per_liquidity_in_block";
pub(super) const FEE_CONFIG_ZTO: &str = "fee_config_zto";
pub(super) const FEE_CONFIG_OTZ: &str = "fee_config_otz";
/// `ticks/{tick}`: the tick's `liquidityDelta`, present while any position references it.
pub(super) const TICKS_PREFIX: &str = "ticks/";
/// `timepoints/{index}`: the two storage words of the operator's `timepoints[index]`.
pub(super) const TIMEPOINTS_PREFIX: &str = "timepoints/";

/// Drops surplus leading bytes above `width`, which must all be the padding byte `pad`.
fn narrow<'a>(name: &str, bytes: &'a [u8], width: usize, pad: u8) -> Result<&'a [u8], String> {
    if bytes.len() <= width {
        return Ok(bytes);
    }
    let (surplus, value) = bytes.split_at(bytes.len() - width);
    if surplus.iter().all(|b| *b == pad) {
        Ok(value)
    } else {
        Err(format!("{name}: {} bytes do not fit in {width}", bytes.len()))
    }
}

fn unsigned(name: &str, bytes: &[u8], width: usize) -> Result<U256, String> {
    Ok(U256::from_be_slice(narrow(name, bytes, width, 0)?))
}

pub(super) fn u16_attr(name: &str, bytes: &[u8]) -> Result<u16, String> {
    Ok(unsigned(name, bytes, 2)?.to::<u16>())
}

pub(super) fn u128_attr(name: &str, bytes: &[u8]) -> Result<u128, String> {
    Ok(unsigned(name, bytes, 16)?.to::<u128>())
}

/// A `uint160` such as the pool's sqrt price.
pub(super) fn u160_attr(name: &str, bytes: &[u8]) -> Result<U256, String> {
    unsigned(name, bytes, 20)
}

fn sign_bit(bytes: &[u8]) -> bool {
    bytes
        .first()
        .is_some_and(|b| b & 0x80 != 0)
}

/// Sign-extends a big-endian two's-complement value of at most 16 bytes.
pub(super) fn sign_extend(bytes: &[u8]) -> i128 {
    let mut buf = if sign_bit(bytes) { [0xffu8; 16] } else { [0u8; 16] };
    buf[16 - bytes.len()..].copy_from_slice(bytes);
    i128::from_be_bytes(buf)
}

/// A signed value of `width` bytes, with the sign bytes it was padded with. Padding that
/// disagrees with the sign of the bytes it wraps means the value does not fit `width`.
fn signed(name: &str, bytes: &[u8], width: usize) -> Result<i128, String> {
    let negative = sign_bit(bytes);
    let value = narrow(name, bytes, width, if negative { 0xff } else { 0 })?;
    if sign_bit(value) != negative {
        return Err(format!("{name}: {} bytes do not fit in {width}", bytes.len()));
    }
    Ok(sign_extend(value))
}

/// An `int24` such as the pool tick, with the sign byte it was padded with.
pub(super) fn i24_attr(name: &str, bytes: &[u8]) -> Result<i32, String> {
    Ok(signed(name, bytes, 3)? as i32)
}

/// An `int128` such as a tick's `liquidityDelta`.
pub(super) fn i128_attr(name: &str, bytes: &[u8]) -> Result<i128, String> {
    signed(name, bytes, 16)
}

/// The tick of a `ticks/{tick}` key, or `None` for any other key.
pub(super) fn tick_of_key(key: &str) -> Option<Result<i32, String>> {
    let index = key.strip_prefix(TICKS_PREFIX)?;
    Some(
        index
            .parse::<i32>()
            .map_err(|err| format!("{key}: {err}")),
    )
}

/// The ring index of a `timepoints/{index}` key, or `None` for any other key.
pub(super) fn timepoint_of_key(key: &str) -> Option<Result<u16, String>> {
    let index = key.strip_prefix(TIMEPOINTS_PREFIX)?;
    Some(
        index
            .parse::<u16>()
            .map_err(|err| format!("{key}: {err}")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_values_padded_beyond_their_width() {
        assert_eq!(u16_attr("fee", &[0, 0, 0, 0x01, 0x2c]).unwrap(), 300);
        assert_eq!(u128_attr("liquidity", &[0u8; 32]).unwrap(), 0);
        assert_eq!(i24_attr("tick", &[0xff, 0xff, 0xff, 0xfc, 0xfc, 0xdf]).unwrap(), -197_409);
        assert_eq!(i128_attr("ticks/1", &[0xff; 20]).unwrap(), -1);
    }

    #[test]
    fn rejects_values_that_do_not_fit() {
        assert!(u16_attr("fee", &[1, 0, 0]).is_err());
        assert!(i24_attr("tick", &[0x7f, 0xff, 0xff, 0xff]).is_err());
        assert!(u160_attr("sqrt_price_x96", &[1u8; 21]).is_err());
        // Padding that disagrees with the sign of the value it wraps: +2^23 and -2^24 + 1.
        assert!(i24_attr("tick", &[0x00, 0x80, 0x00, 0x00]).is_err());
        assert!(i24_attr("tick", &[0xff, 0x00, 0x00, 0x01]).is_err());
        let mut positive = [0u8; 17];
        positive[1] = 0x80;
        assert!(i128_attr("ticks/1", &positive).is_err());
    }

    #[test]
    fn an_empty_value_is_zero() {
        assert_eq!(i24_attr("tick", &[]).unwrap(), 0);
        assert_eq!(i128_attr("ticks/1", &[]).unwrap(), 0);
        assert_eq!(u128_attr("liquidity", &[]).unwrap(), 0);
    }

    #[test]
    fn decodes_narrow_values() {
        assert_eq!(i24_attr("tick", &[0xfe, 0xaf, 0xc6]).unwrap(), -86_074);
        assert_eq!(i128_attr("ticks/1", &[0x01, 0x00]).unwrap(), 256);
        assert_eq!(u160_attr("sqrt_price_x96", &[0x01]).unwrap(), U256::from(1));
    }

    #[test]
    fn parses_prefixed_keys() {
        assert_eq!(
            tick_of_key("ticks/-120")
                .unwrap()
                .unwrap(),
            -120
        );
        assert_eq!(
            timepoint_of_key("timepoints/48648")
                .unwrap()
                .unwrap(),
            48_648
        );
        assert!(tick_of_key("liquidity").is_none());
        assert!(timepoint_of_key("timepoints/70000")
            .unwrap()
            .is_err());
    }
}
