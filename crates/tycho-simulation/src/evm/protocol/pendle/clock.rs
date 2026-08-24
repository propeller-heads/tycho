//! Which block a Pendle quote belongs to, and when it stops being answerable.
//!
//! Pendle's curve moves with time and nothing else: `rateScalar`, `rateAnchor` and `feeRate` are
//! functions of `expiry - block.timestamp`. Those are closed forms, so they can be evaluated at
//! any timestamp exactly — the clock alone is never what limits a quote.
//!
//! `SY.exchangeRate()` is what limits it. It moves with whatever protocol the SY wraps, an
//! external accrual this package holds no model for, so it is valid for the block it was read at
//! and for no other. A quote is therefore only reproducible at [`PendleState`]'s own
//! `rate_sampled_at`, where the rate and the curve describe the same moment.
//!
//! Once the chain moves past that block the state still answers *a* question exactly — just not
//! the current one. Since only exact quotes are wanted, that is refused rather than served with
//! an extrapolated rate: the accrual is not linear for every SY, and `exchangeRate()` can fall,
//! so projecting it forward would replace a measurement with a guess.
//!
//! Contrast `ekubo_v3::pool::timed`, which extrapolates freely and never refuses. It can: TWAMM
//! virtual orders execute on a schedule the SDK reproduces exactly, so advancing its clock is
//! arithmetic rather than estimation. Pendle has no such model for the wrapped protocol.
//!
//! [`PendleState`]: super::state::PendleState

/// Whether a state sampled at `rate_sampled_at` can still answer for the chain head at `head`.
///
/// Equality, not a tolerance: the rate is a measurement taken at one block, and any other block
/// pairs it with a curve from a different moment.
///
/// `head` moving *behind* the sample is not staleness — a snapshot can carry a reading newer than
/// the header it was decoded against — so only a head strictly ahead of the sample is refused.
pub fn is_current(rate_sampled_at: u64, head: u64) -> bool {
    head <= rate_sampled_at
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_read_at_the_head_block_is_current() {
        assert!(is_current(1_800_000_000, 1_800_000_000));
    }

    /// One block is enough. The gap is small in seconds but the rate it pairs with is simply not
    /// the one the chain would use.
    #[test]
    fn a_head_past_the_sample_is_not_current() {
        assert!(!is_current(1_800_000_000, 1_800_000_012));
    }

    /// A snapshot can hold a reading newer than the header it decoded against, which is not a
    /// staleness failure and must not refuse the quote.
    #[test]
    fn a_sample_ahead_of_the_head_is_current() {
        assert!(is_current(1_800_000_012, 1_800_000_000));
    }
}
