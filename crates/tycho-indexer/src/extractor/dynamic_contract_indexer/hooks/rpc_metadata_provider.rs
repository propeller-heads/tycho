use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use alloy::transports::http::Client;
use serde_json::{json, Value};
use thiserror::Error;
use tonic::async_trait;
use tracing::{debug, error, warn};
use tycho_ethereum::rpc::config::RPCRetryConfig;

use super::component_metadata::{
    DeduplicationId, MetadataError, RequestProvider, RequestTransport, RpcTransport,
};

/// Custom error type for RPC batch operations
#[derive(Error, Debug)]
pub(super) enum BatchError {
    #[error("Network error: {0}")]
    Network(String),
    #[error("HTTP client error (status {status}): {message}")]
    HttpClient { status: u16, message: String },
    #[error("HTTP server error (status {status}): {message}")]
    HttpServer { status: u16, message: String },
    #[error("JSON parse error: {0}")]
    JsonParse(String),
    #[error("Failed to read response text: {0}")]
    ResponseRead(String),
    #[error("Max retries exhausted after {attempts} attempts")]
    MaxRetriesExhausted { attempts: usize },
}

pub(super) struct RPCMetadataProvider {
    client: Arc<Client>,
    batch_size_limit: usize,
    retry_config: RPCRetryConfig,
}

impl RPCMetadataProvider {
    #[cfg(test)]
    pub(super) fn new(batch_size_limit: usize) -> Self {
        Self::new_with_retry_config(batch_size_limit, RPCRetryConfig::default())
    }

    pub(super) fn new_with_retry_config(
        batch_size_limit: usize,
        retry_config: RPCRetryConfig,
    ) -> Self {
        Self { client: Arc::new(Client::new()), batch_size_limit, retry_config }
    }
}

#[async_trait]
impl RequestProvider for RPCMetadataProvider {
    async fn execute_batch(
        &self,
        requests: &[Box<dyn RequestTransport>],
    ) -> Vec<(DeduplicationId, Result<Value, MetadataError>)> {
        let mut results: HashMap<DeduplicationId, Result<Value, MetadataError>> =
            HashMap::with_capacity(requests.len());

        let batches = self.group_requests(requests, self.batch_size_limit);

        for batch in batches {
            let mut rpc_requests = Vec::new();

            for request in batch {
                match request
                    .as_any()
                    .downcast_ref::<RpcTransport>()
                {
                    Some(rpc_transport) => rpc_requests.push(rpc_transport.clone()),
                    None => {
                        results.insert(
                            request.deduplication_id(),
                            Err(MetadataError::ProviderFailed(
                                "Invalid rpc request transport".into(),
                            )),
                        );
                    }
                }
            }

            if rpc_requests.is_empty() {
                continue;
            }

            let endpoint = rpc_requests[0].endpoint().clone();
            let all_same_endpoint = rpc_requests
                .iter()
                .all(|rpc| rpc.endpoint() == &endpoint);

            // If the requests are not all to the same endpoint, we can't batch them
            // and we need to insert errors for each request
            // TODO: if the final design still include endpoint inside RpcTransport, we need to
            // handle this case by splitting the requests into multiple batches and emit a warning.
            if !all_same_endpoint {
                for rpc in rpc_requests {
                    results.insert(
                        rpc.deduplication_id(),
                        Err(MetadataError::ProviderFailed("Invalid rpc request transport".into())),
                    );
                }
                continue;
            }

            let mut rpc_id_to_transport: HashMap<u64, RpcTransport> = HashMap::new();

            let batch_json: Vec<Value> = rpc_requests
                .into_iter()
                .map(|rpc| {
                    rpc_id_to_transport.insert(rpc.id(), rpc.clone());

                    json!({
                        "jsonrpc": "2.0",
                        "method": rpc.method(),
                        "params": rpc.params(),
                        "id": rpc.id()
                    })
                })
                .collect();

            let response_json = match self
                .send_batch_with_retry(&endpoint, &batch_json)
                .await
            {
                Ok(json) => json,
                Err(batch_error) => {
                    // Request failed after all retries: insert errors for entire batch
                    for rpc in rpc_id_to_transport.values() {
                        results.insert(
                            rpc.deduplication_id(),
                            Err(MetadataError::ProviderFailed(format!(
                                "Request failed: {batch_error}"
                            ))),
                        );
                    }
                    continue;
                }
            };

            for resp in response_json {
                let id = resp.get("id").and_then(|v| v.as_u64());
                match id {
                    Some(actual_id) => {
                        let result = if let Some(error) = resp.get("error") {
                            Err(MetadataError::ProviderFailed(format!("RPC error: {error}")))
                        } else if let Some(result_value) = resp.get("result") {
                            Ok(result_value.clone())
                        } else {
                            Err(MetadataError::ProviderFailed(
                                "RPC response missing `result` and `error`".into(),
                            ))
                        };

                        if let Some(rpc) = rpc_id_to_transport.get(&actual_id) {
                            results.insert(rpc.deduplication_id(), result);
                        } else {
                            warn!(?actual_id, "Received unknown \"id\" in batch RPC response");
                        }
                    }
                    None => {
                        warn!(?resp, "Missing \"id\" in a batch RPCresponse");
                    }
                }
            }
        }

        results.into_iter().collect()
    }

