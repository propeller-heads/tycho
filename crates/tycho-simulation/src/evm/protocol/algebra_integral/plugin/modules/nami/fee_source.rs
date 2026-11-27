//! Nami's fee sources: contracts that name the segment of a market's week a moment falls in.
//!
//! A plugin with a source enabled charges the fee its admin assigned to the current segment. The
//! sources are pure functions of the timestamp, so they are ported rather than indexed.

use tycho_common::simulation::errors::SimulationError;

use crate::evm::protocol::algebra_integral::plugin::modules::nami::{
    gold_fee_source, nyse_fee_source,
};

/// What a source's `segmentAt` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub id: u8,
    /// The segment the fee is moving toward; equal to `id` outside a transition.
    pub next_id: u8,
    /// How far the transition from `id` to `next_id` has gone, in basis points.
    pub blend_bps: u16,
}

impl Segment {
    pub(super) fn steady(id: u8) -> Self {
        Self { id, next_id: id, blend_bps: 0 }
    }
}

/// A fee source contract whose `segmentAt` the simulation reproduces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeSource {
    /// `NyseFeeSource`: the US stock week, New York time.
    Nyse,
    /// `GoldFeeSource`: the COMEX gold week, Chicago time.
    Gold,
}

impl FeeSource {
    /// The segment `timestamp` falls in.
    ///
    /// Fails for a timestamp of 2^40 seconds or more, and where the contract itself reverts:
    /// within the first days of 1970.
    pub fn segment_at(&self, timestamp: u64) -> Result<Segment, SimulationError> {
        let segment = match self {
            Self::Nyse => nyse_fee_source::segment_at(timestamp),
            Self::Gold => gold_fee_source::segment_at(timestamp),
        };
        segment.ok_or_else(|| {
            SimulationError::InvalidInput(
                format!("fee source {self:?} has no segment for timestamp {timestamp}"),
                None,
            )
        })
    }
}

/// A spike that decays linearly into `next` over `duration` seconds, stepping once a minute.
pub(super) fn spike(spike: u8, next: u8, since: u64, duration: u64) -> Segment {
    const STEP: u64 = 60;
    if since >= duration {
        return Segment::steady(next);
    }
    let elapsed = (since / STEP) * STEP;
    Segment { id: spike, next_id: next, blend_bps: bps(elapsed, duration) }
}

