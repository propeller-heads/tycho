//! What the launchpad hook handlers on Elysium ([`super::montra`], [`super::ascend`]) share: the
//! decoders of the attribute formats the with-hooks indexer writes, the checks both hooks make on
//! a swap's input, and the price bound of a swap.

use std::collections::HashMap;

use alloy::primitives::{I256, U256};
use tycho_common::Bytes;

use crate::{
    evm::protocol::utils::uniswap::tick_math::{MAX_SQRT_RATIO, MIN_SQRT_RATIO},
    protocol::errors::InvalidSnapshotError,
};

/// The largest `int128`, which bounds the amount of a v4 `BalanceDelta`.
pub(super) const INT128_MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX >> 1, 0, 0]);

/// The exact input of a swap whose `amount_specified` is negative, in raw units. Returns the
/// reason the hook's contract or the pool manager refuses any other amount: zero
/// (`SwapAmountCannotBeZero`), an exact-output swap (positive), and an input above
/// `type(int128).max`, which overflows the swap delta.
pub(super) fn exact_input(amount_specified: I256) -> Result<U256, &'static str> {
    if amount_specified == I256::ZERO {
        return Err("a swap of zero reverts SwapAmountCannotBeZero");
    }
    if amount_specified > I256::ZERO {
        return Err("exact-output swaps are not quoted");
    }
    let amount_in = amount_specified.unsigned_abs();
    if amount_in > INT128_MAX {
        return Err("an input above type(int128).max overflows the swap delta");
    }
    Ok(amount_in)
}

/// `price` inside the range a swap's price limit allows: the executor and V4Quoter pass
/// `MIN_SQRT_PRICE + 1` or `MAX_SQRT_PRICE - 1`, which a walk to TickMath's own extreme stops at.
pub(super) fn swap_bound(price: U256, zero_for_one: bool) -> U256 {
    if zero_for_one {
        price.max(MIN_SQRT_RATIO + U256::from(1))
    } else {
        price.min(MAX_SQRT_RATIO - U256::from(1))
    }
}

/// Decodes the attribute `name` of `attributes` with `decode`, taking `base` when it is absent, or
/// failing with `MissingAttribute` when there is no `base` either.
pub(super) fn field<T>(
    attributes: &HashMap<String, Bytes>,
    name: &str,
    decode: impl FnOnce(&str, &Bytes) -> Result<T, InvalidSnapshotError>,
    base: Option<T>,
) -> Result<T, InvalidSnapshotError> {
    match (attributes.get(name), base) {
        (Some(value), _) => decode(name, value),
        (None, Some(base)) => Ok(base),
        (None, None) => Err(InvalidSnapshotError::MissingAttribute(name.to_string())),
    }
}

/// The `N` bytes of the attribute `name`, whose value must be exactly that wide; `what` names the
/// type the width stands for.
pub(super) fn fixed<const N: usize>(
    name: &str,
    value: &Bytes,
    what: &str,
) -> Result<[u8; N], InvalidSnapshotError> {
    <[u8; N]>::try_from(value.as_ref()).map_err(|_wrong_width| {
        InvalidSnapshotError::ValueError(format!(
            "{name} is {} bytes ({value}), not the {N}-byte {what}",
            value.len()
        ))
    })
}

/// An 8-byte big-endian `uint64`: the indexer's format for every attribute of 64 bits or fewer.
pub(super) fn u64_be(name: &str, value: &Bytes) -> Result<u64, InvalidSnapshotError> {
    Ok(u64::from_be_bytes(fixed(name, value, "big-endian u64")?))
}

/// An 8-byte big-endian value below 2^32: the indexer's format for a `uint32` or narrower.
pub(super) fn u32_be(name: &str, value: &Bytes) -> Result<u32, InvalidSnapshotError> {
    let wide = u64_be(name, value)?;
    u32::try_from(wide).map_err(|_overflow| {
        InvalidSnapshotError::ValueError(format!("{name} = {wide} does not fit a uint32"))
    })
}

/// A one-byte boolean, `0` or `1`.
pub(super) fn flag(name: &str, value: &Bytes) -> Result<bool, InvalidSnapshotError> {
    match value.as_ref() {
        [0] => Ok(false),
        [1] => Ok(true),
        _ => Err(InvalidSnapshotError::ValueError(format!(
            "{name} is {value}, not a one-byte 0 or 1"
        ))),
    }
}

/// A 4-byte big-endian two's-complement tick, with no bound on its range.
pub(super) fn tick_i32(name: &str, value: &Bytes) -> Result<i32, InvalidSnapshotError> {
    Ok(i32::from_be_bytes(fixed(name, value, "big-endian i32")?))
}
