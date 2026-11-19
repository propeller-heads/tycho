//! Local time of a US market, ported from Nami's `UsMarketTime` library.
//!
//! Civil date arithmetic and the US daylight saving rule, computed from a timestamp alone. Every
//! function mirrors the Solidity one of the same name, operation for operation.

const DAY: u64 = 86_400;
const HOUR: u64 = 3_600;

/// Midnight UTC of the Sundays US clocks change, for the years the contract lists.
const DST_2026_START: u64 = 1_772_928_000;
const DST_2026_END: u64 = 1_793_491_200;
const DST_2027_START: u64 = 1_804_982_400;
const DST_2027_END: u64 = 1_825_545_600;
const DST_2028_START: u64 = 1_836_432_000;
const DST_2028_END: u64 = 1_856_995_200;
/// 1 January 2029 00:00 UTC, from which the contract computes the switch days by rule.
const DST_TABLE_END: u64 = 1_861_920_000;

/// Timestamps from here on are refused. A `uint40` holds every timestamp the plugin stores, and
/// below it none of the calendar arithmetic can overflow a `u64`.
const TIMESTAMP_END: u64 = 1 << 40;

/// A moment in a market's local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Local {
    /// Days since the Unix epoch, counted in local time.
    pub(super) day: u64,
    pub(super) second_of_day: u64,
    /// 0 is Sunday, 6 is Saturday.
    pub(super) weekday: u64,
    pub(super) year: u64,
    pub(super) month: u64,
    pub(super) day_of_month: u64,
}

/// Local time at `timestamp` in the US zone `standard_hours_west` hours behind UTC in winter.
///
/// `None` where the contract reverts (a timestamp earlier than the zone's offset) and for
/// timestamps of 2^40 seconds or more.
pub(super) fn to_local(timestamp: u64, standard_hours_west: u64) -> Option<Local> {
    if timestamp >= TIMESTAMP_END {
        return None;
    }
    let mut offset = standard_hours_west * HOUR;
    if is_dst(timestamp, standard_hours_west) {
        offset -= HOUR;
    }
    let local = timestamp.checked_sub(offset)?;
    let day = local / DAY;
    let (year, month, day_of_month) = civil(day);
    Some(Local {
        day,
        second_of_day: local - day * DAY,
        weekday: day_of_week(day),
        year,
        month,
        day_of_month,
    })
}

/// Whether `timestamp` falls inside US daylight saving for the zone.
fn is_dst(timestamp: u64, standard_hours_west: u64) -> bool {
    let forward = (2 + standard_hours_west) * HOUR;
    let back = (1 + standard_hours_west) * HOUR;
    if timestamp >= DST_TABLE_END {
        return is_dst_by_rule(timestamp, standard_hours_west);
    }
    let (start, end) = if timestamp < DST_2027_START {
        (DST_2026_START, DST_2026_END)
    } else if timestamp < DST_2028_START {
        (DST_2027_START, DST_2027_END)
    } else {
        (DST_2028_START, DST_2028_END)
    };
    timestamp >= start + forward && timestamp < end + back
}

/// Daylight saving from the second Sunday of March to the first Sunday of November.
fn is_dst_by_rule(timestamp: u64, standard_hours_west: u64) -> bool {
    let (year, _, _) = civil(timestamp / DAY);
    let start =
        day_number(year, 3, nth_weekday(year, 3, 0, 2)) * DAY + (2 + standard_hours_west) * HOUR;
    let end =
        day_number(year, 11, nth_weekday(year, 11, 0, 1)) * DAY + (1 + standard_hours_west) * HOUR;
    timestamp >= start && timestamp < end
}

/// 0 is Sunday, 1 Monday, up to 6 Saturday.
pub(super) fn day_of_week(day: u64) -> u64 {
    (day + 4) % 7
}

/// Day of month of the `n`th `weekday` of a month, `n` counted from 1.
pub(super) fn nth_weekday(year: u64, month: u64, weekday: u64, n: u64) -> u64 {
    let first = day_of_week(day_number(year, month, 1));
    1 + (weekday + 7 - first) % 7 + (n - 1) * 7
}

/// Day of month of the last `weekday` of a month.
pub(super) fn last_weekday(year: u64, month: u64, weekday: u64) -> u64 {
    let length = days_in_month(year, month);
    let last = day_of_week(day_number(year, month, length));
    length - (last + 7 - weekday) % 7
}

