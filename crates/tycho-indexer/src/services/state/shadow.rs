//! Shadow mode: on a sample of state requests, runs the cache path next to the database path,
//! compares the two answers, and records the outcome. Clients always get the database answer.
//!
//! Both answers are normalized first: the differences we know about are erased from both sides.
//! What remains must be equal. Only when it is not does `locate` find where the answers differ,
//! through their JSON form, so it needs no code per response field.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    hash::{DefaultHasher, Hash, Hasher},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
    time::{Duration, Instant},
};

use metrics::{counter, gauge, histogram};
use serde::Serialize;
use serde_json::Value;
use tracing::warn;
use tycho_common::{dto, storage::StorageError, Bytes};

use super::service::{StateService, StateServiceError};
use crate::services::{deltas_buffer::PendingDeltasError, rpc::RpcError};

/// Most differences one comparison collects and logs.
const MAX_LOGGED_DIFFS: usize = 20;

/// Longest value a difference logs in full: a 32-byte word in hex. Longer values are logged as
/// their length and a hash.
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
    pub(crate) async fn run<R>(
        &self,
        request: SampledRequest<'_>,
        db: impl Future<Output = Result<R, RpcError>>,
        cache: impl FnOnce(&StateService) -> Result<R, StateServiceError>,
        compare: impl FnOnce(&StateService, &Result<R, RpcError>, CacheAnswer<R>) -> Comparison,
    ) -> Result<R, RpcError> {
        let before = self
            .service
            .straddle_token(request.protocol_system);
        let answer = db.await;
        let started = Instant::now();
        let cache = run_cache_path(|| cache(&self.service));
        let straddled = self
            .service
            .straddle_token(request.protocol_system) !=
            before;
        let comparison = if straddled && !matches!(cache, CacheAnswer::Panicked) {
            Comparison::without_diffs(Outcome::Discarded)
        } else {
            compare(&self.service, &answer, cache)
        };
        record(&request, &comparison, started.elapsed());
        answer
    }
}

/// What the cache path returned for a sampled request.
pub(crate) enum CacheAnswer<R> {
    Answered(R),
    /// The cache cannot answer the request; serve mode would ask the database path.
    Fallback,
    Failed(StateServiceError),
    Panicked,
}

/// Runs the cache path and turns a panic into [`CacheAnswer::Panicked`], so a cache bug cannot
/// fail the client's request.
fn run_cache_path<R>(read: impl FnOnce() -> Result<R, StateServiceError>) -> CacheAnswer<R> {
    match catch_unwind(AssertUnwindSafe(read)) {
        Ok(Ok(answer)) => CacheAnswer::Answered(answer),
        Ok(Err(StateServiceError::Fallback(_))) => CacheAnswer::Fallback,
        Ok(Err(err)) => CacheAnswer::Failed(err),
        Err(_) => CacheAnswer::Panicked,
    }
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
    fn without_diffs(outcome: Outcome) -> Self {
        Self { outcome, diffs: Vec::new() }
    }

    fn mismatch(diffs: Vec<String>) -> Self {
        Self { outcome: Outcome::Mismatch, diffs }
    }
}

/// Compares the two answers of a sampled `/contract_state` request.
pub(crate) fn compare_contract_state(
    db: &Result<dto::StateRequestResponse, RpcError>,
    cache: CacheAnswer<dto::StateRequestResponse>,
) -> Comparison {
    let (db, mut cache) = match pair(db, cache) {
        Ok(answers) => answers,
        Err(comparison) => return comparison,
    };
    // The client still gets the database answer as it is.
    let mut db = db.clone();
    normalize_accounts(&mut db.accounts);
    normalize_accounts(&mut cache.accounts);
    if db == cache {
        return Comparison::without_diffs(Outcome::Match);
    }
    let mut diffs = Vec::new();
    walk("pagination", &to_value(&db.pagination), &to_value(&cache.pagination), &mut diffs);
    locate(&db.accounts, &cache.accounts, |account| account.address.to_string(), &mut diffs);
    Comparison::mismatch(diffs)
}

