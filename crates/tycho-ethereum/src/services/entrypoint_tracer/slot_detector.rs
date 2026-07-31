use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use alloy::{
    primitives::{Address as AlloyAddress, B256, U256},
    rpc::types::trace::geth::{FourByteFrame, GethTrace, PreStateFrame},
    transports::TransportResult,
};
use serde_json::Value;
use thiserror::Error;
use tracing::{debug, error, warn};
use tycho_common::{
    models::{blockchain::RPCTracerParams, Address, BlockHash},
    Bytes,
};

use crate::{
    rpc::EthereumRpcClient, services::entrypoint_tracer::tracer::EVMEntrypointService, BytesCodec,
};

/// Type alias for intermediate slot detection results: maps token address to (all_slots,
/// expected_value)
type DetectedSlotsResults = HashMap<Address, Result<(SlotValues, U256), SlotDetectorError>>;

/// Type alias for final slot detection results: maps token address to (storage_addr, slot_bytes)
type TokenSlotResults = HashMap<Address, Result<(Address, Bytes), SlotDetectorError>>;

/// Type alias for slot values from trace: (address, slot_bytes) with value
type SlotValues = Vec<((Address, Bytes), U256)>;

/// Type alias for the cache
type ThreadSafeCache<K, V> = Arc<std::sync::RwLock<HashMap<K, V>>>;

/// Maximum number of slot candidates to test per token before giving up.
const MAX_SLOT_CANDIDATES: usize = 5;

#[derive(Debug, Clone)]
pub(crate) struct SlotMetadata {
    token: Address,
    original_value: U256,
    test_value: U256,
    all_slots: SlotValues,
}

/// Shared error type for slot detection RPC operations
#[derive(Clone, Debug, Error)]
pub enum SlotDetectorError {
    #[error("Setup error: {0}")]
    SetupError(String),
    #[error("RPC request failed: {0}")]
    RequestError(String),
    #[error("Invalid response: {0}")]
    InvalidResponse(String),
    #[error("Token not found in trace")]
    TokenNotInTrace,
    #[error("Failed to parse trace: {0}")]
    ParseError(String),
    #[error("Failed to extract target: {0}")]
    ValueExtractionError(String),
    #[error("Unknown error: {0}")]
    UnknownError(String),
    #[error("Wrong slot detected :{0}")]
    WrongSlotError(String),
}

/// Represents a single value request for slot detection (debug_traceCall + eth_call)
#[derive(Debug, Clone)]
pub(crate) struct SlotDetectorValueRequest {
    pub(crate) token: AlloyAddress,
    pub(crate) tracer_params: Value,
}

/// Represents a single slot test request with storage override
#[derive(Debug, Clone)]
pub(crate) struct SlotDetectorSlotTestRequest {
    pub(crate) storage_address: AlloyAddress,
    pub(crate) slot: U256,
    pub(crate) token: AlloyAddress,
    pub(crate) test_value: U256,
}

/// Strategy trait for different slot detection types (balance, allowance, etc.)
/// This allows the SlotDetector to work with different parameter types and cache keys
pub trait SlotDetectionStrategy: Send + Sync {
    /// The cache key type for this strategy
    type CacheKey: std::hash::Hash + Eq + Clone;
    /// The parameters needed for this strategy (e.g., owner for balance, (owner, spender) for
    /// allowance)
    type Params: Clone;

    /// Generate a cache key from token and parameters
    fn cache_key(token: &Address, params: &Self::Params) -> Self::CacheKey;

    /// Encode the calldata for this slot type
    fn encode_calldata(params: &Self::Params) -> Bytes;
}

/// Generic slot detector that handles RPC communication and slot detection.
/// The specific strategy (balance, allowance, etc.) is defined by the generic parameter `S`.
pub struct SlotDetector<S: SlotDetectionStrategy> {
    /// Maximum number of tokens to process per batch (unrelated to RPC batch size)
    max_token_batch_size: usize,
    rpc: EthereumRpcClient,
    cache: ThreadSafeCache<S::CacheKey, (Address, Bytes)>,
}