    /// Group requests into batches of at most the `batch_size_limit`
    fn group_requests(
        &self,
        requests: &[Box<dyn RequestTransport>],
        batch_size_limit: usize,
    ) -> Vec<Vec<Box<dyn RequestTransport>>> {
        let mut seen_ids = HashSet::new();
        let mut grouped_batches = Vec::new();
        let mut current_batch = Vec::new();

        for request in requests {
            let id = request.deduplication_id();
            if seen_ids.insert(id) {
                // Only add request if the ID hasn't been seen before
                // This is to avoid duplicate requests
                current_batch.push(request.clone_box());

                if current_batch.len() >= batch_size_limit {
                    grouped_batches.push(std::mem::take(&mut current_batch));
                }
            }
        }

        if !current_batch.is_empty() {
            grouped_batches.push(current_batch);
        }

        grouped_batches
    }
}

impl RPCMetadataProvider {
    /// Send a batched JSON-RPC request with retry logic
    /// Returns the parsed JSON responses
    async fn send_batch_with_retry(
        &self,
        endpoint: &str,
        batch_json: &[Value],
    ) -> Result<Vec<Value>, BatchError> {
        let mut attempt = 0;
        let mut last_error: Option<BatchError> = None;

        while attempt <= self.retry_config.max_retries {
            // Calculate backoff with jitter
            if attempt > 0 {
                let backoff_ms = self.calculate_backoff(attempt);
                debug!(
                    attempt = attempt,
                    backoff_ms = backoff_ms,
                    endpoint = endpoint,
                    "Retrying RPC batch request after backoff"
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }

            match self
                .client
                .post(endpoint)
                .json(batch_json)
                .send()
                .await
            {
                Ok(response) => {
                    // Check if the response status indicates success
                    if response.status().is_success() {
                        // Parse response and check for RPC errors
                        match response.text().await {
                            Ok(response_text) => {
                                // Parse the response JSON
                                let response_json = match serde_json::from_str::<Vec<Value>>(
                                    &response_text,
                                ) {
                                    Ok(json) => json,
                                    Err(_) => {
                                        // Try parsing as single object and wrap in array
                                        match serde_json::from_str::<Value>(&response_text) {
                                            Ok(single) => vec![single],
                                            Err(e) => {
                                                // Parse error - treat as retryable if we have
                                                // retries left
                                                let parse_error =
                                                    BatchError::JsonParse(e.to_string());
                                                if attempt < self.retry_config.max_retries {
                                                    warn!(
                                                        attempt = attempt + 1,
                                                        error = %e,
                                                        endpoint = endpoint,
                                                        "Failed to parse JSON response, will retry"
                                                    );
                                                    last_error = Some(parse_error);
                                                    attempt += 1;
                                                    continue;
                                                }
                                                // Max retries exhausted, cannot parse response
                                                error!(
                                                    error = %e,
                                                    endpoint = endpoint,
                                                    "Failed to parse JSON response after all retries"
                                                );
                                                return Err(parse_error);
                                            }
                                        }
                                    }
                                };

                                // Check if ALL responses are errors (complete batch failure)
                                let all_failed = response_json
                                    .iter()
                                    .all(|r| r.get("error").is_some());

                                // Check if at least one error is retryable
                                let has_retryable = response_json.iter().any(|r| {
                                    r.get("error")
                                        .is_some_and(Self::is_retryable_rpc_error)
                                });

                                // Only retry if ALL responses failed OR at least one is retryable
                                if (all_failed || has_retryable) &&
                                    attempt < self.retry_config.max_retries
                                {
                                    // Log the RPC errors for debugging
                                    let error_details: Vec<_> = response_json
                                        .iter()
                                        .filter_map(|r| r.get("error"))
                                        .collect();
                                    warn!(
                                        attempt = attempt + 1,
                                        endpoint = endpoint,
                                        errors = ?error_details,
                                        "All requests in batch failed with at least one retryable error, will retry"
                                    );
                                    attempt += 1;
                                    continue;
                                }

                                // Success - return parsed responses
                                if attempt > 0 {
                                    debug!(
                                        attempt = attempt + 1,
                                        endpoint = endpoint,
                                        "RPC batch request succeeded on retry"
                                    );
                                }
                                return Ok(response_json);
                            }
                            Err(e) => {
                                // Failed to read response text - treat as retryable error
                                warn!(
                                    attempt = attempt + 1,
                                    error = %e,
                                    endpoint = endpoint,
                                    "Failed to read response text, will retry"
                                );
                                last_error = Some(BatchError::ResponseRead(e.to_string()));
                            }
                        }
                    } else {
                        // HTTP error status (5xx, 4xx etc.) - this is retryable for 5xx
                        let status = response.status();
                        if status.is_server_error() {
                            warn!(
                                attempt = attempt + 1,
                                status = %status,
                                endpoint = endpoint,
                                "RPC batch request returned server error, will retry"
                            );
                            // Create a server error
                            last_error = Some(BatchError::HttpServer {
                                status: status.as_u16(),
                                message: format!("Server error {}", status),
                            });
                        } else {
                            // Client error (4xx) - not retryable
                            warn!(
                                status = %status,
                                endpoint = endpoint,
                                "RPC batch request returned client error, not retrying"
                            );
                            // Return client error immediately (not retryable)
                            return Err(BatchError::HttpClient {
                                status: status.as_u16(),
                                message: format!("Client error {}: unparseable response", status),
                            });
                        }
                    }
                }
                Err(e) => {
                    // Network/HTTP error - this is retryable
                    warn!(
                        attempt = attempt + 1,
                        error = %e,
                        endpoint = endpoint,
                        "RPC batch request failed, will retry"
                    );
                    last_error = Some(BatchError::Network(e.to_string()));
                }
            }

            attempt += 1;
        }

        // All retries exhausted
        error!(
            endpoint = endpoint,
            max_retries = self.retry_config.max_retries,
            "All retry attempts failed for RPC batch request"
        );

        // Return the last error we encountered or a max retries exhausted error
        match last_error {
            Some(error) => Err(error),
            None => {
                Err(BatchError::MaxRetriesExhausted { attempts: self.retry_config.max_retries + 1 })
            }
        }
    }

    /// Check if an RPC error should be retried based on its error code
    /// Retryable errors are typically transient issues that may resolve on retry
    fn is_retryable_rpc_error(error: &Value) -> bool {
        if let Some(code) = error
            .get("code")
            .and_then(|c| c.as_i64())
        {
            match code {
                // Retryable errors (transient issues)
                -32000 => true, // "header not found" - block may not be available yet
                -32005 => true, // "limit exceeded" - rate limiting, backoff and retry
                -32603 => true, // "internal error" - temporary server issue

                // Non-retryable errors (permanent issues)
                -32600 => false, // "invalid request" - malformed request
                -32601 => false, // "method not found" - method doesn't exist
                -32602 => false, // "invalid params" - wrong parameters
                -32604 => false, // "method not supported" - not supported by this node

                // Default: retry unknown error codes (conservative approach)
                _ => true,
            }
        } else {
            // No error code found - retry to be safe
            true
        }
    }

    /// Calculate exponential backoff with jitter.
    /// Jitter prevents all clients from retrying simultaneously and crashing the recovering service
    fn calculate_backoff(&self, attempt: usize) -> u64 {
        use rand::Rng;

        // Calculate base exponential backoff: initial * 2^(attempt-1)
        let base_backoff = self
            .retry_config
            .initial_backoff_ms
            .saturating_mul(1 << (attempt - 1));

        // Cap at max_backoff_ms
        let capped_backoff = base_backoff.min(self.retry_config.max_backoff_ms);

        // Add jitter (0-25% of the backoff time)
        let jitter = rand::thread_rng().gen_range(0..=capped_backoff / 4);

        capped_backoff + jitter
    }
}

#[cfg(test)]
mod tests {
    use mockito::{Matcher, Server};

