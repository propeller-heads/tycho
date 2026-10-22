//! On-CPU time and run-queue wait of threads, as the kernel's schedstat reports them.
//!
//! [`spawn_thread_cpu_reporter`] exports them per thread group:
//! `tycho_thread_cpu_ns_total{group}`, `tycho_thread_runqueue_wait_ns_total{group}` and
//! `tycho_threads{group}`.
use std::{collections::HashMap, path::Path, sync::Once, time::Duration};

use tokio::task::JoinHandle;
use tracing::debug;

/// Thread name of the tokio runtime that runs the extractors.
pub const EXTRACTION_THREAD_NAME: &str = "tycho-extract";
/// Thread name of the tokio runtime that runs everything else outside the request workers.
pub const MAIN_THREAD_NAME: &str = "tycho-main";

const REPORT_PERIOD: Duration = Duration::from_secs(5);

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

    fn plus(self, other: Self) -> Self {
        Self {
            cpu_ns: self.cpu_ns.saturating_add(other.cpu_ns),
            runqueue_wait_ns: self
                .runqueue_wait_ns
                .saturating_add(other.runqueue_wait_ns),
        }
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

/// Threads of the indexer process, grouped by thread name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ThreadGroup {
    /// The actix request workers, which run the request handlers. The metrics server's workers
    /// have the same name and fall in this group too.
    RpcWorker,
    /// Threads with tokio's default name: the blocking pools of the request workers, which run
    /// the off-worker jobs and response compression.
    RpcBlocking,
    Extraction,
    Main,
    Other,
}

impl ThreadGroup {
    const ALL: [ThreadGroup; 5] = [
        ThreadGroup::RpcWorker,
        ThreadGroup::RpcBlocking,
        ThreadGroup::Extraction,
        ThreadGroup::Main,
        ThreadGroup::Other,
    ];

    /// The group of a thread named `name`. The kernel keeps only the first 15 bytes of a name.
    pub(crate) fn of(name: &str) -> Self {
        if name.starts_with("actix-server wo") {
            ThreadGroup::RpcWorker
        } else if name.starts_with("tokio-runtime-w") {
            ThreadGroup::RpcBlocking
        } else if name == EXTRACTION_THREAD_NAME {
            ThreadGroup::Extraction
        } else if name == MAIN_THREAD_NAME {
            ThreadGroup::Main
        } else {
            ThreadGroup::Other
        }
    }

    fn label(self) -> &'static str {
        match self {
            ThreadGroup::RpcWorker => "rpc_worker",
            ThreadGroup::RpcBlocking => "rpc_blocking",
            ThreadGroup::Extraction => "extraction",
            ThreadGroup::Main => "main",
            ThreadGroup::Other => "other",
        }
    }
}

/// A thread's identity: its id and its start time, since the kernel reuses ids.
type ThreadId = (u32, u64);

/// One thread's counters at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ThreadSample {
    id: ThreadId,
    group: ThreadGroup,
    stat: SchedStat,
}

/// The counters a group gained since the previous sample, and how many threads it has now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GroupDelta {
    stat: SchedStat,
    threads: usize,
}

/// Turns per-thread counters, which drop when a thread exits, into per-group increments that
/// never go down. A thread's counters since the last sample before it exits are lost.
#[derive(Default)]
struct ThreadCpuTotals {
    last: HashMap<ThreadId, SchedStat>,
}

impl ThreadCpuTotals {
    /// Returns what each group gained since the previous call. A thread seen for the first time
    /// counts with everything since it started.
    fn advance(&mut self, samples: &[ThreadSample]) -> HashMap<ThreadGroup, GroupDelta> {
        let mut deltas: HashMap<ThreadGroup, GroupDelta> = HashMap::new();
        let mut last = HashMap::with_capacity(samples.len());
        for sample in samples {
            let previous = self
                .last
                .get(&sample.id)
                .copied()
                .unwrap_or_default();
            let delta = deltas.entry(sample.group).or_default();
            delta.stat = delta
                .stat
                .plus(sample.stat.since(previous));
            delta.threads += 1;
            last.insert(sample.id, sample.stat);
        }
        self.last = last;
        deltas
    }
}

/// Parses the name and start time (in clock ticks since boot) out of a
/// `/proc/<pid>/task/<tid>/stat` line. The name is in parentheses and may contain spaces and
/// parentheses itself.
fn parse_task_stat(text: &str) -> Option<(&str, u64)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let name = text.get(open + 1..close)?;
    // The fields after the name start at field 3 (state); the start time is field 22.
    let start_time = text
        .get(close + 1..)?
        .split_ascii_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    Some((name, start_time))
}

