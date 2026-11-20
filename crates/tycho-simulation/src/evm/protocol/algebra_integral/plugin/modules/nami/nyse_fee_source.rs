//! `NyseFeeSource.segmentAt`, ported.
//!
//! Days follow the NYSE Group calendar and evenings the overnight token market: a 20:00 to 24:00
//! session runs only ahead of a trading day, a full day trades 00:00 to 20:00 and a half day
//! 00:00 to 17:00, all New York time.

use crate::evm::protocol::algebra_integral::plugin::modules::nami::{
    fee_source::{bps, spike, Segment},
    us_market_time::{
        civil, day_number, day_of_week, last_weekday, nth_weekday, observed, to_local, Local,
    },
};

const CLOSED: u8 = 0;
const OVERNIGHT: u8 = 1;
const SESSION: u8 = 2;
const OPEN_SPIKE: u8 = 3;
const OPEN_SPIKE_AFTER_CLOSURE: u8 = 4;
const CLOSE_RAMP: u8 = 5;
const EXTENDED: u8 = 6;
const REOPEN: u8 = 7;

const HOURS_WEST: u64 = 5;
const HOUR: u64 = 3_600;
const MINUTE: u64 = 60;
const PRE_MARKET: u64 = 4 * HOUR;
const OPEN: u64 = 9 * HOUR + 30 * MINUTE;
const CLOSE: u64 = 16 * HOUR;
const EARLY_CLOSE: u64 = 13 * HOUR;
const HALF_DAY_END: u64 = 17 * HOUR;
const EVENING: u64 = 20 * HOUR;
const SPIKE_DURATION: u64 = HOUR;
const RAMP_DURATION: u64 = 30 * MINUTE;

const MONDAY: u64 = 1;
const THURSDAY: u64 = 4;

pub(super) fn segment_at(timestamp: u64) -> Option<Segment> {
    let local = to_local(timestamp, HOURS_WEST)?;
    let second = local.second_of_day;
    let trading = is_trading_day(local.weekday, local.year, local.month, local.day_of_month);
    let early = trading && is_early_close(local.year, local.month, local.day_of_month);

    if second >= EVENING {
        // The evening session belongs to the next day. After a full day it carries the after
        // hours on as overnight; after a weekend, a holiday or a half day it reopens a market
        // that was shut.
        if !trading_tomorrow(&local) {
            return Some(Segment::steady(CLOSED));
        }
        if trading && !early {
            return Some(Segment::steady(OVERNIGHT));
        }
        return Some(spike(REOPEN, OVERNIGHT, second - EVENING, SPIKE_DURATION));
    }
    if !trading {
        return Some(Segment::steady(CLOSED));
    }
    if second < PRE_MARKET {
        return Some(Segment::steady(OVERNIGHT));
    }
    if second < OPEN {
        return Some(Segment::steady(EXTENDED));
    }

    let close = if early { EARLY_CLOSE } else { CLOSE };
    if second >= close {
        let after_hours_end = if early { HALF_DAY_END } else { EVENING };
        if second < after_hours_end {
            return Some(Segment::steady(EXTENDED));
        }
        return Some(Segment::steady(CLOSED));
    }

    let since_open = second - OPEN;
    if since_open < SPIKE_DURATION {
        let opening = if closed_yesterday(&local)? { OPEN_SPIKE_AFTER_CLOSURE } else { OPEN_SPIKE };
        return Some(spike(opening, SESSION, since_open, SPIKE_DURATION));
    }
    let ramp_start = close - RAMP_DURATION;
    if second >= ramp_start {
        let into = ((second - ramp_start) / MINUTE) * MINUTE;
        return Some(Segment {
            id: SESSION,
            next_id: CLOSE_RAMP,
            blend_bps: bps(into, RAMP_DURATION),
        });
    }
    Some(Segment::steady(SESSION))
}

fn trading_tomorrow(local: &Local) -> bool {
    let (year, month, day) = civil(local.day + 1);
    is_trading_day((local.weekday + 1) % 7, year, month, day)
}

/// Whether the day before was not a trading day or ended early. `None` on the first day of the
/// epoch, where the contract reverts.
fn closed_yesterday(local: &Local) -> Option<bool> {
    let (year, month, day) = civil(local.day.checked_sub(1)?);
    let weekday = (local.weekday + 6) % 7;
    Some(!is_trading_day(weekday, year, month, day) || is_early_close(year, month, day))
}

fn is_trading_day(weekday: u64, year: u64, month: u64, day: u64) -> bool {
    if weekday == 0 || weekday == 6 {
        return false;
    }
    !is_holiday(year, month, day)
}

/// The nine NYSE holidays that follow a rule, and Good Friday by date.
fn is_holiday(year: u64, month: u64, day: u64) -> bool {
    match month {
        1 => day == observed(year, 1, 1) || day == nth_weekday(year, 1, MONDAY, 3),
        2 => day == nth_weekday(year, 2, MONDAY, 3),
        5 => day == last_weekday(year, 5, MONDAY),
        6 => day == observed(year, 6, 19),
        7 => day == observed(year, 7, 4),
        9 => day == nth_weekday(year, 9, MONDAY, 1),
        11 => day == nth_weekday(year, 11, THURSDAY, 4),
        // NYSE does not close on the Friday before a Saturday New Year's Day, so December has
        // only Christmas.
        12 => day == observed(year, 12, 25),
        _ => is_good_friday(year, month, day),
    }
}

fn is_good_friday(year: u64, month: u64, day: u64) -> bool {
    match year {
        2026 => month == 4 && day == 3,
        2027 => month == 3 && day == 26,
        2028 => month == 4 && day == 14,
        2029 => month == 3 && day == 30,
        2030 => month == 4 && day == 19,
        _ => false,
    }
}

/// The day after Thanksgiving, and a weekday Christmas Eve or 3 July that is not itself the
/// observed holiday.
fn is_early_close(year: u64, month: u64, day: u64) -> bool {
    match month {
        11 => day == nth_weekday(year, 11, THURSDAY, 4) + 1,
        12 => is_weekday_eve(year, 12, 24, 25) && day == 24,
        7 => is_weekday_eve(year, 7, 3, 4) && day == 3,
        _ => false,
    }
}

/// Whether `eve` falls on a weekday and the holiday on `holiday` is not observed on it.
fn is_weekday_eve(year: u64, month: u64, eve: u64, holiday: u64) -> bool {
    let weekday = day_of_week(day_number(year, month, eve));
    (1..=5).contains(&weekday) && observed(year, month, holiday) != eve
}