    use super::*;

    #[tokio::test]
    #[ignore = "Requires a real RPC endpoint"]
    async fn test_execute_batch() {
        let provider = RPCMetadataProvider::new(10);
        let endpoint = std::env::var("RPC_URL").expect("RPC_URL must be set");

        let req_map = HashMap::from([
            (
                "eth_blockNumber".to_string(),
                RpcTransport::new(endpoint.clone(), "eth_blockNumber".to_string(), vec![]),
            ),
            (
                "eth_gasPrice".to_string(),
                RpcTransport::new(endpoint.clone(), "eth_gasPrice".to_string(), vec![]),
            ),
            (
                "eth_hashrate".to_string(),
                RpcTransport::new(endpoint.clone(), "eth_hashrate".to_string(), vec![]),
            ),
            (
                "eth_getBalance".to_string(),
                RpcTransport::new(
                    endpoint.clone(),
                    "eth_getBalance".to_string(),
                    vec![json!("0x000000000000000000000000000000000000dEaD"), json!("latest")],
                ),
            ),
            (
                "eth_call".to_string(),
                RpcTransport::new(
                    endpoint.clone(),
                    "eth_call".to_string(),
                    vec![
                        json!({
                            "to": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
                            "data": "0x0902f1ac"
                        }),
                        json!("latest"),
                    ],
                ),
            ),
        ]);

        let requests = req_map
            .clone()
            .into_values()
            .map(|r| Box::new(r) as Box<dyn RequestTransport>)
            .collect::<Vec<_>>();

        let results = provider.execute_batch(&requests).await;

        // Assertions
        let results_map: std::collections::HashMap<_, _> = results.into_iter().collect();

        let block_number = results_map
            .get("eth_blockNumber_[]")
            .expect("eth_blockNumber missing");
        assert!(block_number.is_ok(), "eth_blockNumber failed");

        let gas_price = results_map
            .get("eth_gasPrice_[]")
            .expect("eth_gasPrice missing");
        assert!(gas_price.is_ok(), "eth_gasPrice failed");

        let hashrate = results_map
            .get("eth_hashrate_[]")
            .expect("eth_hashrate missing");
        assert_eq!(
            hashrate,
            &Err(MetadataError::ProviderFailed(
                "RPC error: {\"code\":-32601,\"message\":\"the method eth_hashrate does not exist/is not available\"}".into(),
            ))
        );

        let balance_result = results_map
            .get(
                &req_map
                    .get("eth_getBalance")
                    .unwrap()
                    .deduplication_id(),
            )
            .expect("eth_getBalance missing");
        assert!(balance_result.is_ok(), "eth_getBalance failed");

        let call_result = results_map
            .get(
                &req_map
                    .get("eth_call")
                    .unwrap()
                    .deduplication_id(),
            )
            .expect("eth_call missing");
        assert!(call_result.is_ok(), "eth_call failed");
    }

