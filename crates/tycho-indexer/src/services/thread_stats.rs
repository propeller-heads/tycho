//! On-CPU time and run-queue wait of threads, as the kernel's schedstat reports them.
use std::sync::Once;

use tracing::debug;

/// Cumulative on-CPU time and run-queue wait of one thread since it started. The run-queue wait
/// is time the thread was ready to run but had no CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SchedStat {
    pub(crate) cpu_ns: u64,
    pub(crate) runqueue_wait_ns: u64,
}

impl SchedStat {
    /// Reads the counters of the calling thread. Returns `None` if the kernel does not expose
    /// `/proc/thread-self/schedstat`; the first failure is logged at `debug`.
    pub(crate) fn current_thread() -> Option<Self> {
        static LOG_FAILURE: Once = Once::new();
        let stat = std::fs::read_to_string("/proc/thread-self/schedstat")
            .map_err(|err| err.to_string())
            .and_then(|text| {
                Self::parse(&text).ok_or_else(|| format!("unexpected format: {text}"))
            });
        match stat {
            Ok(stat) => Some(stat),
            Err(err) => {
                LOG_FAILURE.call_once(|| debug!(%err, "Thread CPU time is unavailable"));
                None
            }
        }
    }

    /// Parses a schedstat line: on-CPU ns, run-queue wait ns and time slices, space separated.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut fields = text.split_ascii_whitespace();
        let cpu_ns = fields.next()?.parse().ok()?;
        let runqueue_wait_ns = fields.next()?.parse().ok()?;
        Some(Self { cpu_ns, runqueue_wait_ns })
    }

    /// The counters accumulated since `earlier`, or zero for a counter that went down.
    pub(crate) fn since(self, earlier: Self) -> Self {
        Self {
            cpu_ns: self
                .cpu_ns
                .saturating_sub(earlier.cpu_ns),
            runqueue_wait_ns: self
                .runqueue_wait_ns
                .saturating_sub(earlier.runqueue_wait_ns),
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::kernel_line("4030 1200 7\n", Some(SchedStat { cpu_ns: 4030, runqueue_wait_ns: 1200 }))]
    #[case::no_slices("5 6", Some(SchedStat { cpu_ns: 5, runqueue_wait_ns: 6 }))]
    #[case::one_field("5", None)]
    #[case::not_a_number("5 x 7", None)]
    #[case::empty("", None)]
    fn test_parse(#[case] text: &str, #[case] expected: Option<SchedStat>) {
        assert_eq!(SchedStat::parse(text), expected);
    }

    #[test]
    fn test_since_never_goes_below_zero() {
        let earlier = SchedStat { cpu_ns: 10, runqueue_wait_ns: 50 };
        let later = SchedStat { cpu_ns: 25, runqueue_wait_ns: 40 };

        assert_eq!(later.since(earlier), SchedStat { cpu_ns: 15, runqueue_wait_ns: 0 });
    }

    #[test]
    fn test_current_thread_counts_cpu_work() {
        let Some(before) = SchedStat::current_thread() else {
            // Kernels without schedstat return None by design; nothing to measure here.
            return;
        };
        let mut x: u64 = 0;
        for i in 0..20_000_000u64 {
            x = x.wrapping_mul(31).wrapping_add(i);
        }
        std::hint::black_box(x);
        let after = SchedStat::current_thread().expect("schedstat was readable before");

        assert!(after.since(before).cpu_ns > 0);
    }
}
