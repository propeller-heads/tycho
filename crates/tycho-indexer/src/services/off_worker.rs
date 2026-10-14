//! Runs the CPU-heavy steps of state responses off the actix request workers.
//!
//! A request worker serves all requests on its connections from one thread, so CPU-heavy work
//! that runs on it delays every other request it serves, health checks included. Building or
//! serializing a large state response can take seconds.
use std::{sync::Arc, time::Instant};

use actix_web::{http::header::ContentType, HttpResponse};
use serde::Serialize;
use tokio::sync::Semaphore;
use tycho_common::dto;

use crate::services::{rpc::RpcError, state::shadow::Endpoint};

/// Responses whose JSON is estimated below this many bytes are serialized on the request worker.
/// Serialization costs about 1.5 ns per output byte, so this is at most about 0.4 ms on the
/// worker; the hop to the blocking thread pool costs about 30 µs.
const OFF_WORKER_MIN_JSON_BYTES: usize = 256 * 1024;

/// JSON bytes of one storage slot or token balance: two hex strings of up to 32 bytes.
const JSON_BYTES_PER_SLOT: usize = 140;
/// JSON bytes of one protocol attribute or component balance: a short name or a token address,
/// and a short hex value.
const JSON_BYTES_PER_ATTRIBUTE: usize = 48;
/// JSON bytes of the fixed fields of one account, without its code.
const JSON_BYTES_PER_ACCOUNT: usize = 512;
/// JSON bytes of the fixed fields of one component state.
const JSON_BYTES_PER_COMPONENT: usize = 100;

/// Runs the CPU-heavy steps of state responses off the actix request workers. Each endpoint and
/// step (build, serialize) has its own pool of one permit per request worker, so a job never
/// waits behind jobs of another endpoint or step: a protocol state response, built in
/// milliseconds, does not queue behind contract state builds that take seconds, and a built
/// response does not queue behind builds that started after it.
pub(super) struct OffWorker {
    contract_state: EndpointPools,
    protocol_state: EndpointPools,
}

impl OffWorker {
    /// One permit per request worker for each endpoint and step; actix starts one worker per
    /// available CPU.
    pub(super) fn new() -> Self {
        let workers = std::thread::available_parallelism().map_or(1, usize::from);
        Self {
            contract_state: EndpointPools::new(Endpoint::ContractState, workers),
            protocol_state: EndpointPools::new(Endpoint::ProtocolState, workers),
        }
    }

    /// Runs `build` for `endpoint` on the blocking thread pool, in the caller's tracing span, and
    /// returns its result. The job keeps its permit until `build` returns, also when the caller
    /// stops waiting.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError::Unknown`] if `build` panics.
    pub(super) async fn build<R>(
        &self,
        endpoint: Endpoint,
        build: impl FnOnce() -> R + Send + 'static,
    ) -> Result<R, RpcError>
    where
        R: Send + 'static,
    {
        self.pools(endpoint)
            .build
            .run(build)
            .await
    }

    /// Answers `value`, a response of `endpoint`, as a `200 OK` JSON response, the same as
    /// [`HttpResponse::json`]. A response estimated at [`OFF_WORKER_MIN_JSON_BYTES`] or more is
    /// serialized on the blocking thread pool; a smaller one on the request worker.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError::Unknown`] if serialization fails or panics.
    pub(super) async fn json<V>(
        &self,
        endpoint: Endpoint,
        value: Arc<V>,
    ) -> Result<HttpResponse, RpcError>
    where
        V: Serialize + JsonSizeHint + Send + Sync + 'static,
    {
        if value.json_size_hint() < OFF_WORKER_MIN_JSON_BYTES {
            return Ok(HttpResponse::Ok().json(value));
        }
        let body = self
            .pools(endpoint)
            .serialize
            .run(move || serde_json::to_vec(value.as_ref()))
            .await?
            .map_err(|err| RpcError::Unknown(format!("Failed to serialize response: {err}")))?;
        Ok(HttpResponse::Ok()
            .content_type(ContentType::json())
            .body(body))
    }