/// Compares the two answers of a sampled `/protocol_state` request.
pub(crate) fn compare_protocol_state(
    include_balances: bool,
    db: &Result<dto::ProtocolStateRequestResponse, RpcError>,
    cache: CacheAnswer<dto::ProtocolStateRequestResponse>,
) -> Comparison {
    let (db, mut cache) = match pair(db, cache) {
        Ok(answers) => answers,
        Err(comparison) => return comparison,
    };
    // The client still gets the database answer as it is.
    let mut db = db.clone();
    normalize_states(&mut db.states, include_balances);
    normalize_states(&mut cache.states, include_balances);
    if db == cache {
        return Comparison::without_diffs(Outcome::Match);
    }
    let mut diffs = Vec::new();
    walk("pagination", &to_value(&db.pagination), &to_value(&cache.pagination), &mut diffs);
    locate(&db.states, &cache.states, |state| state.component_id.clone(), &mut diffs);
    Comparison::mismatch(diffs)
}

/// Erases the known account differences, then orders the accounts by address.
fn normalize_accounts(accounts: &mut [dto::ResponseAccount]) {
    for account in accounts.iter_mut() {
        // Folds and window deltas do not carry the transaction references.
        account.balance_modify_tx = Bytes::default();
        account.code_modify_tx = Bytes::default();
        // The database path does not recompute the hash when a window delta carries code (TODO in
        // `Account::apply_delta`). The code itself is compared.
        account.code_hash = Bytes::default();
    }
    accounts.sort_by(|a, b| a.address.cmp(&b.address));
}

/// Erases the known component differences, then orders the states by component id.
fn normalize_states(states: &mut Vec<dto::ResponseProtocolState>, include_balances: bool) {
    if !include_balances {
        // The database path applies window balances even when they are not requested.
        for state in states.iter_mut() {
            state.balances.clear();
        }
    }
    // The service serves an unknown component id as an empty state; the database path leaves it
    // out.
    states.retain(|state| !(state.attributes.is_empty() && state.balances.is_empty()));
    states.sort_by(|a, b| a.component_id.cmp(&b.component_id));
}

/// Returns both answers when both sides answered. Otherwise returns the comparison of the
/// failure.
fn pair<R>(db: &Result<R, RpcError>, cache: CacheAnswer<R>) -> Result<(&R, R), Comparison> {
    let err = match (db, cache) {
        (_, CacheAnswer::Fallback) => return Err(Comparison::without_diffs(Outcome::Fallback)),
        (_, CacheAnswer::Panicked) => return Err(Comparison::without_diffs(Outcome::CachePanicked)),
        (Err(db_err), _) if db_failed(db_err) => {
            return Err(Comparison::without_diffs(Outcome::DbFailed))
        }
        (Ok(db), CacheAnswer::Answered(cache)) => return Ok((db, cache)),
        (Err(db_err), CacheAnswer::Answered(_)) => {
            return Err(Comparison::mismatch(vec![format!("error: db={db_err} cache=ok")]))
        }
        (_, CacheAnswer::Failed(err)) => err,
    };
    let db_err = db.as_ref().err();
    if known_error(db_err, &err) {
        return Err(Comparison::without_diffs(Outcome::KnownError));
    }
    // Compare the error a client would get in `serve`.
    let cache_err = RpcError::from(err);
    let outcome = match db_err {
        Some(db_err) if db_err.to_string() == cache_err.to_string() => Outcome::Match,
        Some(db_err) => {
            return Err(Comparison::mismatch(vec![format!("error: db={db_err} cache={cache_err}")]))
        }
        None => return Err(Comparison::mismatch(vec![format!("error: db=ok cache={cache_err}")])),
    };
    Err(Comparison::without_diffs(outcome))
}

/// Whether the database-path error `db` is a failure of the database itself.
fn db_failed(db: &RpcError) -> bool {
    matches!(db, RpcError::Connection(_) | RpcError::Storage(StorageError::Unexpected(_)))
}

