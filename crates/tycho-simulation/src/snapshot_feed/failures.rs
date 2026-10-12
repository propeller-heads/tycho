//! How long a feed waits after a failure, and when it stops trying.

use std::{cmp::min, time::Duration};

/// What a recorded failure earns when the feed carries on: how long a streak it extends, and how
/// long to wait before the attempt that follows it.
pub struct Retry {
    pub consecutive: u32,
    pub backoff: Duration,
}

/// Consecutive-failure counting with capped exponential backoff, shared by the WebSocket and
/// HTTP feed loops so every feed waits, and gives up (or retries forever), the same way.
pub struct FailureTracker {
    consecutive: u32,
    max: Option<u32>,
    backoff_unit: Duration,
    max_backoff_exp: u32,
}

impl FailureTracker {
    pub fn new(max: Option<u32>, backoff_unit: Duration, max_backoff_exp: u32) -> Self {
        FailureTracker { consecutive: 0, max, backoff_unit, max_backoff_exp }
    }

    /// Clears the streak and returns how many consecutive failures it had reached.
    pub fn record_success(&mut self) -> u32 {
        std::mem::take(&mut self.consecutive)
    }

    /// Records one failure: `Ok` with the [`Retry`] it earns while the budget holds, `Err` once it
    /// is used up and the feed is to give up. Both carry the streak, which is what a caller reports
    /// the failure by. The wait doubles per failure already in the streak, so the first one waits a
    /// single `backoff_unit` and the `max_backoff_exp`-th and every later one wait the cap. A feed
    /// left retrying forever stays at `u32::MAX` rather than wrapping back to a fresh streak.
    pub fn record_failure(&mut self) -> Result<Retry, u32> {
        let preceding = self.consecutive;
        self.consecutive = preceding.saturating_add(1);
        if self
            .max
            .is_some_and(|max| self.consecutive >= max)
        {
            Err(self.consecutive)
        } else {
            Ok(Retry {
                consecutive: self.consecutive,
                backoff: self
                    .backoff_unit
                    .saturating_mul(2_u32.saturating_pow(min(preceding, self.max_backoff_exp))),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::limit_reached_at_max(Some(3), 3, true)]
    #[case::below_limit(Some(3), 2, false)]
    fn failure_tracker_limit(
        #[case] max: Option<u32>,
        #[case] failures: u32,
        #[case] expect_reached: bool,
    ) {
        let mut tracker = FailureTracker::new(max, Duration::from_secs(1), 5);

        let mut recorded = Err(0);
        for _ in 0..failures {
            recorded = tracker.record_failure();
        }

        assert_eq!(recorded.is_err(), expect_reached);
        assert_eq!(
            recorded.map_or_else(|consecutive| consecutive, |retry| retry.consecutive),
            failures
        );
    }

    #[rstest]
    #[case::first_failure_waits_one_unit(1, Duration::from_secs(1))]
    #[case::doubles_per_further_failure(4, Duration::from_secs(8))]
    #[case::capped_at_max_exp(10, Duration::from_secs(32))]
    fn failure_tracker_backoff(#[case] failures: u32, #[case] expected: Duration) {
        let mut tracker = FailureTracker::new(None, Duration::from_secs(1), 5);

        let mut backoff = Duration::ZERO;
        for _ in 0..failures {
            // Always `Ok`: this tracker has no limit to use up.
            backoff = tracker
                .record_failure()
                .expect("no limit to reach")
                .backoff;
        }

        assert_eq!(backoff, expected);
    }
}