    fn pools(&self, endpoint: Endpoint) -> &EndpointPools {
        match endpoint {
            Endpoint::ContractState => &self.contract_state,
            Endpoint::ProtocolState => &self.protocol_state,
        }
    }
}

/// The pools of one endpoint, one per step.
struct EndpointPools {
    build: Pool,
    serialize: Pool,
}

impl EndpointPools {
    fn new(endpoint: Endpoint, size: usize) -> Self {
        Self {
            build: Pool::new(endpoint, "build", size),
            serialize: Pool::new(endpoint, "serialize", size),
        }
    }
}

/// A bounded share of the blocking thread pool.
struct Pool {
    /// The `endpoint` and `step` labels of the `off_worker_permit_wait_ms` histogram.
    endpoint: Endpoint,
    step: &'static str,
    permits: Arc<Semaphore>,
}

impl Pool {
    fn new(endpoint: Endpoint, step: &'static str, size: usize) -> Self {
        Self { endpoint, step, permits: Arc::new(Semaphore::new(size)) }
    }

    /// Runs `work` on the blocking thread pool, in the caller's tracing span, once a permit is
    /// free, and returns its result. The job keeps its permit until `work` returns, also when the
    /// caller stops waiting. The wait for the permit is recorded in `off_worker_permit_wait_ms`.
    ///
    /// # Errors
    ///
    /// Returns [`RpcError::Unknown`] if `work` panics.
    async fn run<R>(&self, work: impl FnOnce() -> R + Send + 'static) -> Result<R, RpcError>
    where
        R: Send + 'static,
    {
        let wait_started = Instant::now();
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|err| RpcError::Unknown(format!("Off-worker permits closed: {err}")))?;
        metrics::histogram!(
            "off_worker_permit_wait_ms",
            "endpoint" => self.endpoint.label(),
            "step" => self.step
        )
        .record(wait_started.elapsed().as_secs_f64() * 1000.0);
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || {
            let result = span.in_scope(work);
            drop(permit);
            result
        })
        .await
        .map_err(|err| RpcError::Unknown(format!("Off-worker task failed: {err}")))
    }
}

/// An estimate of the JSON size of a response. Reads map and field lengths only, never map
/// entries, so it costs a few nanoseconds per account or component, however large their maps.
pub(super) trait JsonSizeHint {
    fn json_size_hint(&self) -> usize;
}

impl JsonSizeHint for dto::StateRequestResponse {
    fn json_size_hint(&self) -> usize {
        let mut size = 0;
        for account in &self.accounts {
            size += (account.slots.len() + account.token_balances.len()) * JSON_BYTES_PER_SLOT;
            // Code is a hex string: two characters per byte.
            size += 2 * account.code.len() + JSON_BYTES_PER_ACCOUNT;
        }
        size
    }
}

impl JsonSizeHint for dto::ProtocolStateRequestResponse {
    fn json_size_hint(&self) -> usize {
        let mut size = 0;
        for state in &self.states {
            size += (state.attributes.len() + state.balances.len()) * JSON_BYTES_PER_ATTRIBUTE;
            size += JSON_BYTES_PER_COMPONENT;
        }
        size
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::Duration,
    };

    use actix_web::http::StatusCode;
    use tycho_common::{dto::PaginationResponse, Bytes};

    use super::*;

