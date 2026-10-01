//! Shadow mode: on a sample of state requests, runs the cache path next to the database path,
//! compares the two answers, and records the outcome. Clients always get the database answer.
//!
//! Both answers are normalized first: the differences we know about are erased from both sides.
//! What remains must be equal. Only when it is not does `locate` find where the answers differ,
//! through their JSON form, so it needs no code per response field.

use std::{
    future::Future,
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
    time::Duration,
};

use metrics::{counter, gauge, histogram};
use tracing::warn;
use tycho_common::{dto, Bytes};

use super::service::{StateService, StateServiceError};
use crate::services::rpc::RpcError;

/// Most differences one comparison collects and logs.
#[allow(dead_code, reason = "shadow mode skeleton")]
const MAX_LOGGED_DIFFS: usize = 20;

/// Longest value a difference logs in full: a 32-byte word in hex. Longer values are logged as
/// their length and a hash.
#[allow(dead_code, reason = "shadow mode skeleton")]
const MAX_LOGGED_VALUE_LEN: usize = 66;

/// Decides which state requests shadow mode compares. The decision depends only on the request
/// body, so a repeated request gets the same decision.
#[derive(Debug)]
pub(crate) struct Sampler {
    /// A request is sampled when its hash is at or below this value. `0` samples nothing.
    threshold: u64,
}

impl Sampler {
    /// Samples the share `rate` of requests, from `0.0` (none) to `1.0` (all).
    pub(crate) fn new(rate: f64) -> Self {
        // `as` saturates, so a rate of 1.0 gives `u64::MAX`.
        Self { threshold: (rate * u64::MAX as f64) as u64 }
    }

    /// Whether shadow mode compares `request`. Does no work when the rate is zero.
    fn samples(&self, request: &impl Hash) -> bool {
        if self.threshold == 0 {
            return false;
        }
        // `DefaultHasher::new` uses fixed keys: the same body gets the same decision in every
        // process of one build, so a mismatch can be reproduced.
        let mut hasher = DefaultHasher::new();
        request.hash(&mut hasher);
        hasher.finish() <= self.threshold
    }
}

/// Shadow mode: the state service it compares with, and which requests it compares.
pub(crate) struct Shadow {
    #[allow(dead_code, reason = "shadow mode skeleton")]
    service: Arc<StateService>,
    sampler: Sampler,
}

impl Shadow {
    pub(crate) fn new(service: Arc<StateService>, sampler: Sampler) -> Self {
        Self { service, sampler }
    }

    /// Whether shadow mode compares `request`.
    pub(crate) fn samples(&self, request: &impl Hash) -> bool {
        self.sampler.samples(request)
    }

    /// Answers `request` from `db`, then runs `cache` and compares the two answers with
    /// `compare`. Records the comparison and returns the database answer, whatever the cache path
    /// did.
    ///
    /// A comparison that straddled a change to the window or the cache is discarded. A panic is
    /// reported even when it straddled one: a panic under the window lock poisons it, and that
    /// alone moves the straddle token.
    #[allow(unused_variables, reason = "shadow mode skeleton")]
    pub(crate) async fn run<R>(
        &self,
        request: SampledRequest<'_>,
        db: impl Future<Output = Result<R, RpcError>>,
        cache: impl FnOnce(&StateService) -> Result<R, StateServiceError>,
        compare: impl FnOnce(&StateService, &Result<R, RpcError>, CacheAnswer<R>) -> Comparison,
    ) -> Result<R, RpcError> {
        // Reads the straddle token, awaits `db`, then runs `cache` through `run_cache_path` and
        // reads the token again. Compares with `compare`, unless the read straddled a change and
        // did not panic: then the outcome is `Discarded`. Records the comparison and returns the
        // database answer.
        todo!()
    }
}

/// What the cache path returned for a sampled request.
#[allow(dead_code, reason = "shadow mode skeleton")]
pub(crate) enum CacheAnswer<R> {
    Answered(R),
    /// The cache cannot answer the request; serve mode would ask the database path.
    Fallback,
    Failed(StateServiceError),
    Panicked,
}

/// Runs the cache path and turns a panic into [`CacheAnswer::Panicked`], so a cache bug cannot
/// fail the client's request.
#[allow(dead_code, unused_variables, reason = "shadow mode skeleton")]
fn run_cache_path<R>(read: impl FnOnce() -> Result<R, StateServiceError>) -> CacheAnswer<R> {
    // Runs `read` under `catch_unwind`. `Fallback` becomes `CacheAnswer::Fallback`, another error
    // `CacheAnswer::Failed`, and a panic `CacheAnswer::Panicked`.
    todo!()
}