/// Day of month on which a fixed-date holiday is observed: the Friday before a Saturday, the
/// Monday after a Sunday. A holiday on the first of a month that falls on Saturday gives 0.
pub(super) fn observed(year: u64, month: u64, day: u64) -> u64 {
    match day_of_week(day_number(year, month, day)) {
        6 => day - 1,
        0 => day + 1,
        _ => day,
    }
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn is_leap(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

/// The civil date `(year, month, day)` of a day counted from the Unix epoch.
pub(super) fn civil(day: u64) -> (u64, u64, u64) {
    let z = day + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d)
}

/// Days from the Unix epoch to a civil date in 1970 or later.
pub(super) fn day_number(year: u64, month: u64, day: u64) -> u64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    const NEW_YORK: u64 = 5;
    const CHICAGO: u64 = 6;

    #[rstest]
    #[case::epoch(0, (1970, 1, 1))]
    #[case::leap_day_2028(21_243, (2028, 2, 29))]
    #[case::day_after_leap_day(21_244, (2028, 3, 1))]
    #[case::end_of_2026(20_818, (2026, 12, 31))]
    #[case::century_non_leap(47_541, (2100, 3, 1))]
    fn civil_date_of_a_day(#[case] day: u64, #[case] date: (u64, u64, u64)) {
        assert_eq!(civil(day), date);
        assert_eq!(day_number(date.0, date.1, date.2), day);
    }

    #[rstest]
    // 8 March 2026: clocks go forward at 02:00 EST, 07:00 UTC.
    #[case::before_spring_forward(1_772_953_199, NEW_YORK, false)]
    #[case::at_spring_forward(1_772_953_200, NEW_YORK, true)]
    // 1 November 2026: clocks go back at 02:00 EDT, 06:00 UTC.
    #[case::before_fall_back(1_793_512_799, NEW_YORK, true)]
    #[case::at_fall_back(1_793_512_800, NEW_YORK, false)]
    // Chicago switches an hour after New York.
    #[case::chicago_still_standard(1_772_953_200, CHICAGO, false)]
    #[case::chicago_spring_forward(1_772_956_800, CHICAGO, true)]
    // 11 March 2029, past the table: 07:00 UTC by the rule.
    #[case::rule_before_spring_forward(1_867_906_799, NEW_YORK, false)]
    #[case::rule_at_spring_forward(1_867_906_800, NEW_YORK, true)]
    // 4 November 2029: 06:00 UTC by the rule.
    #[case::rule_before_fall_back(1_888_466_399, NEW_YORK, true)]
    #[case::rule_at_fall_back(1_888_466_400, NEW_YORK, false)]
    fn daylight_saving_switches_at_the_zone_s_two_o_clock(
        #[case] timestamp: u64,
        #[case] hours_west: u64,
        #[case] dst: bool,
    ) {
        assert_eq!(is_dst(timestamp, hours_west), dst);
    }

    #[test]
    fn local_time_applies_the_daylight_offset() {
        // 6 July 2026 13:30:00 UTC is 09:30 in New York, a Monday.
        let local = to_local(1_783_344_600, NEW_YORK).expect("a 2026 timestamp should convert");

        assert_eq!(
            local,
            Local {
                day: 20_640,
                second_of_day: 9 * HOUR + 1_800,
                weekday: 1,
                year: 2026,
                month: 7,
                day_of_month: 6,
            }
        );
    }

    #[rstest]
    #[case::earlier_than_the_offset(5 * HOUR - 1)]
    #[case::past_the_supported_range(TIMESTAMP_END)]
    fn out_of_range_timestamp_has_no_local_time(#[case] timestamp: u64) {
        assert_eq!(to_local(timestamp, NEW_YORK), None);
    }

    #[rstest]
    #[case::third_monday_of_january(2026, 1, 1, 3, 19)]
    #[case::fourth_thursday_of_november(2026, 11, 4, 4, 26)]
    #[case::second_sunday_of_march(2027, 3, 0, 2, 14)]
    #[case::first_weekday_is_the_first(2026, 9, 2, 1, 1)]
    fn nth_weekday_of_a_month(
        #[case] year: u64,
        #[case] month: u64,
        #[case] weekday: u64,
        #[case] n: u64,
        #[case] day: u64,
    ) {
        assert_eq!(nth_weekday(year, month, weekday, n), day);
    }

    #[rstest]
    #[case::memorial_day_2026(2026, 5, 1, 25)]
    #[case::memorial_day_2027(2027, 5, 1, 31)]
    fn last_weekday_of_a_month(
        #[case] year: u64,
        #[case] month: u64,
        #[case] weekday: u64,
        #[case] day: u64,
    ) {
        assert_eq!(last_weekday(year, month, weekday), day);
    }

    #[rstest]
    // 4 July 2026 is a Saturday, observed on Friday the 3rd.
    #[case::saturday_moves_to_friday(2026, 7, 4, 3)]
    // 4 July 2027 is a Sunday, observed on Monday the 5th.
    #[case::sunday_moves_to_monday(2027, 7, 4, 5)]
    #[case::weekday_stays(2028, 7, 4, 4)]
    // 1 January 2028 is a Saturday: the Friday before is in December.
    #[case::saturday_first_of_month(2028, 1, 1, 0)]
    fn observed_day_of_a_fixed_holiday(
        #[case] year: u64,
        #[case] month: u64,
        #[case] day: u64,
        #[case] observed_day: u64,
    ) {
        assert_eq!(observed(year, month, day), observed_day);
    }
}