    /// Waits until `pool` has `n` permits available, and fails after one second.
    async fn wait_for_available_permits(pool: &Pool, n: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.permits.available_permits() != n {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("permits were not released");
    }

    // `tokio::test` runs on one thread. Work that ran on it would block the release below until
    // `recv_timeout` gives up.
    #[tokio::test]
    async fn run_keeps_the_runtime_free_while_the_work_blocks() {
        let pool = Pool::new(Endpoint::ContractState, "test", 1);
        let (release, released) = mpsc::channel::<()>();

        let job = pool.run(move || {
            released
                .recv_timeout(Duration::from_secs(2))
                .is_ok()
        });
        let release = async move { release.send(()).unwrap() };
        let (released_in_time, ()) = tokio::join!(job, release);

        assert!(released_in_time.unwrap());
    }

    #[tokio::test]
    async fn run_keeps_the_permit_until_the_work_returns() {
        let pool = Pool::new(Endpoint::ContractState, "test", 1);
        let (release, released) = mpsc::channel::<()>();

        // Stop waiting while the work still runs, as the handler does when it is dropped.
        let gave_up = tokio::time::timeout(
            Duration::from_millis(50),
            pool.run(move || {
                released
                    .recv_timeout(Duration::from_secs(2))
                    .ok();
            }),
        )
        .await;

        assert!(gave_up.is_err());
        assert_eq!(pool.permits.available_permits(), 0);
        release.send(()).unwrap();
        wait_for_available_permits(&pool, 1).await;
    }

    #[tokio::test]
    async fn run_runs_no_more_jobs_than_permits() {
        let pool = Arc::new(Pool::new(Endpoint::ContractState, "test", 1));
        let (release, released) = mpsc::channel::<()>();
        let first = tokio::spawn({
            let pool = Arc::clone(&pool);
            async move {
                pool.run(move || {
                    released
                        .recv_timeout(Duration::from_secs(2))
                        .ok();
                })
                .await
            }
        });
        wait_for_available_permits(&pool, 0).await;

        let second_started = Arc::new(AtomicBool::new(false));
        let second = tokio::spawn({
            let pool = Arc::clone(&pool);
            let started = Arc::clone(&second_started);
            async move {
                pool.run(move || started.store(true, Ordering::SeqCst))
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(!second_started.load(Ordering::SeqCst));
        release.send(()).unwrap();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert!(second_started.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn run_reports_a_panicking_job_and_frees_its_permit() {
        let pool = Pool::new(Endpoint::ContractState, "test", 1);

        let result = pool
            .run(|| -> () { panic!("job failed") })
            .await;

        assert!(matches!(result, Err(RpcError::Unknown(_))));
        wait_for_available_permits(&pool, 1).await;
    }

    /// A 32-byte value, like a storage slot key or value.
    fn word(seed: u64) -> Bytes {
        let mut word = [0u8; 32];
        word[24..].copy_from_slice(&seed.to_be_bytes());
        Bytes::from(word)
    }

    /// A contract state response with one account, `n_slots` storage slots and `code_len` bytes
    /// of code.
    fn contract_state_response(n_slots: usize, code_len: usize) -> Arc<dto::StateRequestResponse> {
        let slots = (0..n_slots as u64)
            .map(|slot| (word(slot), word(u64::MAX - slot)))
            .collect();
        let account = dto::ResponseAccount {
            slots,
            code: Bytes::from(vec![0x60; code_len]),
            ..Default::default()
        };
        Arc::new(dto::StateRequestResponse::new(vec![account], PaginationResponse::new(0, 10, 1)))
    }

    /// The number of slots that puts a code-less account exactly at the threshold.
    fn slots_at_threshold() -> usize {
        (OFF_WORKER_MIN_JSON_BYTES - JSON_BYTES_PER_ACCOUNT).div_ceil(JSON_BYTES_PER_SLOT)
    }

    /// A protocol state response with `n_components` components of `n_attributes` tick
    /// attributes each. Tick values are short integers, as on uniswap v3 and v4 pools.
    fn protocol_state_response(
        n_components: usize,
        n_attributes: usize,
    ) -> dto::ProtocolStateRequestResponse {
        let states = (0..n_components)
            .map(|component| {
                let attributes = (0..n_attributes as i64)
                    .map(|tick| {
                        let liquidity = Bytes::from(1_000_000_007 * tick as u64);
                        (format!("ticks/{}/net-liquidity", tick * 60 - 887_220), liquidity)
                    })
                    .collect();
                dto::ResponseProtocolState {
                    component_id: format!("0x{component:040x}"),
                    attributes,
                    ..Default::default()
                }
            })
            .collect();
        dto::ProtocolStateRequestResponse::new(states, PaginationResponse::new(0, 10, 1))
    }

    #[test]
    fn json_size_hint_is_close_to_the_serialized_size() {
        let contract = contract_state_response(1_000, 24_000);
        let protocol = protocol_state_response(10, 200);

        for (hint, actual) in [
            (
                contract.json_size_hint(),
                serde_json::to_vec(contract.as_ref())
                    .unwrap()
                    .len(),
            ),
            (
                protocol.json_size_hint(),
                serde_json::to_vec(&protocol)
                    .unwrap()
                    .len(),
            ),
        ] {
            let ratio = hint as f64 / actual as f64;
            assert!((0.8..1.25).contains(&ratio), "hint {hint} for {actual} bytes");
        }
    }

    // A large response goes through the serialize pool; the body must still match.
    #[actix_web::test]
    async fn json_answers_like_http_response_json() {
        let off_worker = OffWorker::new();
        for value in
            [contract_state_response(10, 0), contract_state_response(slots_at_threshold(), 0)]
        {
            let expected = HttpResponse::Ok().json(Arc::clone(&value));

            let actual = off_worker
                .json(Endpoint::ContractState, value)
                .await
                .unwrap();

            assert_eq!(actual.status(), expected.status());
            let content_type = actix_web::http::header::CONTENT_TYPE;
            assert_eq!(actual.headers().get(&content_type), expected.headers().get(&content_type));
            let actual_body = actix_web::body::to_bytes(actual.into_body())
                .await
                .unwrap();
            let expected_body = actix_web::body::to_bytes(expected.into_body())
                .await
                .unwrap();
            assert_eq!(actual_body, expected_body);
        }
    }

    // The serialize pool has no permits, so only a response serialized on the worker can return.
    #[actix_web::test]
    async fn json_serializes_only_large_responses_off_the_worker() {
        let off_worker = OffWorker {
            contract_state: EndpointPools {
                build: Pool::new(Endpoint::ContractState, "build", 1),
                serialize: Pool::new(Endpoint::ContractState, "serialize", 0),
            },
            protocol_state: EndpointPools::new(Endpoint::ProtocolState, 1),
        };
        let below = contract_state_response(slots_at_threshold() - 1, 0);
        let at = contract_state_response(slots_at_threshold(), 0);
        assert!(below.json_size_hint() < OFF_WORKER_MIN_JSON_BYTES);
        assert!(at.json_size_hint() >= OFF_WORKER_MIN_JSON_BYTES);

        let wait = Duration::from_millis(50);
        let small =
            tokio::time::timeout(wait, off_worker.json(Endpoint::ContractState, below)).await;
        let large = tokio::time::timeout(wait, off_worker.json(Endpoint::ContractState, at)).await;

        let small = small.expect("a response below the threshold must not wait for a permit");
        assert_eq!(small.unwrap().status(), StatusCode::OK);
        assert!(large.is_err(), "a response at the threshold must wait for a permit");
    }

    // Every contract state pool is full, so only a job that does not queue behind contract state
    // jobs can return.
    #[actix_web::test]
    async fn protocol_state_jobs_do_not_wait_for_contract_state_permits() {
        let off_worker = OffWorker {
            contract_state: EndpointPools::new(Endpoint::ContractState, 0),
            protocol_state: EndpointPools::new(Endpoint::ProtocolState, 1),
        };
        let large = contract_state_response(slots_at_threshold(), 0);
        let wait = Duration::from_millis(50);

        let built =
            tokio::time::timeout(wait, off_worker.build(Endpoint::ProtocolState, || 1)).await;
        let contract_built =
            tokio::time::timeout(wait, off_worker.build(Endpoint::ContractState, || 1)).await;
        let contract_serialized =
            tokio::time::timeout(wait, off_worker.json(Endpoint::ContractState, large)).await;

        assert_eq!(
            built
                .expect("a protocol state build must not wait")
                .unwrap(),
            1
        );
        assert!(contract_built.is_err(), "a contract state build must wait for a permit");
        assert!(contract_serialized.is_err(), "a contract state serialization must wait");
    }
}
