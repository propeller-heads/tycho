use tycho_common::simulation::protocol_sim::Price;

pub(crate) mod dynamic_fee_module;
pub(crate) mod observations;

const BAND_SCALE: u64 = 1_000_000_000_000;
const FEE_PRECISION: u64 = 1_000_000;
/// Leaves headroom for the `U256` math in `clmm_swap_to_price`.
const MAX_PRICE_BITS: u64 = 200;

/// Returns the raw pool price at which a spot price with a `fee_pips` markup reads the middle of
/// the band `[target, target * (1 + tolerance)]`, or `None` when the price is too wide.
pub(crate) fn raw_target_price(target: &Price, tolerance: f64, fee_pips: u32) -> Option<Price> {
    // The band middle keeps sqrt price and f64 rounding above the target. The cast saturates.
    let bump = (tolerance / 2.0 * BAND_SCALE as f64) as u64;
    let fee_complement = FEE_PRECISION.checked_sub(fee_pips.into())?;
    let numerator = &target.numerator * BAND_SCALE.saturating_add(bump) * fee_complement;
    let denominator = &target.denominator * BAND_SCALE * FEE_PRECISION;
    (numerator.bits() <= MAX_PRICE_BITS && denominator.bits() <= MAX_PRICE_BITS)
        .then(|| Price::new(numerator, denominator))
}
