use tycho_common::simulation::protocol_sim::Price;

pub(crate) mod aero_hook_fee;
pub(crate) mod dynamic_fee_module;
pub(crate) mod observations;

/// The low 32 bits of a block timestamp, as `uint32(block.timestamp)` in the Slipstream oracle and
/// fee modules.
pub(crate) fn oracle_timestamp(timestamp: u64) -> u32 {
    (timestamp & 0xFFFF_FFFF) as u32
}

const BAND_SCALE: u64 = 1_000_000_000_000;
const FEE_PRECISION: u64 = 1_000_000;
/// Leaves headroom for the `U256` math in `clmm_swap_to_price`.
const MAX_PRICE_BITS: u64 = 200;

/// Returns the raw pool price at which a spot price with a `fee_pips` markup reads the middle of
/// the band `[target, target * (1 + tolerance)]`, or `None` when the price is too wide.
pub(crate) fn raw_target_price(target: &Price, tolerance: f64, fee_pips: u32) -> Option<Price> {
    if !tolerance.is_finite() || tolerance < 0.0 {
        return None;
    }
    // The band middle keeps sqrt price and f64 rounding above the target.
    let scaled_bump = tolerance / 2.0 * BAND_SCALE as f64;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the value is finite and non-negative; the cast saturates at u64::MAX"
    )]
    let bump = scaled_bump as u64;
    let fee_complement = FEE_PRECISION.checked_sub(fee_pips.into())?;
    let numerator = &target.numerator * BAND_SCALE.saturating_add(bump) * fee_complement;
    let denominator = &target.denominator * BAND_SCALE * FEE_PRECISION;
    (numerator.bits() <= MAX_PRICE_BITS && denominator.bits() <= MAX_PRICE_BITS)
        .then(|| Price::new(numerator, denominator))
}