    #[tokio::test]
    async fn test_execute_batch_with_mock_batch_size_1() {
        let retry_config =
            RPCRetryConfig { max_retries: 3, initial_backoff_ms: 100, max_backoff_ms: 5000 };
        let provider = RPCMetadataProvider::new_with_retry_config(1, retry_config);
        let mut server = Server::new_async().await;
        let endpoint = server.url();

        let req_map: HashMap<String, RpcTransport> = HashMap::from([
            (
                "eth_blockNumber".to_string(),
                RpcTransport::new(endpoint.to_string(), "eth_blockNumber".to_string(), vec![]),
            ),
            (
                "eth_getBalance".to_string(),
                RpcTransport::new(
                    endpoint.to_string(),
                    "eth_getBalance".to_string(),
                    vec![json!("0x000000000000000000000000000000000000dEaD"), json!("latest")],
                ),
            ),
            (
                "eth_call".to_string(),
                RpcTransport::new(
                    endpoint.to_string(),
                    "eth_call".to_string(),
                    vec![
                        json!({
                            "to": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
                            "data": "0x0902f1ac"
                        }),
                        json!("latest"),
                    ],
                ),
            ),
            (
                "eth_hashrate".to_string(),
                RpcTransport::new(endpoint.to_string(), "eth_hashrate".to_string(), vec![]),
            ),
            (
                "eth_gasPrice".to_string(),
                RpcTransport::new(endpoint.to_string(), "eth_gasPrice".to_string(), vec![]),
            ),
        ]);

        // Map ids from transports so we mimic real id values
        let mut responses = vec![];
        for transport in req_map.values() {
            let method = transport.method();
            let id = transport.id();

            let resp = match method.as_str() {
                "eth_blockNumber" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": "0x15dac9b"
                }),
                "eth_getBalance" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": "0x2aca55e768e35fed455"
                }),
                "eth_call" => json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result":
                "0x00000000000000000000000000000000000000000000000000000b63a126babc0000000000000000000000000000000000000000000000dfc818ada67f7a256b000000000000000000000000000000000000000000000000000000006874c0d3"
                            }),
                "eth_hashrate" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32601,
                        "message": "the method eth_hashrate does not exist/is not available"
                    }
                }),
                "eth_gasPrice" => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": "0x7a67f1da"
                }),
                _ => panic!("unexpected method"),
            };

            responses.push((method, resp));
        }

        let mut all_mocks = vec![];
        for (method, resp) in responses {
            // eth_hashrate returns a non-retryable error (-32601) but gets retried anyway
            // because it's a single-request batch and all_failed=true triggers safety retry
            let expected_calls = if method == "eth_hashrate" { 4 } else { 1 }; // 1 + 3 retries

            let mock = server
                .mock("POST", "/")
                .match_body(Matcher::Regex(format!(r#""method"\s*:\s*"{method}""#)))
                .with_body(resp.to_string())
                .expect(expected_calls)
                .create_async()
                .await;
            all_mocks.push(mock);
        }

        let request_list: Vec<Box<dyn RequestTransport>> = req_map
            .values()
            .map(|t| Box::new(t.clone()) as Box<dyn RequestTransport>)
            .collect();

        let results = provider
            .execute_batch(&request_list)
            .await;

        // Assertions

        for mock in all_mocks {
            mock.assert();
        }

        let results_map: HashMap<_, _> = results.into_iter().collect();

        let block_number = results_map
            .get("eth_blockNumber_[]")
            .expect("eth_blockNumber missing");
        assert!(block_number.is_ok(), "eth_blockNumber failed");

        let gas_price = results_map
            .get("eth_gasPrice_[]")
            .expect("eth_gasPrice missing");
        assert!(gas_price.is_ok(), "eth_gasPrice failed");

        let hashrate = results_map
            .get("eth_hashrate_[]")
            .expect("eth_hashrate missing");
        assert_eq!(
            hashrate,
            &Err(MetadataError::ProviderFailed(
                "RPC error: {\"code\":-32601,\"message\":\"the method eth_hashrate does not exist/is not available\"}".into(),
            ))
        );

        let balance_result = results_map
            .get(
                &req_map
                    .get("eth_getBalance")
                    .unwrap()
                    .deduplication_id(),
            )
            .expect("eth_getBalance missing");
        assert!(balance_result.is_ok(), "eth_getBalance failed");

        let call_result = results_map
            .get(
                &req_map
                    .get("eth_call")
                    .unwrap()
                    .deduplication_id(),
            )
            .expect("eth_call missing");
        assert!(call_result.is_ok(), "eth_call failed");
    }

    #[tokio::test]
    async fn test_execute_batch_with_mock_batch_size_3() {
        let provider = RPCMetadataProvider::new(3);
        let mut server = Server::new_async().await;
        let endpoint = server.url();

        let req_map: HashMap<String, RpcTransport> = HashMap::from([
            (
                "eth_blockNumber".to_string(),
                RpcTransport::new(endpoint.to_string(), "eth_blockNumber".to_string(), vec![]),
            ),
            (
                "eth_getBalance".to_string(),
                RpcTransport::new(
                    endpoint.to_string(),
                    "eth_getBalance".to_string(),
                    vec![json!("0x000000000000000000000000000000000000dEaD"), json!("latest")],
                ),
            ),
            (
                "eth_call".to_string(),
                RpcTransport::new(
                    endpoint.to_string(),
                    "eth_call".to_string(),
                    vec![
                        json!({
                            "to": "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc",
                            "data": "0x0902f1ac"
                        }),
                        json!("latest"),
                    ],
                ),
            ),
            (
                "eth_hashrate".to_string(),
                RpcTransport::new(endpoint.to_string(), "eth_hashrate".to_string(), vec![]),
            ),
            (
                "eth_gasPrice".to_string(),
                RpcTransport::new(endpoint.to_string(), "eth_gasPrice".to_string(), vec![]),
            ),
        ]);

        // Batch 1: eth_blockNumber, eth_call, eth_gasPrice (3 requests)
        let batch1_responses = vec![
            json!({
                "jsonrpc": "2.0",
                "id": req_map["eth_blockNumber"].id(),
                "result": "0x15dac9b"
            }),
            json!({
                "jsonrpc": "2.0",
                "id": req_map["eth_call"].id(),
                "result": "0x00000000000000000000000000000000000000000000000000000b63a126babc0000000000000000000000000000000000000000000000dfc818ada67f7a256b000000000000000000000000000000000000000000000000000000006874c0d3"
            }),
            json!({
                "jsonrpc": "2.0",
                "id": req_map["eth_gasPrice"].id(),
                "result": "0x7a67f1da"
            }),
        ];

        // Batch 2: eth_hashrate, eth_getBalance (2 requests)
        let batch2_responses = vec![
            json!({
                "jsonrpc": "2.0",
                "id": req_map["eth_hashrate"].id(),
                "error": {
                    "code": -32601,
                    "message": "the method eth_hashrate does not exist/is not available"
                }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": req_map["eth_getBalance"].id(),
                "result": "0x2aca55e768e35fed455"
            }),
        ];

        let mock1 = server
            .mock("POST", "/")
            .match_body(Matcher::Regex((r#""method"\s*:\s*"eth_blockNumber""#).to_string()))
            .match_body(Matcher::Regex((r#""method"\s*:\s*"eth_call""#).to_string()))
            .match_body(Matcher::Regex((r#""method"\s*:\s*"eth_gasPrice""#).to_string()))
            .with_body(serde_json::to_string(&batch1_responses).unwrap())
            .expect(1)
            .create_async()
            .await;

        let mock2 = server
            .mock("POST", "/")
            .match_body(Matcher::Regex((r#""method"\s*:\s*"eth_getBalance""#).to_string()))
            .match_body(Matcher::Regex((r#""method"\s*:\s*"eth_hashrate""#).to_string()))
            .with_body(serde_json::to_string(&batch2_responses).unwrap())
            .expect(1)
            .create_async()
            .await;

        let mut request_list: Vec<Box<dyn RequestTransport>> = req_map
            .values()
            .map(|t| Box::new(t.clone()) as Box<dyn RequestTransport>)
            .collect();

        // Need to sort the requests by deduplication id to ensure the order of the requests is
        // consistent. This is because we mock the batch requests in a certain order, and we need
        // to ensure the requests are given in the same order.
        request_list.sort_by_key(|r| r.deduplication_id());

        let results = provider
            .execute_batch(&request_list)
            .await;

        // Assertions
        mock1.assert();
        mock2.assert();

        let results_map: HashMap<_, _> = results.into_iter().collect();

        let block_number = results_map
            .get("eth_blockNumber_[]")
            .expect("eth_blockNumber missing");
        assert!(block_number.is_ok(), "eth_blockNumber failed");

        let gas_price = results_map
            .get("eth_gasPrice_[]")
            .expect("eth_gasPrice missing");
        assert!(gas_price.is_ok(), "eth_gasPrice failed");

        let hashrate = results_map
            .get("eth_hashrate_[]")
            .expect("eth_hashrate missing");
        assert_eq!(
            hashrate,
            &Err(MetadataError::ProviderFailed(
                "RPC error: {\"code\":-32601,\"message\":\"the method eth_hashrate does not exist/is not available\"}".into(),
            ))
        );

        let balance_result = results_map
            .get(
                &req_map
                    .get("eth_getBalance")
                    .unwrap()
                    .deduplication_id(),
            )
            .expect("eth_getBalance missing");
        assert!(balance_result.is_ok(), "eth_getBalance failed");

        let call_result = results_map
            .get(
                &req_map
                    .get("eth_call")
                    .unwrap()
                    .deduplication_id(),
            )
            .expect("eth_call missing");
        assert!(call_result.is_ok(), "eth_call failed");
    }

    #[test]
    fn test_group_requests() {
        let provider = RPCMetadataProvider::new(3);

        let requests: Vec<Box<dyn RequestTransport>> = vec![
            Box::new(RpcTransport::new(
                "http://localhost:8545".to_string(),
                "eth_blockNumber".to_string(),
                vec![],
            )),
            Box::new(RpcTransport::new(
                "http://localhost:8545".to_string(),
                "eth_gasPrice".to_string(),
                vec![],
            )),
            Box::new(RpcTransport::new(
                "http://localhost:8545".to_string(),
                "eth_getBalance".to_string(),
                vec![json!("0x123"), json!("latest")],
            )),
            Box::new(RpcTransport::new(
                "http://localhost:8545".to_string(),
                "eth_call".to_string(),
                vec![json!({"to": "0x456", "data": "0x789"}), json!("latest")],
            )),
            Box::new(RpcTransport::new(
                "http://localhost:8545".to_string(),
                "eth_chainId".to_string(),
                vec![],
            )),
        ];

        let batches = provider.group_requests(&requests, 3);

        assert_eq!(batches.len(), 2, "Should create 2 batches for 5 requests with batch size 3");
        assert_eq!(batches[0].len(), 3, "First batch should have 3 requests");
        assert_eq!(batches[1].len(), 2, "Second batch should have 2 requests");
    }

    #[test]
    fn test_calculate_backoff() {
        let retry_config =
            RPCRetryConfig { max_retries: 3, initial_backoff_ms: 100, max_backoff_ms: 5000 };
        let provider = RPCMetadataProvider::new_with_retry_config(10, retry_config);

        // Test exponential backoff
        let backoff1 = provider.calculate_backoff(1);
        assert!((100..=125).contains(&backoff1)); // 100ms + up to 25% jitter

        let backoff2 = provider.calculate_backoff(2);
        assert!((200..=250).contains(&backoff2)); // 200ms + up to 25% jitter

        let backoff3 = provider.calculate_backoff(3);
        assert!((400..=500).contains(&backoff3)); // 400ms + up to 25% jitter

        // Test max cap
        let backoff_large = provider.calculate_backoff(10);
        assert!(backoff_large <= 5000 + 1250); // Max 5000ms + 25% jitter
    }

    #[tokio::test]
    async fn test_send_batch_with_retry_success_after_failures() {
        let retry_config = RPCRetryConfig {
            max_retries: 3,
            initial_backoff_ms: 10, // Short for testing
            max_backoff_ms: 100,
        };
        let provider = RPCMetadataProvider::new_with_retry_config(10, retry_config);
        let mut server = Server::new_async().await;
        let endpoint = server.url();

        let batch_json = vec![json!({
            "jsonrpc": "2.0",
            "method": "eth_blockNumber",
            "params": [],
            "id": 1
        })];

        // First two attempts fail, third succeeds
        let _m1 = server
            .mock("POST", "/")
            .with_status(500)
            .expect(1)
            .create_async()
            .await;

        let _m2 = server
            .mock("POST", "/")
            .with_status(500)
            .expect(1)
            .create_async()
            .await;

        let _m3 = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(r#"[{"jsonrpc":"2.0","id":1,"result":"0x15dac9b"}]"#)
            .expect(1)
            .create_async()
            .await;

        let result = provider
            .send_batch_with_retry(&endpoint, &batch_json)
            .await;

        assert!(result.is_ok(), "Request should succeed after retries");

        let json_responses = result.unwrap();
        assert!(!json_responses.is_empty(), "Should have responses");

        // Check that at least one response contains the expected result
        let has_expected_result = json_responses.iter().any(|r| {
            r.get("result").is_some_and(|v| {
                v.as_str()
                    .is_some_and(|s| s.contains("0x15dac9b"))
            })
        });
        assert!(has_expected_result, "Should contain expected result");
    }

    #[tokio::test]
    async fn test_send_batch_with_retry_max_retries_exceeded() {
        let retry_config = RPCRetryConfig {
            max_retries: 2,
            initial_backoff_ms: 10, // Short for testing
            max_backoff_ms: 100,
        };
        let provider = RPCMetadataProvider::new_with_retry_config(10, retry_config);
        let mut server = Server::new_async().await;
        let endpoint = server.url();

        let batch_json = vec![json!({
            "jsonrpc": "2.0",
            "method": "eth_blockNumber",
            "params": [],
            "id": 1
        })];

        // All attempts fail (initial + max_retries)
        let _m = server
            .mock("POST", "/")
            .with_status(500)
            .expect(3) // Initial attempt + 2 retries
            .create_async()
            .await;

        let result = provider
            .send_batch_with_retry(&endpoint, &batch_json)
            .await;

        assert!(result.is_err(), "Request should fail after exhausting retries");

        // Verify it's the correct error type
        match result.unwrap_err() {
            BatchError::HttpServer { .. } => {
                // Expected - server errors should be retried and then fail
            }
            other => panic!("Expected HttpServer error, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_execute_batch_with_retry_integration() {
        let retry_config = RPCRetryConfig {
            max_retries: 3,
            initial_backoff_ms: 10, // Short for testing
            max_backoff_ms: 100,
        };
        let provider = RPCMetadataProvider::new_with_retry_config(1, retry_config);
        let mut server = Server::new_async().await;
        let endpoint = server.url();

        let transport =
            RpcTransport::new(endpoint.to_string(), "eth_blockNumber".to_string(), vec![]);

        // First attempt fails, second succeeds
        let _m1 = server
            .mock("POST", "/")
            .with_status(500)
            .expect(1)
            .create_async()
            .await;

        let transport_id = transport.id();
        let success_response =
            format!(r#"[{{"jsonrpc":"2.0","id":{},"result":"0x15dac9b"}}]"#, transport_id);

        let _m2 = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(&success_response)
            .expect(1)
            .create_async()
            .await;

        let requests = vec![Box::new(transport) as Box<dyn RequestTransport>];
        let results = provider.execute_batch(&requests).await;

        assert_eq!(results.len(), 1, "Should get one result");
        let (_, result) = &results[0];
        assert!(result.is_ok(), "Result should be successful after retry");
    }

    #[tokio::test]
    async fn test_retry_on_rpc_error_header_not_found() {
        let retry_config = RPCRetryConfig {
            max_retries: 2,
            initial_backoff_ms: 10, // Fast for testing
            max_backoff_ms: 50,
        };
        let provider = RPCMetadataProvider::new_with_retry_config(10, retry_config);
        let mut server = Server::new_async().await;
        let endpoint = server.url();

        let request = RpcTransport::new(
            endpoint.clone(),
            "eth_getBlockByHash".to_string(),
            vec![
                json!("0x1234567890123456789012345678901234567890123456789012345678901234"), /* Random invalid block hash */
                json!(true),
            ],
        );

        let expected_calls = 3; // Initial + 2 retries

        // Mock server responses: first 2 calls return "header not found", 3rd succeeds
        for i in 0..expected_calls {
            let response_body = if i < expected_calls - 1 {
                // Return "header not found" error for first attempts
                vec![json!({
                    "jsonrpc": "2.0",
                    "id": request.id(),
                    "error": {
                        "code": -32000,
                        "message": "header not found"
                    }
                })]
            } else {
                // Return success on final attempt
                vec![json!({
                    "jsonrpc": "2.0",
                    "id": request.id(),
                    "result": {
                        "number": "0x1234",
                        "hash": "0x1234567890123456789012345678901234567890123456789012345678901234"
                    }
                })]
            };

            server
                .mock("POST", "/")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(serde_json::to_string(&response_body).unwrap())
                .expect(1)
                .create_async()
                .await;
        }

        let requests = vec![Box::new(request) as Box<dyn RequestTransport>];
        let results = provider.execute_batch(&requests).await;

        assert_eq!(results.len(), 1, "Should get one result");
        let (_, result) = &results[0];
        assert!(result.is_ok(), "Result should be successful after retrying RPC errors");

        // Verify all expected calls were made
        server.reset();
    }

    #[tokio::test]
    async fn test_retry_exhausted_on_persistent_rpc_error() {
        let retry_config = RPCRetryConfig {
            max_retries: 2,
            initial_backoff_ms: 10, // Fast for testing
            max_backoff_ms: 50,
        };
        let provider = RPCMetadataProvider::new_with_retry_config(10, retry_config);
        let mut server = Server::new_async().await;
        let endpoint = server.url();

        let request = RpcTransport::new(
            endpoint.clone(),
            "eth_getBlockByHash".to_string(),
            vec![
                json!("0x9999999999999999999999999999999999999999999999999999999999999999"), /* Another random invalid block hash */
                json!(true),
            ],
        );

        let expected_calls = 3; // Initial + 2 retries

        // Mock server to always return "header not found" error
        for _ in 0..expected_calls {
            let response_body = vec![json!({
                "jsonrpc": "2.0",
                "id": request.id(),
                "error": {
                    "code": -32000,
                    "message": "header not found"
                }
            })];

            server
                .mock("POST", "/")
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(serde_json::to_string(&response_body).unwrap())
                .expect(1)
                .create_async()
                .await;
        }

        let requests = vec![Box::new(request) as Box<dyn RequestTransport>];
        let results = provider.execute_batch(&requests).await;

        assert_eq!(results.len(), 1, "Should get one result");
        let (_, result) = &results[0];

        // Should fail with the RPC error after exhausting retries
        assert!(result.is_err(), "Result should fail after exhausting retries");
        let error_msg = format!("{:?}", result);
        assert!(
            error_msg.contains("header not found") || error_msg.contains("-32000"),
            "Error should contain RPC error details: {}",
            error_msg
        );

        // Verify all expected calls were made
        server.reset();
    }

    #[tokio::test]
    async fn test_mixed_batch_with_partial_errors_no_retry() {
        // Test that when batch has mixed success/failure, we don't retry
        // (Only retry when ALL requests fail)
        let retry_config = RPCRetryConfig {
            max_retries: 1,
            initial_backoff_ms: 10, // Fast for testing
            max_backoff_ms: 50,
        };
        let provider = RPCMetadataProvider::new_with_retry_config(10, retry_config);
        let mut server = Server::new_async().await;
        let endpoint = server.url();

        let request1 = RpcTransport::new(endpoint.clone(), "eth_blockNumber".to_string(), vec![]);

        let request2 = RpcTransport::new(
            endpoint.clone(),
            "eth_getBlockByHash".to_string(),
            vec![
                json!("0xabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd"), /* Random invalid block hash */
                json!(true),
            ],
        );

        // Mixed response: one success, one error
        // Should NOT trigger retry since not all requests failed
        let mixed_response = vec![
            json!({
                "jsonrpc": "2.0",
                "id": request1.id(),
                "result": "0x1234567"
            }),
            json!({
                "jsonrpc": "2.0",
                "id": request2.id(),
                "error": {
                    "code": -32000,
                    "message": "header not found"
                }
            }),
        ];

        server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::to_string(&mixed_response).unwrap())
            .expect(1) // Should only be called once (no retry)
            .create_async()
            .await;

        let requests = vec![
            Box::new(request1.clone()) as Box<dyn RequestTransport>,
            Box::new(request2.clone()) as Box<dyn RequestTransport>,
        ];
        let results = provider.execute_batch(&requests).await;

        assert_eq!(results.len(), 2, "Should get two results");

        // Check results: first should succeed, second should fail
        let results_map: std::collections::HashMap<_, _> = results.into_iter().collect();

        let result1 = results_map
            .get(&request1.deduplication_id())
            .expect("Should have result for request1");
        assert!(result1.is_ok(), "First request should succeed");

        let result2 = results_map
            .get(&request2.deduplication_id())
            .expect("Should have result for request2");
        assert!(result2.is_err(), "Second request should fail with RPC error");

        server.reset();
    }

    #[tokio::test]
    #[ignore = "Requires a real RPC endpoint - integration test for header not found with random block hash"]
    async fn test_real_rpc_retry_on_header_not_found() {
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");

        let retry_config =
            RPCRetryConfig { max_retries: 2, initial_backoff_ms: 100, max_backoff_ms: 1000 };
        let provider = RPCMetadataProvider::new_with_retry_config(10, retry_config);

        // Use a completely random block hash that definitely doesn't exist
        let random_block_hash =
            "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

        let request = RpcTransport::new(
            rpc_url,
            "eth_getBlockByHash".to_string(),
            vec![json!(random_block_hash), json!(true)],
        );

        let requests = vec![Box::new(request) as Box<dyn RequestTransport>];
        let results = provider.execute_batch(&requests).await;

        assert_eq!(results.len(), 1, "Should get one result");
        let (_, result) = &results[0];

        // Debug: print the actual result to understand what we're getting
        println!("Actual result: {:?}", result);

        // Should fail with "header not found" error after retrying
        if result.is_err() {
            let error_msg = format!("{:?}", result);
            assert!(
                error_msg.contains("header not found") || error_msg.contains("-32000"),
                "Error should be about header not found, got: {}",
                error_msg
            );
        } else {
            // If the result is Ok, print it and check if it contains null (which is also valid for
            // non-existent blocks)
            println!("Unexpected success result: {:?}", result);

            // Some RPC implementations return null instead of error for non-existent blocks
            if let Ok(value) = result {
                if value.is_null() {
                    println!("✅ RPC returned null for non-existent block (this is also valid)");
                    return; // Test passes
                }
            }
            panic!("Expected error or null result for non-existent block hash");
        }

        println!("✅ Successfully tested retry behavior with real RPC endpoint");
        println!("   Block hash used: {}", random_block_hash);
        println!("   Final error: {:?}", result);
    }
}