/// How a sampled request compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// The answers are equal once the known differences are erased, or both paths failed with the
    /// same error.
    Match,
    /// The cache path failed in a known way; see [`known_error`].
    KnownError,
    /// The answers differ.
    Mismatch,
    /// The database path failed for a reason that says nothing about the data: a lost connection
    /// or an unexpected storage error.
    DbFailed,
    /// The comparison straddled a change to the window or the cache.
    Discarded,
    /// The cache cannot answer the request; serve mode would ask the database path.
    Fallback,
    CachePanicked,
}

impl Outcome {
    const ALL: [Outcome; 7] = [
        Outcome::Match,
        Outcome::KnownError,
        Outcome::Mismatch,
        Outcome::DbFailed,
        Outcome::Discarded,
        Outcome::Fallback,
        Outcome::CachePanicked,
    ];

    fn label(self) -> &'static str {
        match self {
            Outcome::Match => "match",
            Outcome::KnownError => "known_error",
            Outcome::Mismatch => "mismatch",
            Outcome::DbFailed => "db_failed",
            Outcome::Discarded => "discarded",
            Outcome::Fallback => "fallback",
            Outcome::CachePanicked => "cache_panicked",
        }
    }
}

/// The result of comparing one sampled request.
#[derive(Debug)]
pub(crate) struct Comparison {
    outcome: Outcome,
    /// For a mismatch, where the answers differ, as `path: db=… cache=…`; at most
    /// [`MAX_LOGGED_DIFFS`]. Empty otherwise.
    diffs: Vec<String>,
}

impl Comparison {
    #[allow(dead_code, reason = "shadow mode skeleton")]
    fn without_diffs(outcome: Outcome) -> Self {
        Self { outcome, diffs: Vec::new() }
    }

    #[allow(dead_code, reason = "shadow mode skeleton")]
    fn mismatch(diffs: Vec<String>) -> Self {
        Self { outcome: Outcome::Mismatch, diffs }
    }
}

/// Compares the two answers of a sampled `/contract_state` request.
#[allow(unused_variables, reason = "shadow mode skeleton")]
pub(crate) fn compare_contract_state(
    db: &Result<dto::StateRequestResponse, RpcError>,
    cache: CacheAnswer<dto::StateRequestResponse>,
    held_elsewhere: impl Fn(&Bytes) -> bool,
) -> Comparison {
    // Pairs the two answers, erases the known account differences from both, and gives `Match`
    // when they are equal. Otherwise gives a mismatch with the located diffs.
    todo!()
}

/// Compares the two answers of a sampled `/protocol_state` request.
#[allow(unused_variables, reason = "shadow mode skeleton")]
pub(crate) fn compare_protocol_state(
    include_balances: bool,
    db: &Result<dto::ProtocolStateRequestResponse, RpcError>,
    cache: CacheAnswer<dto::ProtocolStateRequestResponse>,
) -> Comparison {
    // Does what `compare_contract_state` does, for component states. It also erases balances
    // that were not requested and empty unknown components.
    todo!()
}

/// Whether the database-path error `db` is a failure of the database itself.
#[allow(dead_code, unused_variables, reason = "shadow mode skeleton")]
fn db_failed(db: &RpcError) -> bool {
    // True for a lost connection and for an unexpected storage error.
    todo!()
}

/// Whether the cache-path error `cache` is a known difference. `db` is the database-path error,
/// `None` when the database path answered. `held_elsewhere` says whether another extractor's
/// window holds an address.
#[allow(dead_code, unused_variables, reason = "shadow mode skeleton")]
fn known_error(
    db: Option<&RpcError>,
    cache: &StateServiceError,
    held_elsewhere: impl Fn(&Bytes) -> bool,
) -> bool {
    // True for an account that only another extractor's window holds, and for an account that
    // neither path finds.
    todo!()
}

/// The state endpoint a comparison belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Endpoint {
    ContractState,
    ProtocolState,
}

impl Endpoint {
    pub(crate) const ALL: [Endpoint; 2] = [Endpoint::ContractState, Endpoint::ProtocolState];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Endpoint::ContractState => "contract_state",
            Endpoint::ProtocolState => "protocol_state",
        }
    }
}

/// The request fields a comparison log names.
pub(crate) struct SampledRequest<'a> {
    pub(crate) endpoint: Endpoint,
    pub(crate) protocol_system: &'a str,
    pub(crate) version: &'a dto::VersionParam,
    pub(crate) id_count: usize,
}

