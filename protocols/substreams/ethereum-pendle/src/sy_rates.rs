//! The live PY index: `max(pyIndexStored, SY.exchangeRate())`.
//!
//! `pyIndexStored` is indexable from the yield token's `NewInterestIndex` event, but
//! `exchangeRate()` has no event stream at all — it moves with whatever protocol the SY wraps,
//! not with Pendle activity. So it is read, once per refresh block, for every SY behind a live
//! market, in one batched `eth_call`.
//!
//! That read also fires for every historical block during a backfill, which is the expensive
//! part: Pendle's first Ethereum market dates to 2023. `sy_rate_refresh_blocks` exists to make
//! the backfill interval a config change rather than a code change.
//!
//! Two clocks are published alongside it, and they are not the same clock. `rate_sampled_at`
//! dates the rate: it is the block the rate was read at, and it stops advancing the moment a read
//! stops resolving. `block_timestamp` dates the *look*: it advances on every refresh block
//! regardless. A consumer holding both can tell a rate that is still current from one the chain
//! has moved past, which one clock alone cannot express.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use substreams::scalar::BigInt;

/// Attribute holding `max(pyIndexStored, SY.exchangeRate())` as of this block.
pub const PY_INDEX_CURRENT: &str = "py_index_current";
/// Attribute holding an SY's `exchangeRate()` as of this block.
pub const SY_EXCHANGE_RATE: &str = "sy_exchange_rate";
/// Attribute holding the block timestamp the rate above was read at.
///
/// Emitted only when the read resolved. A consumer pairs a rate with the curve at the same
/// moment, so a rate that did not arrive must not be dated as though it had.
pub const RATE_SAMPLED_AT: &str = "rate_sampled_at";
/// Attribute holding the timestamp of the block this component was last looked at.
///
/// Emitted on every refresh block whether or not the rate resolved. It is what tells a consumer
/// the chain has moved on from the rate it holds: without it a failed read is indistinguishable
/// from a quiet market, and stale state would go on being quoted as current.
pub const BLOCK_TIMESTAMP: &str = "block_timestamp";

/// How often the SY exchange rates are re-read.
#[derive(Debug, Clone, Deserialize)]
pub struct RefreshParams {
    /// Refresh every `n`-th block. `1` reads every block; a larger value trades index freshness
    /// for backfill cost.
    pub sy_rate_refresh_blocks: u64,
}

impl RefreshParams {
    pub fn parse(input: &str) -> Result<Self> {
        let params: Self = serde_qs::from_str(input)
            .map_err(|e| anyhow!("failed to parse params {input:?}: {e}"))?;
        if params.sy_rate_refresh_blocks == 0 {
            return Err(anyhow!(
                "sy_rate_refresh_blocks must be at least 1; 0 would disable the PY index refresh \
                 entirely, which is never what a caller means"
            ));
        }
        Ok(params)
    }

    /// Whether this block is one the SY rates are read on.
    // `u64::is_multiple_of` reads better and is what a modern clippy asks for, but it stabilised
    // in 1.87 and this package builds on the 1.83 pin in `rust-toolchain.toml`. `parse` rejects a
    // zero interval, so the remainder cannot divide by zero.
    #[allow(clippy::manual_is_multiple_of)]
    pub fn should_refresh(&self, block_number: u64) -> bool {
        block_number % self.sy_rate_refresh_blocks == 0
    }
}

/// The index the market's math will actually run on this block.
///
/// `PendleYieldToken._pyIndexCurrent()` returns `max(_pyIndexStored, SY.exchangeRate())`: the
/// stored index is a monotonic floor that only ever ratchets up to meet the rate, so it wins
/// exactly when the wrapped protocol's rate has fallen since the last interaction.
///
/// A market whose stored index is not yet known — no `NewInterestIndex` since indexing began and
/// no creation-time seed — falls back to the rate alone, which is the correct answer unless the
/// rate has dropped.
pub fn py_index_current(stored: Option<BigInt>, rate: &BigInt) -> BigInt {
    match stored {
        Some(stored) if stored > *rate => stored,
        _ => rate.clone(),
    }
}

/// Encodes a block timestamp as the fixed-width big-endian value the simulation decodes.
///
/// The curve depends on `block.timestamp` through `rateScalar`, `rateAnchor` and `feeRate`, so a
/// quote is only valid for the timestamp it was computed for. Fixed at 8 bytes rather than
/// minimally encoded: a decoder reading it as a `u64` must not have to guess the width.
pub fn encode_timestamp(seconds: u64) -> Vec<u8> {
    seconds.to_be_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_refresh_interval() {
        let params = RefreshParams::parse("sy_rate_refresh_blocks=50").unwrap();
        assert_eq!(params.sy_rate_refresh_blocks, 50);
    }

    /// Zero would silently freeze the index at whatever the last refresh left behind, so it is
    /// rejected rather than reinterpreted.
    #[test]
    fn a_zero_interval_is_rejected() {
        let error = RefreshParams::parse("sy_rate_refresh_blocks=0").unwrap_err();
        assert!(error.to_string().contains("at least 1"), "{error}");
    }

    #[test]
    fn a_missing_interval_is_rejected() {
        assert!(RefreshParams::parse("").is_err());
    }

    #[test]
    fn an_interval_of_one_refreshes_every_block() {
        let params = RefreshParams::parse("sy_rate_refresh_blocks=1").unwrap();
        assert!(params.should_refresh(25_800_000));
        assert!(params.should_refresh(25_800_001));
    }

    #[test]
    fn a_coarse_interval_refreshes_on_multiples_only() {
        let params = RefreshParams::parse("sy_rate_refresh_blocks=50").unwrap();
        assert!(params.should_refresh(25_800_000));
        assert!(!params.should_refresh(25_800_001));
        assert!(params.should_refresh(25_800_050));
    }

    /// The wstETH market's own numbers: the brief quotes `pyIndexStored` *below* the live rate,
    /// so the rate is what the contract uses.
    #[test]
    fn the_rate_wins_while_it_is_climbing() {
        let stored = BigInt::from(1_241_811_000_000_000_000_i64);
        let rate = BigInt::from(1_241_884_000_000_000_000_i64);
        assert_eq!(py_index_current(Some(stored), &rate), rate);
    }

    /// The stored index is a ratchet. If the wrapped protocol's rate falls, the contract keeps
    /// quoting off the high-water mark, and so must we.
    #[test]
    fn the_stored_index_wins_once_the_rate_falls() {
        let stored = BigInt::from(1_241_884_000_000_000_000_i64);
        let rate = BigInt::from(1_241_811_000_000_000_000_i64);
        assert_eq!(py_index_current(Some(stored.clone()), &rate), stored);
    }

    #[test]
    fn an_unknown_stored_index_falls_back_to_the_rate() {
        let rate = BigInt::from(1_095_830_i64);
        assert_eq!(py_index_current(None, &rate), rate);
    }

    /// Fixed width, including for a timestamp whose leading bytes are zero — a decoder that has
    /// to guess the width is a decoder that eventually guesses wrong.
    #[test]
    fn the_block_timestamp_is_always_eight_bytes() {
        assert_eq!(encode_timestamp(1830124800).len(), 8);
        assert_eq!(encode_timestamp(0), vec![0; 8]);
        assert_eq!(
            encode_timestamp(1669201235),
            vec![0x00, 0x00, 0x00, 0x00, 0x63, 0x7d, 0xfd, 0x53]
        );
    }
}
