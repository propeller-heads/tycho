//! `GoldFeeSource.segmentAt`, ported.
//!
//! Globex hours in Chicago time: Sunday 17:00 to Friday 16:00, with a daily break from 16:00 to
//! 17:00. The contract does not model CME holiday halts, so neither does this.

use crate::evm::protocol::algebra_integral::plugin::modules::nami::{
    fee_source::{spike, Segment},
    us_market_time::to_local,
};

const CLOSED: u8 = 0;
const OFF_HOURS: u8 = 1;
const ACTIVE: u8 = 2;
const BREAK: u8 = 3;
const REOPEN_SPIKE: u8 = 4;
const REOPEN_SPIKE_AFTER_WEEKEND: u8 = 5;

const HOURS_WEST: u64 = 6;
const HOUR: u64 = 3_600;
const ACTIVE_START: u64 = 2 * HOUR;
const ACTIVE_END: u64 = 12 * HOUR + 1_800;
const BREAK_START: u64 = 16 * HOUR;
const BREAK_END: u64 = 17 * HOUR;
const SPIKE_DURATION: u64 = 1_800;

const SUNDAY: u64 = 0;
const FRIDAY: u64 = 5;
const SATURDAY: u64 = 6;

pub(super) fn segment_at(timestamp: u64) -> Option<Segment> {
    let local = to_local(timestamp, HOURS_WEST)?;
    let weekday = local.weekday;
    let second = local.second_of_day;

    // The weekend runs from Friday 16:00 to Sunday 17:00.
    if weekday == SATURDAY || (weekday == FRIDAY && second >= BREAK_START) {
        return Some(Segment::steady(CLOSED));
    }
    if weekday == SUNDAY {
        if second < BREAK_END {
            return Some(Segment::steady(CLOSED));
        }
        return Some(reopening(REOPEN_SPIKE_AFTER_WEEKEND, second - BREAK_END));
    }
    if (BREAK_START..BREAK_END).contains(&second) {
        return Some(Segment::steady(BREAK));
    }
    if second >= BREAK_END {
        return Some(reopening(REOPEN_SPIKE, second - BREAK_END));
    }
    if (ACTIVE_START..ACTIVE_END).contains(&second) {
        return Some(Segment::steady(ACTIVE));
    }
    Some(Segment::steady(OFF_HOURS))
}

fn reopening(spike_id: u8, since_reopen: u64) -> Segment {
    spike(spike_id, OFF_HOURS, since_reopen, SPIKE_DURATION)
}