impl<S: SlotDetectionStrategy> SlotDetector<S> {
    /// Create a new SlotDetector with the given configuration and strategy
    pub fn new(rpc: &EthereumRpcClient) -> Self {
        // Create HTTP client with connection pooling and reasonable timeouts
        Self {
            max_token_batch_size: 10,
            rpc: rpc.clone(),
            cache: Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    /// With `max_token_batch_size` limit of processing tokens per batch
    pub fn with_max_token_batch_size(mut self, max_token_batch_size: usize) -> Self {
        self.max_token_batch_size = max_token_batch_size;
        self
    }

    /// Detect slots for tokens using batched requests (debug_traceCall + eth_call per token)
    async fn detect_token_slots(
        &self,
        tokens: &[Address],
        params: &S::Params,
        block_hash: &BlockHash,
    ) -> HashMap<Address, Result<(Address, Bytes), SlotDetectorError>> {
        if tokens.is_empty() {
            return HashMap::new();
        }

        let mut request_tokens = Vec::with_capacity(tokens.len());
        let mut cached_tokens = HashMap::new();

        // Check cache for tokens
        {
            let cache = self.cache.read().unwrap();
            for token in tokens {
                let cache_key = S::cache_key(token, params);
                if let Some(slot) = cache.get(&cache_key) {
                    cached_tokens.insert(token.clone(), Ok(slot.clone()));
                } else {
                    request_tokens.push(token.clone());
                }
            }
        }

        if request_tokens.is_empty() {
            return cached_tokens;
        }

        // Create batched request: 2 requests per token (debug_traceCall + eth_call)
        let calldata = S::encode_calldata(params);
        let requests = self.create_value_requests(&request_tokens, &calldata, block_hash);

        // Send the batched request
        let responses = match self
            .rpc
            .slot_detector_trace(requests, &calldata, &B256::from_bytes(block_hash))
            .await
        {
            Ok(responses) => responses,
            Err(e) => {
                for token in &request_tokens {
                    cached_tokens
                        .insert(token.clone(), Err(SlotDetectorError::RequestError(e.to_string())));
                }
                return cached_tokens;
            }
        };

        // Process the batched response to extract slots
        let token_slots = self.process_batched_response(&request_tokens, responses);

        // Detect the correct slot by testing candidates with storage overrides
        let detected_results = self
            .detect_correct_slots(token_slots, &calldata, block_hash)
            .await;

        // Update cache and prepare final results
        let mut final_results = cached_tokens;
        {
            let mut cache = self.cache.write().unwrap();
            for (token, result) in detected_results {
                match result {
                    Ok((storage_addr, slot_bytes)) => {
                        // Update cache with successful detections
                        let cache_key = S::cache_key(&token, params);
                        cache.insert(cache_key, (storage_addr.clone(), slot_bytes.clone()));
                        final_results.insert(token, Ok((storage_addr, slot_bytes)));
                    }
                    Err(e) => {
                        final_results.insert(token, Err(e));
                    }
                }
            }
        }

        final_results
    }

    /// Detect slots for multiple tokens in chunks
    pub async fn detect_slots_chunked(
        &self,
        tokens: &[Address],
        params: &S::Params,
        block_hash: &BlockHash,
    ) -> HashMap<Address, Result<(Address, Bytes), SlotDetectorError>> {
        let mut all_results = HashMap::new();

        for (chunk_idx, chunk) in tokens
            .chunks(self.max_token_batch_size)
            .enumerate()
        {
            debug!("Processing chunk {} with {} tokens", chunk_idx, chunk.len());

            let chunk_results = self
                .detect_token_slots(chunk, params, block_hash)
                .await;
            all_results.extend(chunk_results);
        }

        all_results
    }

    /// Create batch request data for all tokens (2 requests per token).
    /// We need to send an eth_call after the tracing to get the return value of the balanceOf
    /// function. Currently, debug_traceCall does not support preStateTracer + returning the
    /// value in the same request.
    pub(crate) fn create_value_requests(
        &self,
        tokens: &[Address],
        calldata: &Bytes,
        block_hash: &BlockHash,
    ) -> Vec<SlotDetectorValueRequest> {
        let tracer_params = RPCTracerParams::new(None, calldata.clone());

        tokens
            .iter()
            .map(|token| {
                let tracer_params = EVMEntrypointService::create_trace_call_params(
                    token,
                    &tracer_params,
                    block_hash,
                );

                SlotDetectorValueRequest { token: AlloyAddress::from_bytes(token), tracer_params }
            })
            .collect()
    }

    /// Process batched responses and extract storage slots for each token
    fn process_batched_response(
        &self,
        tokens: &[Address],
        responses: Vec<(GethTrace, U256)>,
    ) -> DetectedSlotsResults {
        let mut token_slots = HashMap::new();

        for ((debug_trace, expected_value), token) in responses.into_iter().zip(tokens.iter()) {
            match self.extract_slot_values_from_trace_response(debug_trace) {
                Ok(all_slots) => {
                    debug!(
                        token = %token,
                        num_slots = all_slots.len(),
                        "Found {} storage slots for token, will test to find correct one",
                        all_slots.len()
                    );
                    token_slots.insert(token.clone(), Ok((all_slots, expected_value)));
                }
                Err(e) => {
                    error!(token = %token, error = %e, "Failed to extract slots for token");
                    token_slots.insert(token.clone(), Err(e));
                }
            }
        }

        token_slots
    }

    async fn test_slots_with_fallback(
        &self,
        slots_to_test: Vec<SlotMetadata>,
        calldata: &Bytes,
        block_hash: &BlockHash,
    ) -> TokenSlotResults {
        let mut detected_results = HashMap::new();
        let mut current_attempts = slots_to_test;

        // Retry loop: Test slots with storage overrides, and if a slot fails validation,
        // remove it from the candidate list and retry with remaining slots.
        // This continues until either:
        // 1. All tokens find a valid slot (added to detected_results)
        // 2. A token exhausts all slot candidates (error added to detected_results)
        // 3. An RPC error occurs (error added to detected_results)
        loop {
            if current_attempts.is_empty() {
                break;
            }

            let requests = match self.create_slot_test_requests(&current_attempts) {
                Ok(requests) => requests,
                Err(e) => {
                    for metadata in current_attempts {
                        detected_results.insert(
                            metadata.token,
                            Err(SlotDetectorError::RequestError(format!(
                                "Failed to create slot test request: {e}"
                            ))),
                        );
                    }
                    break;
                }
            };

            let responses = match self
                .rpc
                .slot_detector_tests(&requests, calldata, &B256::from_bytes(block_hash))
                .await
            {
                Ok(responses) => responses,
                Err(e) => {
                    for metadata in current_attempts {
                        detected_results.insert(
                            metadata.token,
                            Err(SlotDetectorError::RequestError(format!(
                                "Slot test request failed: {e}"
                            ))),
                        );
                    }
                    break;
                }
            };

            current_attempts = self.process_slot_test_responses(
                responses,
                current_attempts,
                &mut detected_results,
            );
        }

        detected_results
    }

    /// Sort slots by priority for testing.
    ///
    /// Primary: slots at the token's own address first (most common case).
    /// Secondary: distance to original value (closer first).
    fn sort_slots_by_priority(slots: &mut SlotValues, token: &Address, original_value: U256) {
        slots.sort_by_key(|((storage_addr, _), new_value)| {
            // Primary: token-address slots first (precompiles / external storage last)
            // Secondary: distance to original balance (closer is better)
            let is_foreign = storage_addr != token;
            (is_foreign, new_value.abs_diff(original_value))
        });
    }

    pub fn extract_u256_from_call_response(
        &self,
        response: &Value,
    ) -> Result<U256, SlotDetectorError> {
        let result = response.as_str().ok_or_else(|| {
            SlotDetectorError::InvalidResponse("Missing result in eth_call".into())
        })?;

        let hex_str = result
            .strip_prefix("0x")
            .unwrap_or(result);
        if hex_str.len() != 64 {
            return Err(SlotDetectorError::ValueExtractionError(format!(
                "Invalid result length: {} (expected 64)",
                hex_str.len()
            )));
        }

        U256::from_str_radix(hex_str, 16)
            .map_err(|e| SlotDetectorError::ValueExtractionError(e.to_string()))
    }

    /// Extract accessed slots with their values from debug_traceCall response for better slot
    /// selection
    fn extract_slot_values_from_trace_response(
        &self,
        response: GethTrace,
    ) -> Result<SlotValues, SlotDetectorError> {
        // The debug_traceCall with prestateTracer returns the result directly as a hashmap
        let frame_map = match response {
            GethTrace::PreStateTracer(PreStateFrame::Default(map)) => map.0,
            // Handle empty traces gracefully
            GethTrace::FourByteTracer(FourByteFrame(map)) => {
                if map.is_empty() {
                    BTreeMap::new()
                } else {
                    error!("Failed to parse trace result as hashmap: unexpected format");
                    return Err(SlotDetectorError::ParseError(
                        "Failed to parse trace result as hashmap: unexpected format".to_string(),
                    ));
                }
            }
            _ => {
                error!("Failed to parse trace result as hashmap: unexpected format");
                return Err(SlotDetectorError::ParseError(
                    "Failed to parse trace result as hashmap: unexpected format".to_string(),
                ));
            }
        };

        let mut slot_values = Vec::new();

        for (address, account_data) in frame_map {
            for (slot_key, slot_value) in account_data.storage {
                slot_values.push((
                    (address.to_bytes(), slot_key.to_bytes()),
                    U256::from_bytes(&slot_value.to_bytes()),
                ));
            }
        }

        Ok(slot_values)
    }

    /// Detects the correct storage slot by testing candidates with storage overrides.
    ///
    /// Candidates are sorted by priority (token-address first, then value distance) and tested in
    /// order. If a candidate fails with a transport error (e.g. chain disallows overriding a
    /// precompile address), it is dropped and the next candidate is retried.
    async fn detect_correct_slots(
        &self,
        token_slots: DetectedSlotsResults,
        calldata: &Bytes,
        block_hash: &BlockHash,
    ) -> TokenSlotResults {
        let mut detected_results = HashMap::new();
        let mut slots_to_test = Vec::new();

        for (token, result) in token_slots {
            match result {
                Ok((mut all_slots, original_value)) => {
                    if all_slots.is_empty() {
                        detected_results.insert(token, Err(SlotDetectorError::TokenNotInTrace));
                    } else {
                        Self::sort_slots_by_priority(&mut all_slots, &token, original_value);
                        all_slots.truncate(MAX_SLOT_CANDIDATES);

                        slots_to_test.push(SlotMetadata {
                            token,
                            original_value,
                            test_value: Self::generate_test_value(original_value),
                            all_slots,
                        });
                    }
                }
                Err(e) => {
                    detected_results.insert(token, Err(e));
                }
            }
        }

        if slots_to_test.is_empty() {
            return detected_results;
        }

        // Test all slot candidates, trying alternate slots if needed
        let test_results = self
            .test_slots_with_fallback(slots_to_test, calldata, block_hash)
            .await;
        detected_results.extend(test_results);

        detected_results
    }

    /// Generate a test value for validation that's different from the original
    fn generate_test_value(original_value: U256) -> U256 {
        if !original_value.is_zero() && original_value != U256::MAX {
            // Set to a different value (double the original) - this function caps to U256::MAX so
            // no overflow can happen.
            original_value.saturating_mul(U256::from(2))
        } else {
            // If original is 0, set to a non-zero value (1 ETH in wei)
            U256::from(1_000_000_000_000_000_000u64)
        }
    }

    fn create_slot_test_requests(
        &self,
        slots_to_test: &[SlotMetadata],
    ) -> Result<Vec<SlotDetectorSlotTestRequest>, SlotDetectorError> {
        slots_to_test
            .iter()
            .map(|metadata| {
                let (storage_addr, slot) = &metadata
                    .all_slots
                    .first()
                    .ok_or(SlotDetectorError::TokenNotInTrace)?
                    .0;

                Ok(SlotDetectorSlotTestRequest {
                    storage_address: AlloyAddress::from_bytes(storage_addr),
                    slot: U256::from_bytes(slot),
                    token: AlloyAddress::from_bytes(&metadata.token),
                    test_value: metadata.test_value,
                })
            })
            .collect::<Result<Vec<_>, SlotDetectorError>>()
    }

    fn process_slot_test_responses(
        &self,
        responses: Vec<TransportResult<Value>>,
        slots_to_test: Vec<SlotMetadata>,
        results: &mut TokenSlotResults,
    ) -> Vec<SlotMetadata> {
        let mut retry_data = Vec::new();
        for (idx, mut metadata) in slots_to_test.into_iter().enumerate() {
            let response_id = idx;

            match responses.get(response_id) {
                Some(response) => {
                    let Some(((storage_addr, slot), _)) = metadata.all_slots.first() else {
                        results.insert(metadata.token, Err(SlotDetectorError::TokenNotInTrace));
                        continue;
                    };
                    let (storage_addr, slot) = (storage_addr.clone(), slot.clone());

                    // On transport errors (e.g. Arbitrum disallows overriding
                    // precompile addresses), drop this candidate and retry.
                    let response_value = match response {
                        Err(error) => {
                            metadata
                                .all_slots
                                .retain(|s| s.0 != (storage_addr.clone(), slot.clone()));
                            if !metadata.all_slots.is_empty() {
                                warn!(
                                    token = %metadata.token,
                                    error = %error,
                                    "Slot test call failed - trying next slot"
                                );
                                retry_data.push(metadata);
                            } else {
                                results.insert(
                                    metadata.token,
                                    Err(SlotDetectorError::RequestError(format!(
                                        "Slot test call failed: {error}",
                                    ))),
                                );
                            }
                            continue;
                        }
                        Ok(response_value) => response_value,
                    };

                    match self.extract_u256_from_call_response(response_value) {
                        Ok(returned_value) => {
                            // Check if the override worked (balance should be different from
                            // original_value). We can't guarantee that it will match the override
                            // value, as some tokens use shares systems, making it hard to control
                            // the balance with a single override.
                            if returned_value != metadata.original_value {
                                // Validation successful - the slot works
                                debug!(
                                    token = %metadata.token,
                                    storage = %storage_addr,
                                    slot = %slot,
                                    returned_balance = %returned_value,
                                    original_value = %metadata.original_value,
                                    "Storage slot detected successfully"
                                );
                                results.insert(
                                    metadata.token,
                                    Ok((storage_addr.clone(), slot.clone())),
                                );
                            } else {
                                // Override didn't change the value - this slot is incorrect.
                                // Remove it from candidates and try the next slot in priority
                                // order.
                                metadata
                                    .all_slots
                                    .retain(|s| s.0 != (storage_addr.clone(), slot.clone()));
                                if !metadata.all_slots.is_empty() {
                                    warn!("Storage slot test failed - trying next slot");
                                    retry_data.push(metadata.clone());
                                } else {
                                    warn!(
                                        token = %metadata.token,
                                        slot = %slot,
                                        "Storage slot test failed - no more slots to try"
                                    );
                                    results.insert(
                                        metadata.token,
                                        Err(SlotDetectorError::WrongSlotError(
                                            "Slot override didn't change value for any detected slot.".to_string(),
                                        )),
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            // If we encounter an extraction error - try the next slot
                            // This handles cases like proxy contracts where some slots return empty
                            // data
                            metadata
                                .all_slots
                                .retain(|s| s.0 != (storage_addr.clone(), slot.clone()));
                            if !metadata.all_slots.is_empty() {
                                warn!(
                                    token = %metadata.token,
                                    error = %e,
                                    "Failed to extract value from response - trying next slot"
                                );
                                retry_data.push(metadata.clone());
                            } else {
                                results.insert(
                                    metadata.token,
                                    Err(SlotDetectorError::InvalidResponse(format!(
                                        "Failed to extract value from slot test response: {e}"
                                    ))),
                                );
                            }
                        }
                    }
                }
                None => {
                    results.insert(
                        metadata.token,
                        Err(SlotDetectorError::InvalidResponse(
                            "Missing validation response".into(),
                        )),
                    );
                }
            }
        }

        retry_data
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::{
        primitives::{Address as AlloyAddress, U256},
        transports::TransportResult,
    };
    use serde_json::{json, Value};
    use tycho_common::{
        models::{Address, BlockHash},
        Bytes,
    };

    use crate::{
        rpc::EthereumRpcClient,
        services::entrypoint_tracer::{
            balance_slot_detector::BalanceStrategy,
            slot_detector::{
                SlotDetectionStrategy, SlotDetector, SlotDetectorError, SlotMetadata, SlotValues,
            },
        },
        test_fixtures::TestFixture,
        BytesCodec,
    };

    struct TestFixtureStrategy {}

    impl SlotDetectionStrategy for TestFixtureStrategy {
        type CacheKey = (Address, Address); // (token, holder)
        type Params = Address; // holder

        fn cache_key(token: &Address, params: &Self::Params) -> Self::CacheKey {
            (token.clone(), params.clone())
        }

        fn encode_calldata(params: &Self::Params) -> Bytes {
            // balanceOf selector + padded address
            let mut calldata = vec![0x70, 0xa0, 0x82, 0x31];
            calldata.extend_from_slice(&[0u8; 12]);
            calldata.extend_from_slice(params.as_ref());
            Bytes::from(calldata)
        }
    }

    fn create_slot_candidates() -> Vec<SlotMetadata> {
        vec![
            SlotMetadata {
                token: Address::from([0x11u8; 20]),
                original_value: U256::from(1000u64),
                test_value: U256::from(2000u64),
                all_slots: vec![(
                    (Address::from([0x11u8; 20]), Bytes::from(vec![0x01u8; 32])),
                    U256::from(1000u64),
                )],
            },
            SlotMetadata {
                token: Address::from([0x22u8; 20]),
                original_value: U256::from(3000u64),
                test_value: U256::from(6000u64),
                all_slots: vec![(
                    (Address::from([0x22u8; 20]), Bytes::from(vec![0x02u8; 32])),
                    U256::from(3000u64),
                )],
            },
        ]
    }

    impl TestFixture {
        fn create_slot_detector_without_rpc() -> SlotDetector<TestFixtureStrategy> {
            TestFixture::create_slot_detector("http://localhost:8545")
        }

        fn create_slot_detector(rpc_url: &str) -> SlotDetector<TestFixtureStrategy> {
            let rpc = EthereumRpcClient::new(rpc_url).expect("Failed to create RPC client");

            SlotDetector::<TestFixtureStrategy>::new(&rpc)
        }
    }

    #[test]
    fn test_generate_test_value() {
        // Test non-zero, non-max value
        let original = U256::from(1000u64);
        let test_value = SlotDetector::<TestFixtureStrategy>::generate_test_value(original);
        assert_eq!(test_value, U256::from(2000u64));

        // Test zero value
        let zero = U256::ZERO;
        let test_value = SlotDetector::<TestFixtureStrategy>::generate_test_value(zero);
        assert_eq!(test_value, U256::from(1_000_000_000_000_000_000u64));

        // Test max value - falls into else branch since original_value == U256::MAX
        let max = U256::MAX;
        let test_value = SlotDetector::<TestFixtureStrategy>::generate_test_value(max);
        assert_eq!(test_value, U256::from(1_000_000_000_000_000_000u64)); // Returns 1 ETH for MAX
    }

    #[test]
    fn test_extract_balance_from_call_response() {
        let detector = TestFixture::create_slot_detector_without_rpc();
        // Test valid response
        let response = json!("0x0000000000000000000000000000000000000000000000000de0b6b3a7640000");
        let balance = detector
            .extract_u256_from_call_response(&response)
            .unwrap();
        assert_eq!(balance, U256::from(1_000_000_000_000_000_000u64)); // 1 ETH

        // Test missing result
        let response = json!({});
        let result = detector.extract_u256_from_call_response(&response);
        assert!(matches!(result, Err(SlotDetectorError::InvalidResponse(_))));

        // Test invalid hex length
        let response = json!("0x1234");
        let result = detector.extract_u256_from_call_response(&response);
        assert!(matches!(result, Err(SlotDetectorError::ValueExtractionError(_))));

        // Test invalid hex characters
        let response = json!("0xGGGG000000000000000000000000000000000000000000000000000000000000");
        let result = detector.extract_u256_from_call_response(&response);
        assert!(matches!(result, Err(SlotDetectorError::ValueExtractionError(_))));
    }

    #[test]
    fn test_extract_slot_values_from_trace_response() {
        let detector = TestFixture::create_slot_detector_without_rpc();
        // Test valid trace with storage
        let response = serde_json::from_value(json!({
                "0x1234567890123456789012345678901234567890": {
                    "storage": {
                        "0x0000000000000000000000000000000000000000000000000000000000000001": "0x0000000000000000000000000000000000000000000000000de0b6b3a7640000",
                        "0x0000000000000000000000000000000000000000000000000000000000000002": "0x0000000000000000000000000000000000000000000000001bc16d674ec80000"
                    }
                }
            }
        )).unwrap();

        let slot_values = detector
            .extract_slot_values_from_trace_response(response)
            .unwrap();
        assert_eq!(slot_values.len(), 2);

        // Verify first slot
        let first_slot = &slot_values[0];
        assert_eq!(first_slot.1, U256::from(1_000_000_000_000_000_000u64));

        // Verify second slot
        let second_slot = &slot_values[1];
        assert_eq!(second_slot.1, U256::from(2_000_000_000_000_000_000u64));

        // Test missing result
        let response = serde_json::from_value(json!({})).unwrap();
        let result = detector.extract_slot_values_from_trace_response(response);
        assert!(result.ok().unwrap().is_empty());
    }

    #[test]
    fn test_process_batched_response() {
        let detector = TestFixture::create_slot_detector_without_rpc();
        let token1 = Address::from([0x11u8; 20]);
        let token2 = Address::from([0x22u8; 20]);

        // Create responses with proper IDs (out of order to test ID mapping)
        let responses = vec![
            (serde_json::from_value(
            json!({
                "0x1111111111111111111111111111111111111111": {
                    "storage": {
                        "0x0000000000000000000000000000000000000000000000000000000000000001": "0x0000000000000000000000000000000000000000000000000de0b6b3a7640000"
                    }
                }
            })).unwrap(),
            serde_json::from_value(json!("0x0000000000000000000000000000000000000000000000000de0b6b3a7640000")).unwrap()),
            (serde_json::from_value(json!({
                    "0x2222222222222222222222222222222222222222": {
                        "storage": {
                            "0x0000000000000000000000000000000000000000000000000000000000000002": "0x0000000000000000000000000000000000000000000000001bc16d674ec80000"
                        }
                    }

            })).unwrap(),
            serde_json::from_value(json!("0x0000000000000000000000000000000000000000000000001bc16d674ec80000")).unwrap()),
        ];

        let tokens = vec![token1.clone(), token2.clone()];
        let result = detector.process_batched_response(&tokens, responses);

        assert_eq!(result.len(), 2);
        assert!(result.contains_key(&token1));
        assert!(result.contains_key(&token2));

        // Verify token1 result
        let token1_result = result.get(&token1).unwrap();
        assert!(token1_result.is_ok());

        // Verify token2 result
        let token2_result = result.get(&token2).unwrap();
        assert!(token2_result.is_ok());
    }

    #[test]
    fn test_create_balance_requests() {
        let detector = TestFixture::create_slot_detector_without_rpc();
        let token1 = Address::from([0x11u8; 20]);
        let token2 = Address::from([0x22u8; 20]);
        let owner = Address::from([0x33u8; 20]);
        let block_hash = BlockHash::from([0x44u8; 32]);

        let tokens = vec![token1.clone(), token2.clone()];
        let calldata = BalanceStrategy::encode_calldata(&owner);
        let requests = detector.create_value_requests(&tokens, &calldata, &block_hash);

        // Should create one joint requests per token
        let array = requests;
        assert_eq!(array.len(), 2);

        assert_eq!(array[0].token, AlloyAddress::from_bytes(&Address::from(token1)));
        assert_eq!(array[1].token, AlloyAddress::from_bytes(&Address::from(token2)));
    }

    #[test]
    fn test_create_validation_requests() {
        let detector = TestFixture::create_slot_detector_without_rpc();
        let slot_candidates = create_slot_candidates();

        // Create responses - first one changes (valid), second doesn't (invalid)
        let responses = vec![
            Ok(json!(
                "0x00000000000000000000000000000000000000000000000000000000000007d0" /* 2000 (changed) */
            )),
            Ok(json!(
                "0x0000000000000000000000000000000000000000000000000000000000000bb8" /* 3000 (unchanged) */
            )),
        ];

        let mut results = HashMap::new();
        let retry_data =
            detector.process_slot_test_responses(responses, slot_candidates, &mut results);

        assert_eq!(results.len(), 2);

        // First token should be valid (balance changed)
        let token1 = Address::from([0x11u8; 20]);
        assert!(results.get(&token1).unwrap().is_ok());

        // Second token should be invalid (balance didn't change) - no retry because SlotValues is
        // empty after pop
        let token2 = Address::from([0x22u8; 20]);
        assert!(matches!(results.get(&token2).unwrap(), Err(SlotDetectorError::WrongSlotError(_))));

        // No retries should be scheduled since SlotValues only had one slot each
        assert!(retry_data.is_empty());
    }

    #[tokio::test]
    async fn test_cache_prevents_duplicate_rpc_calls() {
        let mut server = mockito::Server::new_async().await;

        let token = Address::from([0x11u8; 20]);
        let holder = Address::from([0x22u8; 20]);
        let block_hash = BlockHash::from([0x33u8; 32]);

        let trace_response = json!([
            {
                "jsonrpc": "2.0",
                "id": 0,
                "result": {
                    "0x1111111111111111111111111111111111111111": {
                        "storage": {
                            "0x0000000000000000000000000000000000000000000000000000000000000001": "0x00000000000000000000000000000000000000000000000000000000000003e8"
                        }
                    }
                }
            },
            {
                "jsonrpc": "2.0",
                "id": 1,
                "result": "0x00000000000000000000000000000000000000000000000000000000000003e8"
            }
        ]);
        let slot_test_response = json!([
            {
                "jsonrpc": "2.0",
                "id": 2,
                "result": "0x00000000000000000000000000000000000000000000000000000000000007d0"
            }
        ]);

        // First RPC call: trace + balance fetch.
        let trace_mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(trace_response.to_string())
            .expect(1)
            .create_async()
            .await;

        // Second RPC call: slot test with state override.
        let slot_test_mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(slot_test_response.to_string())
            .expect(1)
            .create_async()
            .await;

        let detector = TestFixture::create_slot_detector(&server.url());

        // First call - should populate the cache
        let results1 = detector
            .detect_slots_chunked(std::slice::from_ref(&token), &holder, &block_hash)
            .await;
        assert!(results1[&token].is_ok(), "First call should succeed: {:?}", results1[&token]);

        // Verify both RPC calls were made after the first invocation
        trace_mock.assert();
        slot_test_mock.assert();

        // Second call - should use the cache
        let results2 = detector
            .detect_slots_chunked(std::slice::from_ref(&token), &holder, &block_hash)
            .await;
        assert!(results2[&token].is_ok(), "Second call should succeed from cache");

        // Verify no additional RPC calls were made (counts unchanged)
        trace_mock.assert();
        slot_test_mock.assert();
    }

    #[test]
    fn test_sort_slots_by_priority() {
        let addr = Address::from([0x11u8; 20]);
        let slot_a = Bytes::from(vec![0x01u8; 32]);
        let slot_b = Bytes::from(vec![0x02u8; 32]);
        let slot_c = Bytes::from(vec![0x03u8; 32]);

        let original_value = U256::from(1000u64);

        // Slot values at different distances from original_value (1000):
        //   slot_a: 5000 → distance 4000
        //   slot_b: 999  → distance 1
        //   slot_c: 1500 → distance 500
        let mut slots: SlotValues = vec![
            ((addr.clone(), slot_a.clone()), U256::from(5000u64)),
            ((addr.clone(), slot_b.clone()), U256::from(999u64)),
            ((addr.clone(), slot_c.clone()), U256::from(1500u64)),
        ];

        SlotDetector::<TestFixtureStrategy>::sort_slots_by_priority(
            &mut slots,
            &addr,
            original_value,
        );

        // Expected order: slot_b (dist 1), slot_c (dist 500), slot_a (dist 4000)
        assert_eq!(slots[0].0 .1, slot_b);
        assert_eq!(slots[1].0 .1, slot_c);
        assert_eq!(slots[2].0 .1, slot_a);
    }

    #[test]
    fn test_sort_slots_by_priority_foreign_address_last() {
        let token = Address::from([0x11u8; 20]);
        let foreign = Address::from([0xaau8; 20]);
        let slot_a = Bytes::from(vec![0x01u8; 32]);
        let slot_b = Bytes::from(vec![0x02u8; 32]);
        let slot_c = Bytes::from(vec![0x03u8; 32]);

        let original_value = U256::from(1000u64);

        // slot_a is at a foreign address (e.g. Arbitrum precompile) with closest value,
        // slot_b and slot_c are at the token address with farther values.
        // Token-address slots must come before foreign-address slots regardless of distance.
        let mut slots: SlotValues = vec![
            ((foreign.clone(), slot_a.clone()), U256::from(999u64)), // distance 1, but foreign
            ((token.clone(), slot_b.clone()), U256::from(1500u64)),  // distance 500, token
            ((token.clone(), slot_c.clone()), U256::from(5000u64)),  // distance 4000, token
        ];

        SlotDetector::<TestFixtureStrategy>::sort_slots_by_priority(
            &mut slots,
            &token,
            original_value,
        );

        // Token slots first (ordered by distance), foreign slot last
        assert_eq!(slots[0].0 .1, slot_b);
        assert_eq!(slots[1].0 .1, slot_c);
        assert_eq!(slots[2].0 .1, slot_a);
    }

    #[test]
    fn test_failed_slot_schedules_retry_with_remaining() {
        let detector = TestFixture::create_slot_detector_without_rpc();

        let token = Address::from([0x11u8; 20]);
        let slot_a = Bytes::from(vec![0x01u8; 32]);
        let slot_b = Bytes::from(vec![0x02u8; 32]);

        let slots_to_test = vec![SlotMetadata {
            token: token.clone(),
            original_value: U256::from(1000u64),
            test_value: U256::from(2000u64),
            all_slots: vec![
                ((token.clone(), slot_a.clone()), U256::from(1000u64)),
                ((token.clone(), slot_b.clone()), U256::from(900u64)),
            ],
        }];

        // Response returns the original value (unchanged) → slot_a is wrong
        let responses =
            vec![Ok(json!("0x00000000000000000000000000000000000000000000000000000000000003e8"))];

        let mut results = HashMap::new();
        let retry_data =
            detector.process_slot_test_responses(responses, slots_to_test, &mut results);

        // Should schedule a retry with the remaining slot (slot_b)
        assert_eq!(retry_data.len(), 1);
        assert_eq!(retry_data[0].all_slots.len(), 1);
        assert_eq!(retry_data[0].all_slots[0].0 .1, slot_b);
        // No final result yet — token is still being retried
        assert!(results.is_empty());
    }

    #[test]
    fn test_transport_error_schedules_retry_with_remaining() {
        let detector = TestFixture::create_slot_detector_without_rpc();

        let token = Address::from([0x11u8; 20]);
        let slot_a = Bytes::from(vec![0x01u8; 32]);
        let slot_b = Bytes::from(vec![0x02u8; 32]);

        // A transport error on slot_a (e.g. Arbitrum precompile override rejected)
        // should drop that candidate and retry with slot_b.
        let slots_to_test = vec![SlotMetadata {
            token: token.clone(),
            original_value: U256::ZERO,
            test_value: U256::from(1_000_000_000_000_000_000u64),
            all_slots: vec![
                ((token.clone(), slot_a.clone()), U256::ZERO),
                ((token.clone(), slot_b.clone()), U256::ZERO),
            ],
        }];

        let responses: Vec<TransportResult<Value>> =
            vec![Err(alloy::transports::RpcError::LocalUsageError(Box::new(
                std::io::Error::other("transport error"),
            )))];

        let mut results = HashMap::new();
        let retry_data =
            detector.process_slot_test_responses(responses, slots_to_test, &mut results);

        assert_eq!(retry_data.len(), 1, "Should schedule retry with remaining slot");
        assert_eq!(retry_data[0].all_slots.len(), 1);
        assert_eq!(retry_data[0].all_slots[0].0 .1, slot_b);
        assert!(results.is_empty(), "No final result yet — token is still being retried");
    }

    #[test]
    fn test_transport_error_with_no_remaining_slots_is_terminal() {
        let detector = TestFixture::create_slot_detector_without_rpc();

        let token = Address::from([0x11u8; 20]);
        let slot_a = Bytes::from(vec![0x01u8; 32]);

        let slots_to_test = vec![SlotMetadata {
            token: token.clone(),
            original_value: U256::ZERO,
            test_value: U256::from(1_000_000_000_000_000_000u64),
            all_slots: vec![((token.clone(), slot_a.clone()), U256::ZERO)],
        }];

        let responses: Vec<TransportResult<Value>> =
            vec![Err(alloy::transports::RpcError::LocalUsageError(Box::new(
                std::io::Error::other("overriding address not allowed"),
            )))];

        let mut results = HashMap::new();
        let retry_data =
            detector.process_slot_test_responses(responses, slots_to_test, &mut results);

        assert!(retry_data.is_empty(), "No more slots to retry");
        assert!(results.contains_key(&token));
        assert!(matches!(results[&token], Err(SlotDetectorError::RequestError(_))));
    }

    #[test]
    fn test_sort_slots_by_priority_stable_on_equal_distance() {
        let addr = Address::from([0x11u8; 20]);
        let slot_a = Bytes::from(vec![0x01u8; 32]);
        let slot_b = Bytes::from(vec![0x02u8; 32]);

        let original_value = U256::from(1000u64);

        // Both slots equidistant from original_value: 900 and 1100, both distance 100
        let mut slots: SlotValues = vec![
            ((addr.clone(), slot_a.clone()), U256::from(900u64)),
            ((addr.clone(), slot_b.clone()), U256::from(1100u64)),
        ];

        SlotDetector::<TestFixtureStrategy>::sort_slots_by_priority(
            &mut slots,
            &addr,
            original_value,
        );

        // Stable sort preserves original order for equal keys
        assert_eq!(slots[0].0 .1, slot_a);
        assert_eq!(slots[1].0 .1, slot_b);
    }
}