/// Reads the counters of every thread under `task_dir` (`/proc/self/task`). A thread that exits
/// while it is read is left out.
fn read_threads(task_dir: &Path) -> Vec<ThreadSample> {
    let Ok(entries) = std::fs::read_dir(task_dir) else {
        return Vec::new();
    };
    let mut samples = Vec::new();
    for entry in entries.flatten() {
        let Some(tid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let path = entry.path();
        let Ok(stat_text) = std::fs::read_to_string(path.join("stat")) else {
            continue;
        };
        let Some((name, start_time)) = parse_task_stat(&stat_text) else {
            continue;
        };
        let Some(stat) = std::fs::read_to_string(path.join("schedstat"))
            .ok()
            .and_then(|text| SchedStat::parse(&text))
        else {
            continue;
        };
        samples.push(ThreadSample { id: (tid, start_time), group: ThreadGroup::of(name), stat });
    }
    samples
}

/// Every 5 s, reads the counters of all threads of this process and adds what each thread group
/// gained to `tycho_thread_cpu_ns_total{group}` and
/// `tycho_thread_runqueue_wait_ns_total{group}`; sets `tycho_threads{group}` to its thread count.
/// The period is shorter than the 10 s after which an idle blocking thread exits, so the work of
/// a blocking thread is counted before it exits.
pub fn spawn_thread_cpu_reporter() -> JoinHandle<()> {
    tokio::spawn(async {
        let task_dir = Path::new("/proc/self/task");
        let mut totals = ThreadCpuTotals::default();
        let mut interval = tokio::time::interval(REPORT_PERIOD);
        loop {
            interval.tick().await;
            let deltas = totals.advance(&read_threads(task_dir));
            for group in ThreadGroup::ALL {
                let delta = deltas
                    .get(&group)
                    .copied()
                    .unwrap_or_default();
                let label = group.label();
                metrics::counter!("tycho_thread_cpu_ns_total", "group" => label)
                    .increment(delta.stat.cpu_ns);
                metrics::counter!("tycho_thread_runqueue_wait_ns_total", "group" => label)
                    .increment(delta.stat.runqueue_wait_ns);
                metrics::gauge!("tycho_threads", "group" => label).set(delta.threads as f64);
            }
        }
    })
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

    fn sample(tid: u32, start_time: u64, group: ThreadGroup, cpu_ns: u64) -> ThreadSample {
        ThreadSample {
            id: (tid, start_time),
            group,
            stat: SchedStat { cpu_ns, runqueue_wait_ns: cpu_ns / 10 },
        }
    }

    #[rstest]
    #[case::actix_worker("actix-server wo", ThreadGroup::RpcWorker)]
    #[case::tokio_default("tokio-runtime-w", ThreadGroup::RpcBlocking)]
    #[case::extraction("tycho-extract", ThreadGroup::Extraction)]
    #[case::main("tycho-main", ThreadGroup::Main)]
    #[case::process("tycho-indexer", ThreadGroup::Other)]
    fn test_group_of(#[case] name: &str, #[case] expected: ThreadGroup) {
        assert_eq!(ThreadGroup::of(name), expected);
    }

    #[test]
    fn test_parse_task_stat_handles_names_with_spaces_and_parentheses() {
        let rest = (3..=21)
            .map(|field| field.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let text = format!("42 (actix-server (wo)) {rest} 98765 0 0\n");

        assert_eq!(parse_task_stat(&text), Some(("actix-server (wo)", 98765)));
        assert_eq!(parse_task_stat("42 (short) S 1"), None);
    }

    #[test]
    fn test_advance_counts_only_new_work() {
        let mut totals = ThreadCpuTotals::default();
        let first = totals.advance(&[
            sample(1, 10, ThreadGroup::RpcBlocking, 100),
            sample(2, 10, ThreadGroup::RpcBlocking, 50),
        ]);
        // Thread 2 exits, thread 1 works on, thread 3 starts.
        let second = totals.advance(&[
            sample(1, 10, ThreadGroup::RpcBlocking, 130),
            sample(3, 20, ThreadGroup::Main, 40),
        ]);

        assert_eq!(
            first[&ThreadGroup::RpcBlocking]
                .stat
                .cpu_ns,
            150
        );
        assert_eq!(first[&ThreadGroup::RpcBlocking].threads, 2);
        assert_eq!(
            second[&ThreadGroup::RpcBlocking]
                .stat
                .cpu_ns,
            30
        );
        assert_eq!(second[&ThreadGroup::RpcBlocking].threads, 1);
        assert_eq!(second[&ThreadGroup::Main].stat.cpu_ns, 40);
    }

    #[test]
    fn test_advance_treats_a_reused_thread_id_as_a_new_thread() {
        let mut totals = ThreadCpuTotals::default();
        totals.advance(&[sample(7, 10, ThreadGroup::RpcBlocking, 500)]);

        // Same id, later start: a new thread whose counters started at zero.
        let deltas = totals.advance(&[sample(7, 30, ThreadGroup::RpcBlocking, 20)]);

        assert_eq!(
            deltas[&ThreadGroup::RpcBlocking]
                .stat
                .cpu_ns,
            20
        );
    }

    #[test]
    fn test_read_threads_sees_this_process() {
        let samples = read_threads(Path::new("/proc/self/task"));
        if SchedStat::current_thread().is_none() {
            // Kernels without schedstat yield no samples by design.
            assert!(samples.is_empty());
            return;
        }

        assert!(!samples.is_empty());
    }
}
