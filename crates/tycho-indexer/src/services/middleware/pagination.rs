use tycho_common::dto::PaginationLimits;

use crate::services::rpc::RpcError;

/// Validates pagination: a page of at least 0, and a page size from 1 up to a compression-aware
/// maximum.
pub trait RequestPaginationValidation: PaginationLimits {
    fn validate_pagination(&self, req: &actix_web::HttpRequest) -> Result<(), RpcError> {
        let page = self.pagination().page;
        let page_size = self.pagination().page_size;
        if page < 0 || page_size < 1 {
            return Err(RpcError::InvalidPagination { page, page_size });
        }

        let supports_compression = req
            .headers()
            .get("accept-encoding")
            .and_then(|h| h.to_str().ok())
            .map(|enc| enc.contains("zstd"))
            .unwrap_or(false);

        let max_allowed = Self::effective_max_page_size(supports_compression);

        if page_size > max_allowed {
            Err(RpcError::Pagination(max_allowed as usize))
        } else {
            Ok(())
        }
    }
}

impl<T: PaginationLimits> RequestPaginationValidation for T {}

#[cfg(test)]
mod tests {
    use actix_web::test;
    use rstest::rstest;
    use tycho_common::dto::PaginationParams;

    use super::*;

    #[derive(Clone)]
    struct TestPaginationRequest {
        pagination: PaginationParams,
    }

    const MAX_PAGE_SIZE_UNCOMPRESSED: i64 = 100;
    const MAX_PAGE_SIZE_COMPRESSED: i64 = 200;

    impl PaginationLimits for TestPaginationRequest {
        const MAX_PAGE_SIZE_COMPRESSED: i64 = MAX_PAGE_SIZE_COMPRESSED;
        const MAX_PAGE_SIZE_UNCOMPRESSED: i64 = MAX_PAGE_SIZE_UNCOMPRESSED;

        fn pagination(&self) -> &PaginationParams {
            &self.pagination
        }
    }

    #[rstest]
    #[case::first_page(0, 1, true)]
    #[case::negative_page(-1, 2, false)]
    #[case::negative_page_size(0, -1, false)]
    #[case::zero_page_size(0, 0, false)]
    #[actix_web::test]
    async fn test_pagination_lower_bounds(
        #[case] page: i64,
        #[case] page_size: i64,
        #[case] should_pass: bool,
    ) {
        let request = TestPaginationRequest { pagination: PaginationParams { page, page_size } };
        let req = test::TestRequest::get().to_http_request();

        let result = request.validate_pagination(&req);

        if should_pass {
            assert!(result.is_ok(), "page {page}, page_size {page_size} should pass: {result:?}");
        } else {
            assert!(
                matches!(
                    result,
                    Err(RpcError::InvalidPagination { page: p, page_size: s }) if p == page && s == page_size
                ),
                "page {page}, page_size {page_size} should be rejected: {result:?}"
            );
        }
    }

    #[rstest]
    #[case::at_uncompressed_limit(MAX_PAGE_SIZE_UNCOMPRESSED, None, true)]
    #[case::over_uncompressed_limit(MAX_PAGE_SIZE_UNCOMPRESSED + 1, None, false)]
    #[case::over_uncompressed_with_zstd(MAX_PAGE_SIZE_UNCOMPRESSED + 1, Some("zstd"), true)]
    #[case::at_compressed_limit_with_zstd(MAX_PAGE_SIZE_COMPRESSED, Some("zstd"), true)]
    #[case::over_compressed_limit_with_zstd(MAX_PAGE_SIZE_COMPRESSED + 1, Some("zstd"), false)]
    #[case::over_uncompressed_with_gzip(MAX_PAGE_SIZE_UNCOMPRESSED + 1, Some("gzip"), false)]
    #[case::over_uncompressed_with_multiple_encodings(
        MAX_PAGE_SIZE_UNCOMPRESSED + 1,
        Some("gzip, zstd, br"),
        true
    )]
    #[actix_web::test]
    async fn test_pagination_validation(
        #[case] page_size: i64,
        #[case] accept_encoding: Option<&str>,
        #[case] should_pass: bool,
    ) {
        let request = TestPaginationRequest { pagination: PaginationParams { page: 0, page_size } };

        let mut req_builder = test::TestRequest::get();
        if let Some(encoding) = accept_encoding {
            req_builder = req_builder.insert_header(("accept-encoding", encoding));
        }
        let req = req_builder.to_http_request();

        let result = request.validate_pagination(&req);

        if should_pass {
            assert!(result.is_ok(), "Expected validation to pass for page_size={page_size}, encoding={accept_encoding:?}");
        } else {
            assert!(result.is_err(), "Expected validation to fail for page_size={page_size}, encoding={accept_encoding:?}");

            if let Err(RpcError::Pagination(limit)) = result {
                let has_zstd = accept_encoding.is_some_and(|e| e.contains("zstd"));
                let expected_limit = if has_zstd {
                    MAX_PAGE_SIZE_COMPRESSED as usize
                } else {
                    MAX_PAGE_SIZE_UNCOMPRESSED as usize
                };
                assert_eq!(limit, expected_limit, "Wrong limit reported");
            }
        }
    }
}