/// Whether the cache-path error `cache` is a known difference. `db` is the database-path error,
/// `None` when the database path answered.
fn known_error(db: Option<&RpcError>, cache: &StateServiceError) -> bool {
    match (db, cache) {
        // Neither path finds the account: the database path answers 500, the service 404.
        // TODO: answer a missing account with one status on both paths, then remove this case.
        (
            Some(RpcError::DeltasError(PendingDeltasError::ReorgBufferError(
                StorageError::NotFound(kind, id),
            ))),
            StateServiceError::ContractNotFound(address),
        ) => kind == "Contract" && *id == address.to_string(),
        _ => false,
    }
}

/// Adds where the entities of two normalized answers differ, as `id.path: db=… cache=…`, up to
/// [`MAX_LOGGED_DIFFS`]. An entity that compares equal is not serialized.
fn locate<T: Serialize + PartialEq>(
    db: &[T],
    cache: &[T],
    id: impl Fn(&T) -> String,
    diffs: &mut Vec<String>,
) {
    let db: BTreeMap<String, &T> = db
        .iter()
        .map(|entity| (id(entity), entity))
        .collect();
    let cache: BTreeMap<String, &T> = cache
        .iter()
        .map(|entity| (id(entity), entity))
        .collect();
    let ids: BTreeSet<&String> = db.keys().chain(cache.keys()).collect();
    for id in ids {
        if diffs.len() >= MAX_LOGGED_DIFFS {
            break;
        }
        let (db_entity, cache_entity) = (db.get(id), cache.get(id));
        if db_entity != cache_entity {
            let value =
                |entity: Option<&&T>| entity.map_or(Value::Null, |entity| to_value(*entity));
            walk(id, &value(db_entity), &value(cache_entity), diffs);
        }
    }
}

/// Adds `path: db=… cache=…` for each leaf where the two values differ. Objects are walked key by
/// key, in key order; any other value is a leaf. Stops at [`MAX_LOGGED_DIFFS`].
fn walk(path: &str, db: &Value, cache: &Value, diffs: &mut Vec<String>) {
    if diffs.len() >= MAX_LOGGED_DIFFS || db == cache {
        return;
    }
    static NULL: Value = Value::Null;
    if let (Value::Object(db), Value::Object(cache)) = (db, cache) {
        let keys: BTreeSet<&String> = db.keys().chain(cache.keys()).collect();
        for key in keys {
            let (db, cache) = (db.get(key).unwrap_or(&NULL), cache.get(key).unwrap_or(&NULL));
            walk(&format!("{path}.{key}"), db, cache, diffs);
        }
        return;
    }
    diffs.push(format!("{path}: db={} cache={}", render(db), render(cache)));
}

/// The JSON form of a response part. Never fails the request: a serialization error becomes the
/// value.
fn to_value(value: &impl Serialize) -> Value {
    serde_json::to_value(value)
        .unwrap_or_else(|err| Value::String(format!("unserializable: {err}")))
}

