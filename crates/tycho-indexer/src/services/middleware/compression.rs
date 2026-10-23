use std::{
    task::Poll,
    time::{Duration, Instant},
};

use actix_web::{
    body::{BodyStream, EitherBody, MessageBody},
    dev::{ServiceRequest, ServiceResponse},
    http::header::CONTENT_ENCODING,
    middleware::{Compress, Next},
};
use futures03::Stream;

/// Creates the compression middleware for RPC responses.
///
/// Enables zstd compression while being backwards compatible with clients that do not support it.
/// The middleware automatically handles Accept-Encoding negotiation:
/// - Clients that send "Accept-Encoding: zstd" will receive compressed responses
/// - Clients that don't specify Accept-Encoding will receive uncompressed responses
/// - Clients that explicitly send "Accept-Encoding: identity" will receive uncompressed responses
pub(in crate::services) fn compression_middleware() -> Compress {
    Compress::default()
}

/// Measures compressed response bodies as they are sent: the time a body waits for its
/// compression job in `rpc_response_encode_wait_ms{endpoint}`, and its size in
/// `rpc_response_wire_bytes{endpoint}`. Must wrap the compression middleware.
///
/// Our bodies are complete bytes and the server polls a body only when it can write, so a body
/// that is not ready waits for compression. A compressed body has no known size either way, so
/// streaming it through the measurement sends the same bytes the same way.
pub(in crate::services) async fn compression_metrics_middleware<B>(
    req: ServiceRequest,
    next: Next<B>,
) -> Result<ServiceResponse<impl MessageBody>, actix_web::Error>
where
    B: MessageBody + 'static,
{
    let res = next.call(req).await?;
    let compressed = res
        .headers()
        .contains_key(CONTENT_ENCODING);
    let endpoint = res
        .request()
        .match_pattern()
        .unwrap_or_else(|| "unmatched".to_owned());
    Ok(res.map_body(move |_, body| {
        if compressed {
            EitherBody::left(BodyStream::new(measured(body, endpoint)))
        } else {
            EitherBody::right(body)
        }
    }))
}

fn measured<B>(
    body: B,
    endpoint: String,
) -> impl Stream<Item = Result<actix_web::web::Bytes, B::Error>>
where
    B: MessageBody + 'static,
{
    let mut body = Box::pin(body);
    let mut pending_since = None;
    let mut waited = Duration::ZERO;
    let mut bytes = 0;
    futures03::stream::poll_fn(move |cx| {
        let poll = body.as_mut().poll_next(cx);
        match &poll {
            Poll::Pending => {
                pending_since.get_or_insert_with(Instant::now);
            }
            Poll::Ready(chunk) => {
                if let Some(since) = pending_since.take() {
                    waited += since.elapsed();
                }
                match chunk {
                    Some(Ok(chunk)) => bytes += chunk.len(),
                    Some(Err(_)) => {}
                    None => {
                        metrics::histogram!("rpc_response_encode_wait_ms", "endpoint" => endpoint.clone())
                            .record(waited.as_secs_f64() * 1000.0);
                        metrics::histogram!("rpc_response_wire_bytes", "endpoint" => endpoint.clone())
                            .record(bytes as f64);
                    }
                }
            }
        }
        poll
    })
}

#[cfg(test)]
mod tests {
    use actix_web::{test, web, App};

    use super::*;
    use crate::services::rpc::health as health_endpoint;

    #[actix_web::test]
    async fn test_without_accept_encoding_header() {
        let app = test::init_service(
            App::new()
                .wrap(compression_middleware())
                .service(web::resource("/health").route(web::get().to(health_endpoint))),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/health")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        assert!(resp
            .headers()
            .get("content-encoding")
            .is_none());

        let body = test::read_body(resp).await;
        assert!(!body.is_empty());
    }

    async fn large_json() -> actix_web::HttpResponse {
        let slots: Vec<String> = (0..20_000)
            .map(|i| format!("0x{i:064x}"))
            .collect();
        actix_web::HttpResponse::Ok().json(slots)
    }

    #[actix_web::test]
    async fn test_metrics_middleware_leaves_responses_unchanged() {
        let plain = test::init_service(
            App::new()
                .wrap(compression_middleware())
                .route("/large", web::get().to(large_json)),
        )
        .await;
        let measured = test::init_service(
            App::new()
                .wrap(compression_middleware())
                .wrap(actix_web::middleware::from_fn(compression_metrics_middleware))
                .route("/large", web::get().to(large_json)),
        )
        .await;

        for encoding in ["zstd", "identity"] {
            let request = || {
                test::TestRequest::get()
                    .uri("/large")
                    .insert_header(("accept-encoding", encoding))
                    .to_request()
            };
            let expected = test::call_service(&plain, request()).await;
            let actual = test::call_service(&measured, request()).await;

            assert_eq!(
                actual.headers().get("content-encoding"),
                expected
                    .headers()
                    .get("content-encoding"),
                "{encoding}"
            );
            assert_eq!(actual.response().body().size(), expected.response().body().size());
            assert_eq!(test::read_body(actual).await, test::read_body(expected).await);
        }
    }

    #[actix_web::test]
    async fn test_with_zstd_encoding() {
        let app = test::init_service(
            App::new()
                .wrap(compression_middleware())
                .service(web::resource("/health").route(web::get().to(health_endpoint))),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/health")
            .insert_header(("accept-encoding", "zstd"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let content_encoding = resp
            .headers()
            .get("content-encoding")
            .and_then(|h| h.to_str().ok());
        assert_eq!(content_encoding, Some("zstd"));

        let body = test::read_body(resp).await;
        assert!(!body.is_empty());
    }

    #[actix_web::test]
    async fn test_with_explicit_identity() {
        let app = test::init_service(
            App::new()
                .wrap(compression_middleware())
                .service(web::resource("/health").route(web::get().to(health_endpoint))),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/health")
            .insert_header(("accept-encoding", "identity"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        assert!(resp
            .headers()
            .get("content-encoding")
            .is_none());

        let body = test::read_body(resp).await;
        assert!(!body.is_empty());
    }
}
