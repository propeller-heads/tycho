//! Measures response bodies as the server sends them, after compression.
//!
//! The body is wrapped in [`MeteredBody`], which forwards every call unchanged. Per response it
//! records, labelled by `endpoint` and `encoding` (the `Content-Encoding` header, or `identity`):
//!
//! - `rpc_response_body_ms`: from the first poll of the body to its end.
//! - `rpc_response_encode_wait_ms`: the part of that time the body was not ready. Our response
//!   bodies are complete bytes and the server polls a body only when it can write, so a body that
//!   is not ready waits for its compression job.
//! - `rpc_response_wire_bytes`: bytes sent, after compression.
//!
//! A body dropped before its end, e.g. because the client disconnected, counts in
//! `rpc_response_aborted_total{endpoint}` instead.
use std::{
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use actix_web::{
    body::{BodySize, MessageBody},
    dev::{ServiceRequest, ServiceResponse},
    http::{header::CONTENT_ENCODING, StatusCode},
    middleware::Next,
    web::Bytes,
};

/// Wraps each response body in a [`MeteredBody`]. Websocket upgrades are passed through without
/// measurement: their body is the connection, not a response.
pub(in crate::services) async fn response_metrics_middleware<B>(
    req: ServiceRequest,
    next: Next<B>,
) -> Result<ServiceResponse<MeteredBody<B>>, actix_web::Error>
where
    B: MessageBody + Unpin + 'static,
{
    let res = next.call(req).await?;
    let recorder = (res.status() != StatusCode::SWITCHING_PROTOCOLS).then(|| {
        let endpoint = res
            .request()
            .match_pattern()
            .unwrap_or_else(|| "unmatched".to_owned());
        let encoding = res
            .headers()
            .get(CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("identity")
            .to_owned();
        BodyRecorder::new(endpoint, encoding)
    });
    Ok(res.map_body(|_, body| MeteredBody { inner: body, recorder }))
}

/// A response body that forwards every call to `inner` and measures it. Without a recorder it
/// only forwards.
pub(in crate::services) struct MeteredBody<B> {
    inner: B,
    recorder: Option<BodyRecorder>,
}

impl<B> MessageBody for MeteredBody<B>
where
    B: MessageBody + Unpin,
{
    type Error = B::Error;

    fn size(&self) -> BodySize {
        self.inner.size()
    }

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Self::Error>>> {
        let this = &mut *self;
        let poll = Pin::new(&mut this.inner).poll_next(cx);
        if let Some(recorder) = this.recorder.as_mut() {
            recorder.observe(&poll);
        }
        poll
    }

    fn try_into_bytes(self) -> Result<Bytes, Self> {
        let MeteredBody { inner, recorder } = self;
        match inner.try_into_bytes() {
            Ok(bytes) => {
                if let Some(mut recorder) = recorder {
                    recorder.complete(bytes.len());
                }
                Ok(bytes)
            }
            Err(inner) => Err(MeteredBody { inner, recorder }),
        }
    }
}

/// The measurements of one response body. Records them when dropped.
struct BodyRecorder {
    endpoint: String,
    encoding: String,
    first_poll: Option<Instant>,
    pending_since: Option<Instant>,
    encode_wait: Duration,
    bytes: u64,
    /// Time from the first poll to the end of the body, once the body ended.
    body_time: Option<Duration>,
}

impl BodyRecorder {
    fn new(endpoint: String, encoding: String) -> Self {
        Self {
            endpoint,
            encoding,
            first_poll: None,
            pending_since: None,
            encode_wait: Duration::ZERO,
            bytes: 0,
            body_time: None,
        }
    }

    /// Accounts for one poll of the body and its result.
    fn observe<E>(&mut self, poll: &Poll<Option<Result<Bytes, E>>>) {
        let now = Instant::now();
        let first_poll = *self.first_poll.get_or_insert(now);
        match poll {
            Poll::Pending => {
                self.pending_since.get_or_insert(now);
            }
            Poll::Ready(chunk) => {
                if let Some(since) = self.pending_since.take() {
                    self.encode_wait += now - since;
                }
                match chunk {
                    Some(Ok(bytes)) => self.bytes += bytes.len() as u64,
                    // An error ends the body without completing it; the drop counts it as
                    // aborted.
                    Some(Err(_)) => {}
                    None => self.body_time = Some(now - first_poll),
                }
            }
        }
    }

    /// Accounts for a body handed over as one piece of `len` bytes, without polling.
    fn complete(&mut self, len: usize) {
        self.bytes += len as u64;
        self.body_time = Some(Duration::ZERO);
    }
}

impl Drop for BodyRecorder {
    fn drop(&mut self) {
        let endpoint = std::mem::take(&mut self.endpoint);
        let encoding = std::mem::take(&mut self.encoding);
        let Some(body_time) = self.body_time else {
            metrics::counter!("rpc_response_aborted_total", "endpoint" => endpoint).increment(1);
            return;
        };
        let labels = [("endpoint", endpoint), ("encoding", encoding)];
        metrics::histogram!("rpc_response_body_ms", &labels)
            .record(body_time.as_secs_f64() * 1000.0);
        metrics::histogram!("rpc_response_encode_wait_ms", &labels)
            .record(self.encode_wait.as_secs_f64() * 1000.0);
        metrics::histogram!("rpc_response_wire_bytes", &labels).record(self.bytes as f64);
    }
}

#[cfg(test)]
mod tests {
    use actix_web::{
        body::BoxBody, http::header::ACCEPT_ENCODING, middleware, test as actix_test, web, App,
        HttpResponse,
    };
    use futures03::future::poll_fn;

    use super::*;
    use crate::services::middleware::compression_middleware;

    /// A body that is not ready on its first poll, then yields `chunks`, then ends.
    struct SlowBody {
        chunks: Vec<Bytes>,
        polled: bool,
    }

    impl MessageBody for SlowBody {
        type Error = actix_web::Error;

        fn size(&self) -> BodySize {
            BodySize::Stream
        }

        fn poll_next(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Bytes, Self::Error>>> {
            if !self.polled {
                self.polled = true;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            if self.chunks.is_empty() {
                return Poll::Ready(None);
            }
            Poll::Ready(Some(Ok(self.chunks.remove(0))))
        }
    }

    #[actix_web::test]
    async fn test_recorder_measures_a_body_that_waits() {
        let mut recorder = BodyRecorder::new("/v1/test".to_owned(), "zstd".to_owned());
        let mut body = SlowBody { chunks: vec![Bytes::from_static(b"abc"); 2], polled: false };

        loop {
            let poll = poll_fn(|cx| {
                let poll = Pin::new(&mut body).poll_next(cx);
                recorder.observe(&poll);
                Poll::Ready(poll)
            })
            .await;
            if let Poll::Ready(None) = poll {
                break;
            }
        }

        assert_eq!(recorder.bytes, 6);
        assert!(recorder.body_time.is_some(), "the body ended, so it is complete");
        assert!(recorder.encode_wait <= recorder.body_time.unwrap());
        assert!(recorder.pending_since.is_none());
    }

    #[test]
    fn test_recorder_without_end_counts_as_aborted() {
        let mut recorder = BodyRecorder::new("/v1/test".to_owned(), "identity".to_owned());
        recorder.observe::<actix_web::Error>(&Poll::Ready(Some(Ok(Bytes::from_static(b"a")))));

        assert!(recorder.body_time.is_none());
    }

    #[test]
    fn test_metered_body_forwards_size_and_bytes() {
        let body = MeteredBody {
            inner: BoxBody::new(Bytes::from_static(b"hello")),
            recorder: Some(BodyRecorder::new("/v1/test".to_owned(), "identity".to_owned())),
        };

        assert_eq!(body.size(), BodySize::Sized(5));
        assert_eq!(body.try_into_bytes().ok(), Some(Bytes::from_static(b"hello")));
    }

    async fn large_json() -> HttpResponse {
        let slots: Vec<String> = (0..20_000)
            .map(|i| format!("0x{i:064x}"))
            .collect();
        HttpResponse::Ok().json(slots)
    }

    /// The measured app must answer exactly like the app without the middleware.
    #[actix_web::test]
    async fn test_responses_are_unchanged() {
        let plain = actix_test::init_service(
            App::new()
                .wrap(compression_middleware())
                .route("/large", web::get().to(large_json)),
        )
        .await;
        let measured = actix_test::init_service(
            App::new()
                .wrap(compression_middleware())
                .wrap(middleware::from_fn(response_metrics_middleware))
                .route("/large", web::get().to(large_json)),
        )
        .await;

        for encoding in ["zstd", "identity"] {
            let request = || {
                actix_test::TestRequest::get()
                    .uri("/large")
                    .insert_header((ACCEPT_ENCODING, encoding))
                    .to_request()
            };
            let expected = actix_test::call_service(&plain, request()).await;
            let actual = actix_test::call_service(&measured, request()).await;

            assert_eq!(actual.status(), expected.status(), "{encoding}");
            assert_eq!(
                actual.headers().get(CONTENT_ENCODING),
                expected.headers().get(CONTENT_ENCODING),
                "{encoding}"
            );
            assert_eq!(
                actual.response().body().size(),
                expected.response().body().size(),
                "{encoding}"
            );
            let expected_body = actix_test::read_body(expected).await;
            let actual_body = actix_test::read_body(actual).await;
            assert!(expected_body.len() > 1024, "the body must be large enough to compress");
            assert_eq!(actual_body, expected_body, "{encoding}");
        }
    }
}
