//! The grid of timestamps a TWAMM order (or a fee boost) may start or end on, ported from
//! `evm-contracts/src/math/time.sol`. The step grows in powers of 16 with the distance from the
//! current time, so at most 91 valid times lie ahead of any moment. The seed only needs to check
//! single timestamps; the grid enumeration is kept as the test oracle for that check.

#[cfg(test)]
pub const MAX_NUM_VALID_TIMES: usize = 91;

/// The multiple `time` must be of, given how far it lies after `current_time`.
pub fn compute_step_size(current_time: u64, time: u64) -> u64 {
    if time <= current_time + 4095 {
        return 256;
    }

    let diff = time - current_time;
    let msb = 63 - diff.leading_zeros();
    let msb = msb - msb % 4;

    1 << msb
}

pub fn is_time_valid(current_time: u64, time: u64) -> bool {
    let step = compute_step_size(current_time, time);

    time.is_multiple_of(step) && (time < current_time || time - current_time < 1 << 32)
}

/// The smallest valid time after `after_time`, or `None` when no valid time within `u32::MAX`
/// seconds of `current_time` remains.
#[cfg(test)]
pub fn next_valid_time(current_time: u64, after_time: u64) -> Option<u64> {
    let step = compute_step_size(current_time, after_time);
    let mut next = after_time.checked_add(step)?;
    next -= next % step;

    let next_step = compute_step_size(current_time, next);
    if next_step != step {
        next = after_time.checked_add(next_step)?;
        next -= next % next_step;
    }

    (next <= current_time + u32::MAX as u64).then_some(next)
}

/// Every valid time after `current_time`, ascending
/// (`TWAMMDataFetcher.sol::getAllValidFutureTimes`).
#[cfg(test)]
pub fn all_valid_future_times(current_time: u64) -> Vec<u64> {
    let mut times = Vec::with_capacity(MAX_NUM_VALID_TIMES);
    let mut time = current_time;
    while let Some(next) = next_valid_time(current_time, time) {
        times.push(next);
        time = next;
    }

    times
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_size_grows_in_powers_of_sixteen() {
        let now = 1_800_000_000;

        assert_eq!(compute_step_size(now, now), 256);
        assert_eq!(compute_step_size(now, now + 4095), 256);
        assert_eq!(compute_step_size(now, now + 4096), 4096);
        assert_eq!(compute_step_size(now, now + 65535), 4096);
        assert_eq!(compute_step_size(now, now + 65536), 65536);
    }

    #[test]
    fn future_times_are_valid_ascending_and_bounded() {
        let now = 1_800_000_123;
        let times = all_valid_future_times(now);

        assert!(!times.is_empty());
        assert!(times.len() <= MAX_NUM_VALID_TIMES, "{} valid times", times.len());
        assert!(times
            .windows(2)
            .all(|pair| pair[0] < pair[1]));
        assert!(times
            .iter()
            .all(|&time| is_time_valid(now, time)));
        assert_eq!(times[0], (now / 256 + 1) * 256);
        assert!(*times.last().unwrap() <= now + u32::MAX as u64);
    }

    #[test]
    fn next_valid_time_jumps_to_the_coarser_grid() {
        let now = 1_800_000_000;
        let last_fine = now + 4095 - (now + 4095) % 256;

        let next = next_valid_time(now, last_fine).unwrap();

        assert_eq!(next % 4096, 0);
        assert!(next > now + 4095);
    }
}