/// `part` of `whole` in basis points, for `part <= whole`.
pub(super) fn bps(part: u64, whole: u64) -> u16 {
    u16::try_from(part * 10_000 / whole).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// Rows of `timestamp,id,nextId,blendBps`, as the Solidity source answers `segmentAt`.
    fn reference_rows(csv: &str) -> Vec<(u64, Segment)> {
        let mut rows = Vec::new();
        for line in csv.lines() {
            let fields: Vec<&str> = line.split(',').collect();
            let [timestamp, id, next_id, blend_bps] = fields.as_slice() else {
                panic!("malformed reference row {line:?}");
            };
            let segment = Segment {
                id: id.parse().expect("id should be a u8"),
                next_id: next_id
                    .parse()
                    .expect("nextId should be a u8"),
                blend_bps: blend_bps
                    .parse()
                    .expect("blendBps should be a u16"),
            };
            rows.push((
                timestamp
                    .parse()
                    .expect("timestamp should be a u64"),
                segment,
            ));
        }
        rows
    }

    fn assert_matches_reference(source: FeeSource, csv: &str) {
        let rows = reference_rows(csv);
        assert!(!rows.is_empty(), "the reference should not be empty");
        for (timestamp, expected) in rows {
            let segment = source
                .segment_at(timestamp)
                .expect("a reference timestamp should have a segment");
            assert_eq!(segment, expected, "{source:?} at {timestamp}");
        }
    }

    // The references are the contracts' answers every 40,037 seconds from 1 January 2026 to
    // 1 January 2031, produced by running `segmentAt` of the Solidity sources in Foundry.
    #[rstest]
    #[case::nyse(FeeSource::Nyse, include_str!("testdata/nyse_segments.csv"))]
    #[case::gold(FeeSource::Gold, include_str!("testdata/gold_segments.csv"))]
    fn segments_match_the_contract(#[case] source: FeeSource, #[case] csv: &str) {
        assert_matches_reference(source, csv);
    }

    const fn steady(id: u8) -> Segment {
        Segment { id, next_id: id, blend_bps: 0 }
    }

    #[rstest]
    // Monday 6 July 2026, New York on daylight time (UTC-4).
    #[case::overnight(1_783_310_400, steady(1))]
    #[case::pre_market(1_783_324_800, steady(6))]
    // The day after the observed Independence Day and a weekend.
    #[case::open_after_closure(1_783_344_600, Segment { id: 4, next_id: 2, blend_bps: 0 })]
    #[case::open_spike_half_way(1_783_346_400, Segment { id: 4, next_id: 2, blend_bps: 5_000 })]
    #[case::session(1_783_348_200, steady(2))]
    #[case::close_ramp_starts(1_783_366_200, Segment { id: 2, next_id: 5, blend_bps: 0 })]
    #[case::close_ramp_last_minute(1_783_367_940, Segment { id: 2, next_id: 5, blend_bps: 9_666 })]
    #[case::after_hours(1_783_368_000, steady(6))]
    #[case::evening_after_a_full_day(1_783_382_400, steady(1))]
    // Tuesday 7 July 2026 opens after an ordinary trading day.
    #[case::open_after_trading_day(1_783_431_000, Segment { id: 3, next_id: 2, blend_bps: 0 })]
    // Friday 3 July 2026, Independence Day observed.
    #[case::holiday(1_783_087_200, steady(0))]
    // Sunday 5 July 2026 20:00, the market reopens ahead of Monday.
    #[case::reopen(1_783_296_000, Segment { id: 7, next_id: 1, blend_bps: 0 })]
    #[case::reopen_decaying(1_783_296_930, Segment { id: 7, next_id: 1, blend_bps: 2_500 })]
    // Friday 27 November 2026, the half day after Thanksgiving, on standard time (UTC-5).
    #[case::half_day_after_hours(1_795_802_400, steady(6))]
    #[case::half_day_shut_from_17(1_795_816_800, steady(0))]
    fn nyse_week(#[case] timestamp: u64, #[case] expected: Segment) {
        let segment = FeeSource::Nyse
            .segment_at(timestamp)
            .expect("a 2026 timestamp should have a segment");

        assert_eq!(segment, expected);
    }

    #[rstest]
    // Week of Monday 6 July 2026, Chicago on daylight time (UTC-5).
    #[case::off_hours(1_783_314_000, steady(1))]
    #[case::active(1_783_321_200, steady(2))]
    #[case::active_ends(1_783_359_000, steady(1))]
    #[case::daily_break(1_783_371_600, steady(3))]
    #[case::reopen(1_783_375_200, Segment { id: 4, next_id: 1, blend_bps: 0 })]
    #[case::reopen_decaying(1_783_375_830, Segment { id: 4, next_id: 1, blend_bps: 3_333 })]
    #[case::reopen_over(1_783_377_000, steady(1))]
    #[case::friday_close(1_783_717_200, steady(0))]
    #[case::saturday(1_783_771_200, steady(0))]
    #[case::sunday_before_reopen(1_783_893_599, steady(0))]
    #[case::sunday_reopen(1_783_893_600, Segment { id: 5, next_id: 1, blend_bps: 0 })]
    fn gold_week(#[case] timestamp: u64, #[case] expected: Segment) {
        let segment = FeeSource::Gold
            .segment_at(timestamp)
            .expect("a 2026 timestamp should have a segment");

        assert_eq!(segment, expected);
    }

    #[rstest]
    #[case::nyse(FeeSource::Nyse)]
    #[case::gold(FeeSource::Gold)]
    fn out_of_range_timestamp_is_refused(#[case] source: FeeSource) {
        let error = source
            .segment_at(1 << 40)
            .expect_err("a timestamp past the supported range should be refused");

        let SimulationError::InvalidInput(message, None) = error else {
            panic!("expected an invalid input, got {error:?}");
        };
        assert!(message.contains(&(1u64 << 40).to_string()), "{message}");
    }
}
