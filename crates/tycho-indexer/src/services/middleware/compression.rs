use actix_web::middleware::Compress;

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

#[cfg(test)]
mod tests {
    use actix_web::{test, web, App, HttpResponse};

    use super::*;

    async fn health_endpoint() -> HttpResponse {
        HttpResponse::Ok().json(tycho_common::dto::Health::Ready)
    }

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