/// Registers every comparison counter at zero, so the first event of a series counts as growth,
/// and sets the sample-rate gauge. The duration histogram appears with its first sample, like the
/// other histograms of the indexer.
pub(crate) fn register_metrics(sample_rate: f64) {
    for endpoint in Endpoint::ALL {
        for outcome in Outcome::ALL {
            counter!(
                "entity_cache_shadow_comparisons_total",
                "endpoint" => endpoint.label(),
                "outcome" => outcome.label()
            )
            .increment(0);
        }
    }
    gauge!("entity_cache_shadow_sample_rate").set(sample_rate);
}

/// Records one comparison: its outcome, the time the cache path and the comparison took, and a
/// warning with the differences for a mismatch.
#[allow(dead_code, reason = "shadow mode skeleton")]
fn record(request: &SampledRequest<'_>, comparison: &Comparison, elapsed: Duration) {
    let endpoint = request.endpoint.label();
    counter!(
        "entity_cache_shadow_comparisons_total",
        "endpoint" => endpoint,
        "outcome" => comparison.outcome.label()
    )
    .increment(1);
    histogram!("entity_cache_shadow_duration_seconds", "endpoint" => endpoint)
        .record(elapsed.as_secs_f64());
    if comparison.outcome == Outcome::Mismatch {
        warn!(
            endpoint,
            protocol_system = request.protocol_system,
            version = ?request.version,
            id_count = request.id_count,
            diffs = %comparison.diffs.join("; "),
            "Entity cache shadow mismatch"
        );
    }
}

#[cfg(test)]
mod test {
    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder, Snapshotter},
        MetricKind,
    };

    use super::*;

    #[test]
    fn zero_rate_samples_nothing() {
        let sampler = Sampler::new(0.0);

        assert!((0..1_000u64).all(|n| !sampler.samples(&n)));
    }

    #[test]
    fn full_rate_samples_everything() {
        let sampler = Sampler::new(1.0);

        assert!((0..1_000u64).all(|n| sampler.samples(&n)));
    }

    #[test]
    fn the_same_request_gets_the_same_decision() {
        let first = Sampler::new(0.5);
        let second = Sampler::new(0.5);

        assert!((0..1_000u64).all(|n| first.samples(&n) == second.samples(&n)));
    }

    #[test]
    fn rate_sets_the_sampled_share() {
        let sampler = Sampler::new(0.1);

        let sampled = (0..10_000u64)
            .filter(|n| sampler.samples(n))
            .count();

        // The hash is fixed, so this count is deterministic; the range only allows for the
        // hasher's distribution, not for randomness.
        assert!((800..=1_200).contains(&sampled), "sampled {sampled} of 10000");
    }

    /// A counter's name, its labels sorted by key, and its value.
    type Counter = (String, Vec<(String, String)>, u64);

    fn counters(snapshotter: &Snapshotter) -> Vec<Counter> {
        let mut counters: Vec<_> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(key, _, _, _)| key.kind() == MetricKind::Counter)
            .map(|(key, _, _, value)| {
                let mut labels: Vec<_> = key
                    .key()
                    .labels()
                    .map(|l| (l.key().to_string(), l.value().to_string()))
                    .collect();
                labels.sort();
                let DebugValue::Counter(count) = value else { panic!("not a counter") };
                (key.key().name().to_string(), labels, count)
            })
            .collect();
        counters.sort();
        counters
    }

    #[test]
    fn register_metrics_starts_every_series_at_zero() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();

        metrics::with_local_recorder(&recorder, || register_metrics(0.25));

        let counters = counters(&snapshotter);
        assert_eq!(counters.len(), Endpoint::ALL.len() * Outcome::ALL.len());
        assert!(counters
            .iter()
            .all(|(_, _, count)| *count == 0));
    }

    #[test]
    fn record_counts_the_outcome_once() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let version = dto::VersionParam::default();
        let sampled = SampledRequest {
            endpoint: Endpoint::ContractState,
            protocol_system: "ex",
            version: &version,
            id_count: 2,
        };
        let comparison = Comparison::mismatch(vec!["pagination.total: db=1 cache=2".to_string()]);

        metrics::with_local_recorder(&recorder, || {
            record(&sampled, &comparison, std::time::Duration::from_millis(1))
        });

        assert_eq!(
            counters(&snapshotter),
            vec![(
                "entity_cache_shadow_comparisons_total".to_string(),
                vec![
                    ("endpoint".to_string(), "contract_state".to_string()),
                    ("outcome".to_string(), "mismatch".to_string())
                ],
                1
            )]
        );
    }
}
