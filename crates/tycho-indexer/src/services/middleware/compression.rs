use actix_web::{
    body::{BoxBody, MessageBody},
    http::header::{self, AcceptEncoding, Encoding, HeaderValue},
    middleware::Compress,
    HttpMessage, HttpRequest, HttpResponse,
};

use crate::services::rpc::RpcError;

/// Responses smaller than this are left to [`compression_middleware`]; compressing them costs
/// little at any level.
const OWN_ZSTD_MIN_BYTES: usize = 256 * 1024;

/// zstd level of [`compress_large_response`]. On state responses, level 1 takes about half the CPU
/// time of level 3, the level the middleware uses, and its output is no larger: in hex-heavy JSON
/// the 5-byte matches level 3 also takes cost more bits than the bytes they replace.
const OWN_ZSTD_LEVEL: i32 = 1;

/// Creates the compression middleware for RPC responses.
///
/// Enables zstd compression while being backwards compatible with clients that do not support it.
/// The middleware automatically handles Accept-Encoding negotiation:
/// - Clients that send "Accept-Encoding: zstd" will receive compressed responses
/// - Clients that don't specify Accept-Encoding will receive uncompressed responses
/// - Clients that explicitly send "Accept-Encoding: identity" will receive uncompressed responses
///
/// It compresses at zstd level 3 and leaves responses that already carry a `Content-Encoding`
/// unchanged, such as those of [`compress_large_response`].
pub(in crate::services) fn compression_middleware() -> Compress {
    Compress::default()
}

/// Compresses `response` with zstd at [`OWN_ZSTD_LEVEL`] on the blocking thread pool if the client
/// accepts zstd and the body is a complete body of at least [`OWN_ZSTD_MIN_BYTES`]. The response
/// then carries `Content-Encoding: zstd`, so [`compression_middleware`] sends it as it is. Any
/// other response is returned unchanged, for the middleware to handle.
///
/// # Errors
///
/// Returns [`RpcError::Unknown`] if compression fails or panics.
pub(in crate::services) async fn compress_large_response(
    req: &HttpRequest,
    response: HttpResponse,
) -> Result<HttpResponse, RpcError> {
    if !accepts_zstd(req) {
        return Ok(response);
    }
    let (response, body) = response.into_parts();
    let body = match body.try_into_bytes() {
        Ok(bytes) if bytes.len() >= OWN_ZSTD_MIN_BYTES => bytes,
        Ok(bytes) => return Ok(response.set_body(BoxBody::new(bytes))),
        Err(body) => return Ok(response.set_body(body)),
    };
    let compressed =
        tokio::task::spawn_blocking(move || zstd::bulk::compress(&body, OWN_ZSTD_LEVEL))
            .await
            .map_err(|err| RpcError::Unknown(format!("Response compression panicked: {err}")))?
            .map_err(|err| RpcError::Unknown(format!("Failed to compress response: {err}")))?;
    let mut response = response.set_body(BoxBody::new(compressed));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("zstd"));
    headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    Ok(response)
}

/// Whether the client accepts zstd, decided as [`compression_middleware`] decides: a request
/// without `Accept-Encoding` gets identity.
fn accepts_zstd(req: &HttpRequest) -> bool {
    req.get_header::<AcceptEncoding>()
        .and_then(|accept| accept.negotiate([Encoding::identity(), Encoding::zstd()].iter())) ==
        Some(Encoding::zstd())
}

#[cfg(test)]
mod tests {
    use actix_web::{http::header::ContentType, test, web, App};
    use rstest::rstest;

    use super::*;
    use crate::services::rpc::health as health_endpoint;

    /// A JSON body of `bytes` bytes, hex strings like a state response.
    fn hex_json(bytes: usize) -> Vec<u8> {
        let mut body = b"{\"slots\":{".to_vec();
        let mut i = 0u64;
        while body.len() < bytes {
            body.extend(format!("\"0x{i:064x}\":\"0x{:064x}\",", i * 7919).into_bytes());
            i += 1;
        }
        body.extend(b"\"0x\":\"0x\"}}");
        body
    }

    async fn state_like(
        req: HttpRequest,
        body: web::Data<Vec<u8>>,
    ) -> Result<HttpResponse, RpcError> {
        let response = HttpResponse::Ok()
            .content_type(ContentType::json())
            .body(body.get_ref().clone());
        compress_large_response(&req, response).await
    }

    /// Calls a handler that returns `body` through [`compress_large_response`] and the
    /// middleware, and returns the response's `Content-Encoding` and its body.
    async fn call_state_like(body: Vec<u8>, accept: Option<&str>) -> (Option<String>, Vec<u8>) {
        let app = test::init_service(
            App::new()
                .wrap(compression_middleware())
                .app_data(web::Data::new(body))
                .service(web::resource("/state").route(web::post().to(state_like))),
        )
        .await;
        let mut req = test::TestRequest::post().uri("/state");
        if let Some(accept) = accept {
            req = req.insert_header(("accept-encoding", accept));
        }
        let resp = test::call_service(&app, req.to_request()).await;
        assert!(resp.status().is_success());
        let encoding = resp
            .headers()
            .get("content-encoding")
            .map(|h| h.to_str().unwrap().to_string());
        (encoding, test::read_body(resp).await.to_vec())
    }

    #[rstest]
    #[case::large(OWN_ZSTD_MIN_BYTES)]
    #[case::small(OWN_ZSTD_MIN_BYTES - 1000)]
    #[actix_web::test]
    async fn test_zstd_body_decompresses_once_to_the_json(#[case] bytes: usize) {
        let json = hex_json(bytes);

        let (encoding, body) = call_state_like(json.clone(), Some("zstd")).await;

        assert_eq!(encoding.as_deref(), Some("zstd"));
        assert_eq!(zstd::decode_all(body.as_slice()).unwrap(), json);
    }

    #[rstest]
    #[case::no_header(None)]
    #[case::identity(Some("identity"))]
    #[case::gzip_only(Some("gzip"))]
    #[actix_web::test]
    async fn test_large_body_stays_uncompressed_without_zstd(#[case] accept: Option<&str>) {
        let json = hex_json(OWN_ZSTD_MIN_BYTES);

        let (encoding, body) = call_state_like(json.clone(), accept).await;

        assert_eq!(encoding, None);
        assert_eq!(body, json);
    }

    #[actix_web::test]
    async fn test_large_body_uses_own_level() {
        let json = hex_json(OWN_ZSTD_MIN_BYTES);

        let (_, body) = call_state_like(json.clone(), Some("zstd")).await;

        assert_eq!(body, zstd::bulk::compress(&json, OWN_ZSTD_LEVEL).unwrap());
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
