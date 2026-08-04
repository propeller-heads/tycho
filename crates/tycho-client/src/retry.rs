//! Retry pacing shared by the websocket client and the state synchronizers.
//!
//! Both retry loops reconnect or re-bootstrap against the same server, so how they space their
//! attempts decides how much load a server outage generates. A constant cooldown makes every
//! client in a fleet retry in lockstep at a fixed rate for as long as the outage lasts: the
//! attempts stay perfectly correlated (they were all dropped by the same event) and the rate never
//! decays, so the server is hit hardest exactly while it is recovering.
//!
//! [`RetryConfiguration::exponential`] addresses both halves of that: the delay doubles per
//! consecutive failure (decaying rate) and each delay is drawn uniformly from `[0, bound]`
//! (decorrelation). Full jitter — rather than a fixed delay with a small random offset — is what
//! spreads clients that failed at the same instant across the whole window.

use std::time::Duration;

use rand::Rng;

/// How a retry loop spaces its attempts.
///
/// Non-exhaustive: new pacing strategies may be added, so match on it via the accessors rather
/// than on its variants.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum RetryConfiguration {
    Constant(ConstantRetryConfiguration),
    Exponential(ExponentialRetryConfiguration),
}

impl RetryConfiguration {
    /// A fixed delay between attempts.
    ///
    /// Prefer [`RetryConfiguration::exponential`]: a fixed delay keeps retries of all clients
    /// correlated and does not decay while the server is down.
    pub fn constant(max_attempts: u64, cooldown: Duration) -> Self {
        RetryConfiguration::Constant(ConstantRetryConfiguration { max_attempts, cooldown })
    }

    /// Exponentially growing delay with full jitter.
    ///
    /// Attempt `n` (0-based) waits a uniformly random duration in
    /// `[0, min(initial_cooldown * 2^n, max_cooldown)]`.
    pub fn exponential(
        max_attempts: u64,
        initial_cooldown: Duration,
        max_cooldown: Duration,
    ) -> Self {
        RetryConfiguration::Exponential(ExponentialRetryConfiguration {
            max_attempts,
            initial_cooldown,
            max_cooldown,
        })
    }

    /// How many attempts the loop makes before giving up.
    pub fn max_attempts(&self) -> u64 {
        match self {
            RetryConfiguration::Constant(c) => c.max_attempts,
            RetryConfiguration::Exponential(c) => c.max_attempts,
        }
    }

    /// The delay used for the first retry, ignoring jitter and growth.
    ///
    /// Only meaningful for comparing two configurations against each other.
    pub fn initial_cooldown(&self) -> Duration {
        match self {
            RetryConfiguration::Constant(c) => c.cooldown,
            RetryConfiguration::Exponential(c) => c.initial_cooldown,
        }
    }

    /// Returns the same pacing with a different attempt budget.
    pub fn with_max_attempts(&self, max_attempts: u64) -> Self {
        match self {
            RetryConfiguration::Constant(c) => {
                RetryConfiguration::constant(max_attempts, c.cooldown)
            }
            RetryConfiguration::Exponential(c) => {
                RetryConfiguration::exponential(max_attempts, c.initial_cooldown, c.max_cooldown)
            }
        }
    }

    /// Upper bound of the delay before `attempt` (0-based, i.e. the delay after the first failure
    /// is `attempt == 0`). Deterministic — the jitter is applied by [`Self::delay`].
    pub fn delay_bound(&self, attempt: u64) -> Duration {
        match self {
            RetryConfiguration::Constant(c) => c.cooldown,
            RetryConfiguration::Exponential(c) => {
                // Clamping the exponent keeps `2^attempt` in range; any attempt beyond it is
                // capped by `max_cooldown` regardless.
                let factor = 2u32.saturating_pow(attempt.min(31) as u32);
                c.initial_cooldown
                    .checked_mul(factor)
                    .unwrap_or(c.max_cooldown)
                    .min(c.max_cooldown)
            }
        }
    }

    /// Delay to wait before `attempt` (0-based).
    pub fn delay(&self, attempt: u64) -> Duration {
        let bound = self.delay_bound(attempt);
        match self {
            RetryConfiguration::Constant(_) => bound,
            RetryConfiguration::Exponential(_) => {
                let bound_millis = bound.as_millis().min(u64::MAX as u128) as u64;
                Duration::from_millis(rand::thread_rng().gen_range(0..=bound_millis))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConstantRetryConfiguration {
    pub(crate) max_attempts: u64,
    pub(crate) cooldown: Duration,
}

#[derive(Clone, Debug)]
pub struct ExponentialRetryConfiguration {
    pub(crate) max_attempts: u64,
    pub(crate) initial_cooldown: Duration,
    pub(crate) max_cooldown: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_delay_does_not_grow() {
        let config = RetryConfiguration::constant(5, Duration::from_secs(3));

        assert_eq!(config.max_attempts(), 5);
        for attempt in 0..10 {
            assert_eq!(config.delay(attempt), Duration::from_secs(3));
        }
    }

    #[test]
    fn test_exponential_bound_doubles_then_caps() {
        let config =
            RetryConfiguration::exponential(32, Duration::from_secs(3), Duration::from_secs(60));

        assert_eq!(config.delay_bound(0), Duration::from_secs(3));
        assert_eq!(config.delay_bound(1), Duration::from_secs(6));
        assert_eq!(config.delay_bound(2), Duration::from_secs(12));
        assert_eq!(config.delay_bound(3), Duration::from_secs(24));
        assert_eq!(config.delay_bound(4), Duration::from_secs(48));
        // 96s would exceed the cap.
        assert_eq!(config.delay_bound(5), Duration::from_secs(60));
        // Far beyond the point where `2^attempt` would overflow.
        assert_eq!(config.delay_bound(1_000), Duration::from_secs(60));
    }

    #[test]
    fn test_exponential_delay_is_jittered_within_bound() {
        let config =
            RetryConfiguration::exponential(32, Duration::from_secs(1), Duration::from_secs(60));

        // Sampling the same attempt repeatedly must stay within the bound and must not always
        // return the same value — that decorrelation is the point of the jitter.
        let samples: Vec<_> = (0..100)
            .map(|_| config.delay(4))
            .collect();
        let bound = config.delay_bound(4);
        assert!(samples.iter().all(|d| *d <= bound), "jittered delay must not exceed the bound");
        assert!(
            samples.iter().any(|d| *d != samples[0]),
            "100 samples all equal - jitter is not being applied"
        );
    }

    #[test]
    fn test_zero_cooldown_is_supported() {
        let config = RetryConfiguration::exponential(3, Duration::ZERO, Duration::ZERO);

        assert_eq!(config.delay(0), Duration::ZERO);
        assert_eq!(config.delay(7), Duration::ZERO);
    }

    #[test]
    fn test_with_max_attempts_keeps_pacing() {
        let exponential =
            RetryConfiguration::exponential(32, Duration::from_secs(2), Duration::from_secs(60))
                .with_max_attempts(8);
        assert_eq!(exponential.max_attempts(), 8);
        assert_eq!(exponential.delay_bound(3), Duration::from_secs(16));

        let constant =
            RetryConfiguration::constant(32, Duration::from_secs(2)).with_max_attempts(8);
        assert_eq!(constant.max_attempts(), 8);
        assert_eq!(constant.delay_bound(3), Duration::from_secs(2));
    }
}