/// A value short enough to log: in full up to [`MAX_LOGGED_VALUE_LEN`] characters, otherwise its
/// length and a hash.
fn render(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    if text.len() <= MAX_LOGGED_VALUE_LEN {
        return text;
    }
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    format!("len={} hash={:016x}", text.len(), hasher.finish())
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
pub(crate) mod testing {
    use metrics_util::{
        debugging::{DebugValue, Snapshotter},
        MetricKind,
    };

    /// The comparison counters that moved, as `(endpoint, outcome, count)`, sorted.
    pub(crate) fn moved_comparisons(snapshotter: &Snapshotter) -> Vec<(String, String, u64)> {
        let mut moved: Vec<_> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(key, _, _, _)| {
                key.kind() == MetricKind::Counter &&
                    key.key().name() == "entity_cache_shadow_comparisons_total"
            })
            .filter_map(|(key, _, _, value)| {
                let DebugValue::Counter(count) = value else { return None };
                let label = |name: &str| {
                    key.key()
                        .labels()
                        .find(|l| l.key() == name)
                        .map(|l| l.value().to_string())
                        .unwrap_or_default()
                };
                (count > 0).then(|| (label("endpoint"), label("outcome"), count))
            })
            .collect();
        moved.sort();
        moved
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;

    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder, Snapshotter},
        MetricKind,
    };
    use rstest::rstest;
    use tycho_common::dto::PaginationResponse;

    use super::*;
    use crate::services::state::{
        cache::EntityCache,
        service::FallbackReason,
        window::{new_windows, WindowConfig},
    };

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

    fn address(n: u64) -> Bytes {
        Bytes::from(n).lpad(20, 0)
    }

    fn account(n: u64) -> dto::ResponseAccount {
        dto::ResponseAccount {
            address: address(n),
            slots: HashMap::from([(Bytes::from(1u64), Bytes::from(n))]),
            code: Bytes::from("0x6000"),
            ..Default::default()
        }
    }

    /// Both paths report `total` as the number of requested ids, not of returned entities.
    fn contracts(accounts: Vec<dto::ResponseAccount>, total: usize) -> dto::StateRequestResponse {
        dto::StateRequestResponse::new(accounts, PaginationResponse::new(0, 100, total as i64))
    }

    fn compare_contracts(
        db: Vec<dto::ResponseAccount>,
        cache: Vec<dto::ResponseAccount>,
    ) -> Comparison {
        compare_contract_state(&Ok(contracts(db, 2)), CacheAnswer::Answered(contracts(cache, 2)))
    }

    #[test]
    fn equal_answers_match_in_any_order() {
        let comparison =
            compare_contracts(vec![account(1), account(2)], vec![account(2), account(1)]);

        assert_eq!(comparison.outcome, Outcome::Match);
        assert!(comparison.diffs.is_empty());
    }

    #[test]
    fn a_corrupted_slot_is_located() {
        let mut corrupted = account(1);
        corrupted
            .slots
            .insert(Bytes::from(1u64), Bytes::from(99u64));

        let comparison = compare_contracts(vec![account(1)], vec![corrupted]);

        assert_eq!(comparison.outcome, Outcome::Mismatch);
        assert_eq!(
            comparison.diffs,
            vec![format!(
                "{}.slots.0x0000000000000001: db=0x0000000000000001 cache=0x0000000000000063",
                address(1)
            )]
        );
    }

    #[test]
    fn an_account_missing_from_the_cache_answer_is_located() {
        let comparison = compare_contracts(vec![account(1), account(2)], vec![account(1)]);

        assert_eq!(comparison.outcome, Outcome::Mismatch);
        assert_eq!(comparison.diffs.len(), 1);
        assert!(comparison.diffs[0].starts_with(&format!("{}: db=len=", address(2))));
        assert!(comparison.diffs[0].ends_with("cache=null"));
    }

    #[test]
    fn a_pagination_difference_is_located() {
        let comparison = compare_contract_state(
            &Ok(contracts(vec![account(1)], 1)),
            CacheAnswer::Answered(contracts(vec![account(1)], 2)),
        );

        assert_eq!(comparison.diffs, vec!["pagination.total: db=1 cache=2"]);
    }

    #[test]
    fn a_long_value_is_logged_as_its_length_and_hash() {
        let mut cache = account(1);
        cache.code = Bytes::from(vec![0x60; 100]);

        let comparison = compare_contracts(vec![account(1)], vec![cache]);

        assert_eq!(comparison.diffs.len(), 1);
        assert!(comparison.diffs[0]
            .starts_with(&format!("{}.code: db=0x6000 cache=len=202 hash=", address(1))));
    }

    #[test]
    fn locate_stops_at_the_most_logged_diffs() {
        let mut cache = account(1);
        cache.slots = (0..MAX_LOGGED_DIFFS as u64 + 5)
            .map(|n| (Bytes::from(n + 100), Bytes::from(n)))
            .collect();

        let comparison = compare_contracts(vec![account(1)], vec![cache]);

        assert_eq!(comparison.diffs.len(), MAX_LOGGED_DIFFS);
    }

    #[rstest]
    #[case::transaction_references(|account: &mut dto::ResponseAccount| {
        account.balance_modify_tx = Bytes::from(1u64);
        account.code_modify_tx = Bytes::from(2u64);
    })]
    #[case::stale_code_hash(|account: &mut dto::ResponseAccount| {
        account.code_hash = Bytes::from(7u64);
    })]
    fn known_account_differences_match(#[case] change: fn(&mut dto::ResponseAccount)) {
        let mut db = account(1);
        change(&mut db);

        let comparison = compare_contracts(vec![db], vec![account(1)]);

        assert_eq!(comparison.outcome, Outcome::Match);
    }

    #[rstest]
    #[case::cache_error_only(
        Ok(contracts(vec![account(1)], 1)),
        CacheAnswer::Failed(StateServiceError::InvalidVersion("bad".to_string())),
        Outcome::Mismatch
    )]
    #[case::account_only_the_database_path_finds(
        Ok(contracts(vec![account(1)], 1)),
        CacheAnswer::Failed(StateServiceError::ContractNotFound(address(1))),
        Outcome::Mismatch
    )]
    #[case::same_error_on_both(
        Err(RpcError::Parse("bad".to_string())),
        CacheAnswer::Failed(StateServiceError::InvalidVersion("bad".to_string())),
        Outcome::Match
    )]
    #[case::same_variant_other_error(
        Err(RpcError::Parse("bad".to_string())),
        CacheAnswer::Failed(StateServiceError::InvalidVersion("worse".to_string())),
        Outcome::Mismatch
    )]
    #[case::account_missing_on_both(
        Err(RpcError::DeltasError(PendingDeltasError::ReorgBufferError(StorageError::NotFound(
            "Contract".to_string(),
            address(1).to_string(),
        )))),
        CacheAnswer::Failed(StateServiceError::ContractNotFound(address(1))),
        Outcome::KnownError
    )]
    #[case::different_accounts_missing(
        Err(RpcError::DeltasError(PendingDeltasError::ReorgBufferError(StorageError::NotFound(
            "Contract".to_string(),
            address(1).to_string(),
        )))),
        CacheAnswer::Failed(StateServiceError::ContractNotFound(address(2))),
        Outcome::Mismatch
    )]
    #[case::database_fails_unexpectedly(
        Err(RpcError::Storage(StorageError::Unexpected("db down".to_string()))),
        CacheAnswer::Answered(contracts(vec![account(1)], 1)),
        Outcome::DbFailed
    )]
    #[case::database_fails_unexpectedly_and_the_cache_fails(
        Err(RpcError::Storage(StorageError::Unexpected("db down".to_string()))),
        CacheAnswer::Failed(StateServiceError::InvalidVersion("bad".to_string())),
        Outcome::DbFailed
    )]
    #[case::only_the_database_fails(
        Err(RpcError::Unknown("boom".to_string())),
        CacheAnswer::Answered(contracts(vec![account(1)], 1)),
        Outcome::Mismatch
    )]
    #[case::fallback(Ok(contracts(vec![account(1)], 1)), CacheAnswer::Fallback, Outcome::Fallback)]
    #[case::panicked(Ok(contracts(vec![account(1)], 1)), CacheAnswer::Panicked, Outcome::CachePanicked)]
    fn errors_classify_by_side(
        #[case] db: Result<dto::StateRequestResponse, RpcError>,
        #[case] cache: CacheAnswer<dto::StateRequestResponse>,
        #[case] expected: Outcome,
    ) {
        let comparison = compare_contract_state(&db, cache);

        assert_eq!(comparison.outcome, expected);
    }

    #[test]
    fn an_error_mismatch_logs_both_errors() {
        let comparison = compare_contract_state(
            &Ok(contracts(vec![account(1)], 1)),
            CacheAnswer::Failed(StateServiceError::InvalidVersion("boom".to_string())),
        );

        assert_eq!(comparison.diffs, vec!["error: db=ok cache=Failed to parse JSON: boom"]);
    }

    #[test]
    fn run_cache_path_turns_a_panic_into_panicked() {
        let answer = run_cache_path::<()>(|| panic!("cache path bug"));

        assert!(matches!(answer, CacheAnswer::Panicked));
    }

    #[test]
    fn run_cache_path_maps_the_service_errors() {
        assert!(matches!(
            run_cache_path::<()>(|| Err(StateServiceError::Fallback(FallbackReason::BelowWindow))),
            CacheAnswer::Fallback
        ));
        assert!(matches!(
            run_cache_path::<()>(|| Err(StateServiceError::ContractNotFound(Bytes::from(1u64)))),
            CacheAnswer::Failed(StateServiceError::ContractNotFound(_))
        ));
        assert!(matches!(run_cache_path(|| Ok(1)), CacheAnswer::Answered(1)));
    }

    fn state(id: &str, x: u64) -> dto::ResponseProtocolState {
        dto::ResponseProtocolState {
            component_id: id.to_string(),
            attributes: HashMap::from([("x".to_string(), Bytes::from(x))]),
            balances: HashMap::from([(address(9), Bytes::from(1u64))]),
        }
    }

    fn states(
        states: Vec<dto::ResponseProtocolState>,
        total: usize,
    ) -> dto::ProtocolStateRequestResponse {
        dto::ProtocolStateRequestResponse::new(
            states,
            PaginationResponse::new(0, 100, total as i64),
        )
    }

    fn compare_states(
        include_balances: bool,
        db: Vec<dto::ResponseProtocolState>,
        cache: Vec<dto::ResponseProtocolState>,
    ) -> Comparison {
        compare_protocol_state(
            include_balances,
            &Ok(states(db, 2)),
            CacheAnswer::Answered(states(cache, 2)),
        )
    }

    #[test]
    fn equal_states_match_in_any_order() {
        let comparison = compare_states(
            true,
            vec![state("c1", 1), state("c2", 2)],
            vec![state("c2", 2), state("c1", 1)],
        );

        assert_eq!(comparison.outcome, Outcome::Match);
    }

    #[test]
    fn a_corrupted_attribute_is_located() {
        let comparison = compare_states(true, vec![state("c1", 1)], vec![state("c1", 2)]);

        assert_eq!(comparison.outcome, Outcome::Mismatch);
        assert_eq!(
            comparison.diffs,
            vec!["c1.attributes.x: db=0x0000000000000001 cache=0x0000000000000002"]
        );
    }

    #[test]
    fn an_empty_state_only_the_cache_serves_matches() {
        let empty =
            dto::ResponseProtocolState { component_id: "c2".to_string(), ..Default::default() };

        let comparison = compare_states(true, vec![state("c1", 1)], vec![state("c1", 1), empty]);

        assert_eq!(comparison.outcome, Outcome::Match);
    }

    #[test]
    fn a_non_empty_state_only_the_cache_serves_is_located() {
        let comparison =
            compare_states(true, vec![state("c1", 1)], vec![state("c1", 1), state("c3", 3)]);

        assert_eq!(comparison.outcome, Outcome::Mismatch);
        assert!(comparison.diffs[0].starts_with("c3: db=null cache="));
    }

    #[rstest]
    #[case::balances_not_requested(false, Outcome::Match)]
    #[case::balances_requested(true, Outcome::Mismatch)]
    fn balances_only_the_database_serves(
        #[case] include_balances: bool,
        #[case] expected: Outcome,
    ) {
        let mut without_balances = state("c1", 1);
        without_balances.balances.clear();

        let comparison =
            compare_states(include_balances, vec![state("c1", 1)], vec![without_balances]);

        assert_eq!(comparison.outcome, expected);
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

    #[tokio::test]
    async fn run_reports_a_panic_that_straddled_a_change() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _recorder = metrics::set_default_local_recorder(&recorder);
        let windows = new_windows(["ex"], WindowConfig::default());
        let window = windows["ex"].clone();
        let shadow = Shadow::new(
            Arc::new(StateService::new(windows, Arc::new(EntityCache::new()))),
            Sampler::new(1.0),
        );
        let version = dto::VersionParam::default();
        let request = SampledRequest {
            endpoint: Endpoint::ContractState,
            protocol_system: "ex",
            version: &version,
            id_count: 1,
        };

        let answer = shadow
            .run(
                request,
                async { Ok(contracts(vec![account(1)], 1)) },
                |_| -> Result<dto::StateRequestResponse, StateServiceError> {
                    let _guard = window.lock().unwrap();
                    panic!("cache path bug");
                },
                |_, db, cache| compare_contract_state(db, cache),
            )
            .await;

        assert!(answer.is_ok());
        assert_eq!(
            testing::moved_comparisons(&snapshotter),
            vec![("contract_state".to_string(), "cache_panicked".to_string(), 1)]
        );
    }
}
