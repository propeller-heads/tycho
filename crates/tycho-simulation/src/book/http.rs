//! The one-request shape every polling book source shares: send, require a success status, parse
//! the JSON body.

use reqwest::RequestBuilder;
use serde::de::DeserializeOwned;

use crate::book::errors::FeedError;

/// Sends `request` and parses its JSON body. A transport failure or a non-success status is a
/// `ConnectionError`, an unparseable body a `ParsingError`; `what` names the resource in both
/// (e.g. "Hashflow price levels").
pub async fn fetch_json<T: DeserializeOwned>(
    request: RequestBuilder,
    what: &str,
) -> Result<T, FeedError> {
    let response = request
        .send()
        .await
        .map_err(|e| FeedError::ConnectionError(format!("Failed to fetch {what}: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_default();
        return Err(FeedError::ConnectionError(format!("{what} HTTP error {status}: {body}")));
    }
    response
        .json()
        .await
        .map_err(|e| FeedError::ParsingError(format!("Failed to parse {what} response: {e}")))
}

/// A local HTTP/1.1 server for feed tests: answers every connection through `respond`, which
/// maps the request line (e.g. `GET /price-levels?chainId=1 HTTP/1.1`) to a status and a JSON
/// body. Serves until the runtime drops it.
#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        net::SocketAddr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    /// One canned HTTP response; `(status_line, body)` converts into it.
    pub(crate) struct MockResponse {
        pub status: &'static str,
        pub body: String,
        /// Wait before answering, for tests that exercise request deadlines.
        pub delay: Duration,
    }

    impl From<(&'static str, String)> for MockResponse {
        fn from((status, body): (&'static str, String)) -> Self {
            MockResponse { status, body, delay: Duration::ZERO }
        }
    }

    /// A running mock server: where it listens and how many requests it has answered.
    pub(crate) struct MockHttpServer {
        pub address: SocketAddr,
        pub requests: Arc<AtomicUsize>,
    }

    impl MockHttpServer {
        pub fn url(&self) -> String {
            format!("http://{}", self.address)
        }

        pub fn request_count(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    /// Serves HTTP/1.1 requests until the runtime drops it. `route` maps the request target
    /// (path plus query string) to a response; a target it returns `None` for gets a 404. Each
    /// request is counted before `route` runs, so a route closure holding its own counter can
    /// answer per attempt.
    pub(crate) async fn spawn_http_server<R: Into<MockResponse>>(
        route: impl Fn(&str) -> Option<R> + Send + 'static,
    ) -> MockHttpServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let served = Arc::clone(&requests);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                if reader
                    .read_line(&mut request_line)
                    .await
                    .is_err()
                {
                    continue;
                }
                served.fetch_add(1, Ordering::SeqCst);
                let target = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default();
                let response = route(target)
                    .map(Into::into)
                    .unwrap_or_else(|| {
                        MockResponse::from(("404 Not Found", format!("no route for {target}")))
                    });
                if !response.delay.is_zero() {
                    tokio::time::sleep(response.delay).await;
                }
                let MockResponse { status, body, .. } = response;
                let payload = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let mut stream = reader.into_inner();
                let _ = stream
                    .write_all(payload.as_bytes())
                    .await;
                let _ = stream.shutdown().await;
            }
        });
        MockHttpServer { address, requests }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde::Deserialize;

    use super::{test_support::spawn_http_server, *};

    #[derive(Debug, Deserialize, PartialEq)]
    struct Payload {
        value: u32,
    }

    #[tokio::test]
    async fn parses_a_successful_json_body() {
        let server = spawn_http_server(|_| Some(("200 OK", r#"{"value":7}"#.to_string()))).await;

        let payload: Payload =
            fetch_json(reqwest::Client::new().get(format!("{}/x", server.url())), "thing")
                .await
                .unwrap();

        assert_eq!(payload, Payload { value: 7 });
    }

    /// A transport failure or a non-success status is a connection error naming the resource
    /// (and, for a status, carrying it and the body); a body that is not the expected JSON is a
    /// parsing error.
    #[rstest]
    #[case::non_success_status_with_body("503 Service Unavailable", "maintenance", false)]
    #[case::unparseable_success_body("200 OK", "<html>", true)]
    #[tokio::test]
    async fn classifies_status_and_body_failures(
        #[case] status: &'static str,
        #[case] body: &'static str,
        #[case] expect_parsing_error: bool,
    ) {
        let server = spawn_http_server(move |_| Some((status, body.to_string()))).await;

        let result: Result<Payload, FeedError> =
            fetch_json(reqwest::Client::new().get(format!("{}/x", server.url())), "thing").await;

        match result {
            Err(FeedError::ParsingError(msg)) if expect_parsing_error => {
                assert!(msg.contains("thing"), "{msg}")
            }
            Err(FeedError::ConnectionError(msg)) if !expect_parsing_error => {
                assert!(msg.contains("thing") && msg.contains("503") && msg.contains(body), "{msg}")
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn refused_connection_is_a_connection_error() {
        // Bind and drop so the port is known to be closed.
        let address = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();

        let result: Result<Payload, FeedError> =
            fetch_json(reqwest::Client::new().get(format!("http://{address}/x")), "thing").await;

        assert!(matches!(result, Err(FeedError::ConnectionError(msg)) if msg.contains("thing")));
    }
}
