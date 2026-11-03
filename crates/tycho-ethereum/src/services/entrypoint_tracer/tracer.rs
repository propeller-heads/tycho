use std::collections::{BTreeMap, HashMap, HashSet};

use alloy::{
    primitives::{
        Address as AlloyAddress, BlockHash as AlloyBlockHash, Bytes as AlloyBytes, B256, U256,
    },
    rpc::types::{
        trace::geth::{
            GethDebugBuiltInTracerType, GethDebugTracerConfig, GethDebugTracerType,
            GethDebugTracingOptions, GethDefaultTracingOptions, GethTrace, PreStateFrame,
            PreStateMode,
        },
        AccessListResult, BlockId, TransactionInput, TransactionRequest,
    },
};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use tycho_common::{
    models::{
        blockchain::{
            AccountOverrides, AddressStorageLocation, EntryPointWithTracingParams, RPCTracerParams,
            StorageOverride, TracedEntryPoint, TracingParams, TracingResult,
        },
        Address, BlockHash,
    },
    traits::EntryPointTracer,
    Bytes,
};

use crate::{
    rpc::{config::RPCRetryConfig, EthereumRpcClient},
    BytesCodec, RPCError,
};

#[derive(Debug, Clone)]
pub struct EVMEntrypointService {
    rpc: EthereumRpcClient,
}

impl EVMEntrypointService {
    // TODO: consider if we want to use the default retry policy from the rpc client
    pub fn new(rpc: &EthereumRpcClient) -> Self {
        Self::new_with_config(rpc, 3, 200)
    }

    pub fn new_with_config(rpc: &EthereumRpcClient, max_retries: u32, retry_delay_ms: u64) -> Self {
        let policy = RPCRetryConfig {
            max_retries: max_retries as usize,
            initial_backoff_ms: retry_delay_ms,
            max_backoff_ms: retry_delay_ms,
        };

        let rpc_client = rpc.clone().with_retry(policy);

        Self { rpc: rpc_client }
    }

    fn create_access_list_params(
        target: &Address,
        params: &RPCTracerParams,
        block_hash: &BlockHash,
    ) -> Value {
        let mut tx_params = json!({
            "to": target.to_string(),
            "data": params.calldata.to_string()
        });

        if let Some(caller) = &params.caller {
            tx_params["from"] = json!(caller.to_string());
        }

        if params.state_overrides.is_none() ||
            params
                .state_overrides
                .as_ref()
                .unwrap_or(&BTreeMap::new())
                .is_empty()
        {
            json!([tx_params, block_hash.to_string()])
        } else {
            let state_overrides = Self::build_state_overrides(
                params
                    .state_overrides
                    .as_ref()
                    .unwrap_or(&BTreeMap::new()),
            );
            json!([tx_params, block_hash.to_string(), Value::Object(state_overrides)])
        }
    }

    pub(crate) fn create_trace_call_params(
        target: &Address,
        params: &RPCTracerParams,
        block_id: BlockId,
    ) -> Value {
        let caller = params.caller.as_ref().map(|addr| {
            AlloyAddress::try_from(addr.as_ref())
                .unwrap_or_else(|_| panic!("Invalid caller address: {addr}"))
        }); //TODO: Handle this error gracefully

        let tx_request = TransactionRequest {
            to: Some(
                AlloyAddress::try_from(target.as_ref())
                    .unwrap_or_else(|_| panic!("Invalid target address: {target}")) //TODO: Handle this error gracefully
                    .into(),
            ),
            from: caller,
            input: TransactionInput::new(AlloyBytes::from(params.calldata.to_vec())),
            ..Default::default()
        };

        // Check if we have non-empty state overrides
        let has_state_overrides = params
            .state_overrides
            .as_ref()
            .is_some_and(|o| !o.is_empty());

        if has_state_overrides {
            // Use build_state_overrides to properly normalize balance values (removes leading
            // zeros) This is required because Go's hexutil.Big rejects hex numbers with
            // leading zeros.
            let state_overrides = Self::build_state_overrides(
                params
                    .state_overrides
                    .as_ref()
                    .unwrap_or(&BTreeMap::new()),
            );
            let tracing_with_overrides = json!({
                "enableReturnData": true,
                "tracer": "prestateTracer",
                "stateOverrides": Value::Object(state_overrides)
            });

            json!([tx_request, block_id, tracing_with_overrides])
        } else {
            let tracing_options = GethDebugTracingOptions {
                config: GethDefaultTracingOptions::default().enable_return_data(),
                tracer: Some(GethDebugTracerType::BuiltInTracer(
                    GethDebugBuiltInTracerType::PreStateTracer,
                )),
                tracer_config: GethDebugTracerConfig::default(),
                timeout: None,
            };
            json!([tx_request, block_id, tracing_options])
        }
    }

    async fn batch_trace_and_access_list(
        &self,
        target: &Address,
        params: &RPCTracerParams,
        block_hash: &BlockHash,
    ) -> Result<(HashMap<Address, HashSet<Bytes>>, GethTrace), RPCError> {
        let access_list_params = Self::create_access_list_params(target, params, block_hash);
        let trace_call_params = Self::create_trace_call_params(
            target,
            params,
            BlockId::Hash(AlloyBlockHash::from_slice(block_hash.as_ref()).into()),
        );

        let (access_list_data, pre_state_trace) = self
            .rpc
            .trace_and_access_list(
                &alloy::primitives::Address::from_bytes(target),
                &B256::from_bytes(block_hash),
                &access_list_params,
                &trace_call_params,
            )
            .await?;

        let mut accessed_slots = Self::try_get_accessed_slots(&access_list_data)?;

        // eth_createAccessList excludes the target address from the access list unless
        // its state is accessed. This line ensures that the target
        // address is included in the access list even if its state is not accessed.
        // Source: https://github.com/ethereum/go-ethereum/blob/51342136fadf2972320cd70badb1336efe3259e1/internal/ethapi/api.go#L1180C2-L1180C87
        if !accessed_slots.contains_key(target) {
            accessed_slots.insert(target.clone(), HashSet::new());
        }

        Ok((accessed_slots, pre_state_trace))
    }

    fn try_get_accessed_slots(
        access_list: &AccessListResult,
    ) -> Result<HashMap<Address, HashSet<Bytes>>, RPCError> {
        if let Some(error) = &access_list.error {
            return Err(RPCError::TracingFailure(error.to_string()));
        }

        let mut out = HashMap::new();

        for entry in &access_list.access_list.0 {
            // Parse Address
            let addr = Address::from(entry.address.to_bytes());

            // Parse storage keys
            let set = entry
                .storage_keys
                .iter()
                .cloned()
                .map(|k| k.to_bytes())
                .collect();

            out.insert(addr, set);
        }
        Ok(out)
    }

    /// Detects if any called addresses are stored in a packed storage slot.
    ///
    /// On Ethereum, a storage slot is 32 bytes, and an address is 20 bytes. This means
    /// a single address can be packed with up to 12 bytes of other data in one slot.
    /// This function searches for any of the called addresses within the storage value
    /// and returns the storage location with the correct offset if found.
    fn detect_retrigger(
        called_addresses: &HashSet<Address>,
        slot: &B256,
        val: &B256,
    ) -> Option<AddressStorageLocation> {
        let value_bytes: &[u8] = val.as_ref();

        if let Some((offset, _window)) = value_bytes
            .windows(20)
            .enumerate()
            .find(|(_idx, window)| {
                let address = Address::from(*window);
                called_addresses.contains(&address)
            })
        {
            return Some(AddressStorageLocation::new(
                tycho_common::Bytes::from(slot.as_slice()),
                // This is safe since indices into B256 will always fit into u8
                offset as u8,
            ));
        }
        None
    }

    /// Normalizes a hex string by removing leading zeros.
    /// Converts Bytes to U256 and formats as hex without leading zeros.
    /// This is required for Ethereum JSON-RPC state overrides which don't accept
    /// hex numbers with leading zeros.
    fn normalize_hex_bytes(bytes: &Bytes) -> String {
        // Convert bytes to U256 (big-endian)
        let bytes_slice = bytes.as_ref();
        let mut buf = [0u8; 32];
        let start = 32usize.saturating_sub(bytes_slice.len());
        buf[start..].copy_from_slice(bytes_slice);
        let value = U256::from_be_bytes(buf);

        // Format as hex without leading zeros, but keep at least "0x0" for zero
        if value.is_zero() {
            "0x0".to_string()
        } else {
            format!("0x{:x}", value)
        }
    }

    fn build_state_overrides(
        overrides: &BTreeMap<Address, AccountOverrides>,
    ) -> Map<String, Value> {
        let mut state_overrides = Map::new();

        for (address, account_override) in overrides {
            let mut override_obj = Map::new();

            if let Some(ref code) = account_override.code {
                override_obj.insert("code".to_string(), json!(code));
            }

            if let Some(ref balance) = account_override.native_balance {
                let normalized_balance = Self::normalize_hex_bytes(balance);
                override_obj.insert("balance".to_string(), json!(normalized_balance));
            }

            if let Some(ref slots) = account_override.slots {
                match slots {
                    StorageOverride::Diff(slot_map) => {
                        let mut state_diff = Map::new();
                        for (slot, value) in slot_map {
                            state_diff.insert(slot.to_string(), json!(value));
                        }
                        override_obj.insert("stateDiff".to_string(), json!(state_diff));
                    }
                    StorageOverride::Replace(slot_map) => {
                        let mut state_map = Map::new();
                        for (slot, value) in slot_map {
                            state_map.insert(slot.to_string(), json!(value));
                        }
                        override_obj.insert("state".to_string(), json!(state_map));
                    }
                }
            }

            if !override_obj.is_empty() {
                state_overrides.insert(address.to_string(), json!(override_obj));
            }
        }

        state_overrides
    }
}
const ZERO_ADDRESS: [u8; 20] = [0u8; 20];

#[async_trait]
impl EntryPointTracer for EVMEntrypointService {
    type Error = RPCError;

    async fn trace(
        &self,
        block_hash: BlockHash,
        entry_points: Vec<EntryPointWithTracingParams>,
    ) -> Vec<Result<TracedEntryPoint, Self::Error>> {
        let mut results_with_indices = Vec::with_capacity(entry_points.len());

        for entry_point in entry_points {
            let result = match &entry_point.params {
                TracingParams::RPCTracer(rpc_entry_point) => {
                    // Use batched RPC call for both access list and trace
                    let (accessed_slots, pre_state_trace) = match self
                        .batch_trace_and_access_list(
                            &entry_point.entry_point.target,
                            rpc_entry_point,
                            &block_hash,
                        )
                        .await
                    {
                        Ok(trace) => trace,
                        Err(e) => {
                            results_with_indices.push(Err(e));
                            continue;
                        }
                    };

                    // Exclude ZERO_ADDRESS to avoid false positive retriggers on 0
                    //  value slots or slots with small values
                    let called_addresses: HashSet<Address> = accessed_slots
                        .keys()
                        .filter(|addr| addr.as_ref() != ZERO_ADDRESS)
                        .cloned()
                        .collect();

                    // Provides a very simplistic way of finding retriggers. A better way would
                    // involve using the structure of callframes. So basically iterate the call
                    // tree in a parent child manner then search the childs address in the prestate
                    // of parent.
                    let retriggers = if let GethTrace::PreStateTracer(PreStateFrame::Default(
                        PreStateMode(frame),
                    )) = pre_state_trace
                    {
                        let mut retriggers = HashSet::new();
                        for (address, account) in frame.iter() {
                            let address_bytes =
                                tycho_common::Bytes::from(address.as_ref() as &[u8]);
                            let storage = &account.storage;
                            for (slot, val) in storage.iter() {
                                if let Some(storage_location) =
                                    Self::detect_retrigger(&called_addresses, slot, val)
                                {
                                    retriggers.insert((address_bytes.clone(), storage_location));
                                }
                            }
                        }
                        retriggers
                    } else {
                        results_with_indices.push(Err(RPCError::UnknownError(
                            "invalid trace result for PreStateTracer".to_string(),
                        )));
                        continue;
                    };

                    Ok(TracedEntryPoint::new(
                        entry_point.clone(),
                        block_hash.clone(),
                        TracingResult::new(retriggers, accessed_slots),
                    ))
                }
            };
            results_with_indices.push(result);
        }

        results_with_indices
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mockito::Server;
    use rstest::rstest;
    use tycho_common::{
        keccak256,
        models::blockchain::{AccountOverrides, EntryPoint, RPCTracerParams},
        Bytes,
    };

    use super::*;
    use crate::{rpc::errors::ReqwestError, test_fixtures::TestFixture, RequestError};

    impl TestFixture {
        fn create_tracer() -> EVMEntrypointService {
            let fixture = TestFixture::new();

            let rpc_client = fixture.create_rpc_client(true);
            EVMEntrypointService::new(&rpc_client)
        }
    }

    #[test]
    fn test_build_state_overrides() {
        let mut state_overrides = BTreeMap::new();
        let mut slots = BTreeMap::new();

        slots.insert(
            Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap(),
            Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000042")
                .unwrap(),
        );

        let account_override = AccountOverrides {
            slots: Some(StorageOverride::Diff(slots)),
            native_balance: None,
            code: Some(Bytes::from_str("0x6060604052").unwrap()),
        };

        state_overrides.insert(
            Bytes::from_str("0x1234567890123456789012345678901234567890").unwrap(),
            account_override,
        );

        let result = EVMEntrypointService::build_state_overrides(&state_overrides);

        assert!(!result.is_empty());
        assert!(result.contains_key("0x1234567890123456789012345678901234567890"));

        let override_obj = &result["0x1234567890123456789012345678901234567890"];
        assert!(override_obj.get("code").is_some());
        assert!(override_obj.get("stateDiff").is_some());
    }

    #[tokio::test]
    #[ignore = "requires a RPC connection"]
    async fn test_trace_balancer_v3_stable_pool() {
        let tracer = TestFixture::create_tracer();
        let entry_points = vec![
            EntryPointWithTracingParams::new(
                EntryPoint::new(
                    "0xEdf63cce4bA70cbE74064b7687882E71ebB0e988:getRate()".to_string(),
                    Bytes::from_str("0xEdf63cce4bA70cbE74064b7687882E71ebB0e988").unwrap(),
                    "getRate()".to_string(),
                ),
                TracingParams::RPCTracer(RPCTracerParams::new(
                    None,
                    Bytes::from(&keccak256("getRate()").to_vec()[0..4]),
                )),
            ),
            EntryPointWithTracingParams::new(
                EntryPoint::new(
                    "0x8f4E8439b970363648421C692dd897Fb9c0Bd1D9:getRate()".to_string(),
                    Bytes::from_str("0x8f4E8439b970363648421C692dd897Fb9c0Bd1D9").unwrap(),
                    "getRate()".to_string(),
                ),
                TracingParams::RPCTracer(RPCTracerParams::new(
                    None,
                    Bytes::from(&keccak256("getRate()")[0..4]),
                )),
            ),
        ];
        let traced_entry_points = tracer
            .trace(
                // Block 22589134 hash
                Bytes::from_str(
                    "0x283666c6c90091fa168ebf52c0c61043d6ada7a2ffe10dc303b0e4ff111e172e",
                )
                .unwrap(),
                entry_points.clone(),
            )
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(
            traced_entry_points,
            vec![
                TracedEntryPoint {
                    entry_point_with_params: entry_points[0].clone(),
                    detection_block_hash: Bytes::from_str("0x283666c6c90091fa168ebf52c0c61043d6ada7a2ffe10dc303b0e4ff111e172e").unwrap(),
                    tracing_result: TracingResult::new(
                        HashSet::from([
                        (
                            Bytes::from_str("0x7bc3485026ac48b6cf9baf0a377477fff5703af8").unwrap(),
                            AddressStorageLocation::new(Bytes::from_str("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc").unwrap(), 12),
                        ),
                        (
                            Bytes::from_str("0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2").unwrap(),
                            AddressStorageLocation::new(Bytes::from_str("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc").unwrap(), 12),
                        ),
                    ]),
                    HashMap::from([
                        (Bytes::from_str("0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2").unwrap(), HashSet::from([
                            Bytes::from_str("0xca6decca4edae0c692b2b0c41376a54b812edb060282d36e07a7060ccb58244d").unwrap(),
                            Bytes::from_str("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc").unwrap(),
                            Bytes::from_str("0xca6decca4edae0c692b2b0c41376a54b812edb060282d36e07a7060ccb58244f").unwrap(),
                        ])),
                        (Bytes::from_str("0x487c2c53c0866f0a73ae317bd1a28f63adcd9ad1").unwrap(), HashSet::new()),
                        (Bytes::from_str("0x9aeb8aaa1ca38634aa8c0c8933e7fb4d61091327").unwrap(), HashSet::new()),
                        (Bytes::from_str("0xedf63cce4ba70cbe74064b7687882e71ebb0e988").unwrap(), HashSet::new()),
                        (Bytes::from_str("0x7bc3485026ac48b6cf9baf0a377477fff5703af8").unwrap(), HashSet::from([
                            Bytes::from_str("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc").unwrap(),
                            Bytes::from_str("0x0773e532dfede91f04b12a73d3d2acd361424f41f76b4fb79f090161e36b4e00").unwrap(),
                        ])),
                        ]),
                    ),
                },
                TracedEntryPoint {
                    entry_point_with_params: entry_points[1].clone(),
                    detection_block_hash: Bytes::from_str("0x283666c6c90091fa168ebf52c0c61043d6ada7a2ffe10dc303b0e4ff111e172e").unwrap(),
                    tracing_result: TracingResult::new(
                        HashSet::from([
                            (
                            Bytes::from_str("0xd4fa2d31b7968e448877f69a96de69f5de8cd23e").unwrap(),
                            AddressStorageLocation::new(Bytes::from_str("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc").unwrap(), 12),
                        ),
                        (
                            Bytes::from_str("0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2").unwrap(),
                            AddressStorageLocation::new(Bytes::from_str("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc").unwrap(), 12),
                        ),
                    ]),
                    HashMap::from([
                        (Bytes::from_str("0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2").unwrap(), HashSet::from([
                            Bytes::from_str("0xed960c71bd5fa1333658850f076b35ec5565086b606556c3dd36a916b43ddf23").unwrap(),
                            Bytes::from_str("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc").unwrap(),
                            Bytes::from_str("0xed960c71bd5fa1333658850f076b35ec5565086b606556c3dd36a916b43ddf21").unwrap(),
                        ])),
                        (Bytes::from_str("0x487c2c53c0866f0a73ae317bd1a28f63adcd9ad1").unwrap(), HashSet::new()),
                        (Bytes::from_str("0x9aeb8aaa1ca38634aa8c0c8933e7fb4d61091327").unwrap(), HashSet::new()),
                        (Bytes::from_str("0x8f4e8439b970363648421c692dd897fb9c0bd1d9").unwrap(), HashSet::new()),
                        (Bytes::from_str("0xd4fa2d31b7968e448877f69a96de69f5de8cd23e").unwrap(), HashSet::from([
                            Bytes::from_str("0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc").unwrap(),
                            Bytes::from_str("0x0773e532dfede91f04b12a73d3d2acd361424f41f76b4fb79f090161e36b4e00").unwrap(),
                        ])),
                        ]),
                    ),
                },
            ],
        );
    }

    #[tokio::test]
    #[ignore = "requires a RPC connection"]
    /// This test traces a UniswapV2Router02 swapExactTokensForTokens call
    /// It uses an account with no balance and relies on tracer overrides for setting custom values
    /// for POLS token balance and allowance attributes
    async fn test_trace_univ2_swap() {
        let tracer = TestFixture::create_tracer();

        // Create state overrides for the POLS contract
        let mut state_overrides = BTreeMap::new();
        let pols_address = Bytes::from_str("0x83e6f1e41cdd28eaceb20cb649155049fac3d5aa").unwrap();

        // Create storage overrides
        let mut slots = BTreeMap::new();
        // Override POLS balance for the caller
        slots.insert(
            Bytes::from_str("0x563494035215327c9cc08a85694f34eab8bc22017bd383b01d83f2bb8c78aa91")
                .unwrap(),
            Bytes::from_str("0x00000000000000000000000000000000000000000000004c4c6e64f5134a0000")
                .unwrap(),
        );
        // Override POLS allowance for the caller to UniswapV2Router02 contract
        slots.insert(
            Bytes::from_str("0x6402d480789caf1f1824771fcdd31558cac90b7d044d14b2201c8ca95eae8955")
                .unwrap(),
            Bytes::from_str("0x00000000000000000000000000000000000000000000004c4c6e64f5134a0000")
                .unwrap(),
        );

        // Create account overrides
        let account_overrides = AccountOverrides {
            slots: Some(StorageOverride::Diff(slots)),
            native_balance: None,
            code: None,
        };

        // Add to the state overrides map
        state_overrides.insert(pols_address.clone(), account_overrides);

        // UniswapV2Router02 address on Ethereum mainnet
        let router_address = Bytes::from_str("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D").unwrap();

        // Prepare swapExactTokensForTokens parameters
        // Function signature: swapExactTokensForTokens(uint amountIn, uint amountOutMin, address[]
        // path, address to, uint deadline) Function selector: 0x38ed1739

        // Parameters:
        // amountIn: 1407460000000000000000 - amount of POLS
        // amountOutMin: 105047450000000000 - minimum amount of WETH
        // path: [POLS, WETH] - token swap path
        // to: caller address - recipient of the swapped tokens
        // deadline: 1750085651

        let caller = Bytes::from_str("0xd0a3dAC187ab0CbAaE92127F143A31fB6badbabe").unwrap();

        // Construct calldata for swapExactTokensForTokens
        let calldata = Bytes::from(
            "0x38ed173900000000000000000000000000000000000000000000004c4c6e64f5134a00000000000000000000000000000000000000000000000000000175341965cf840000000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000d0a3dac187ab0cbaae92127f143a31fb6badbabe0000000000000000000000000000000000000000000000000000000068503013000000000000000000000000000000000000000000000000000000000000000200000000000000000000000083e6f1e41cdd28eaceb20cb649155049fac3d5aa000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
        );

        let entry_points = vec![EntryPointWithTracingParams::new(
            EntryPoint::new(
                "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D:swapExactTokensForTokens(uint amountIn, uint amountOutMin, address[] path, address to, uint deadline)"
                    .to_string(),
                router_address.clone(),
                "swapExactTokensForTokens(uint amountIn, uint amountOutMin, address[] path, address to, uint deadline)".to_string(),
            ),
            TracingParams::RPCTracer(RPCTracerParams::new(
                Some(caller.clone()),
                calldata,
            ).with_state_overrides(state_overrides)),
        )];

        let block_hash =
            Bytes::from_str("0xfebbe1110db8fd453b7125860a1c909561d00872aedb40765f54356ac4d7cc40")
                .unwrap();
        let traced_entry_points = tracer
            .trace(
                // 22717805 block hash
                block_hash.clone(),
                entry_points.clone(),
            )
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(
            traced_entry_points,
            vec ![
            TracedEntryPoint {
            entry_point_with_params: entry_points[0].clone(),
            detection_block_hash: block_hash,
            tracing_result: TracingResult::new(
            // Retriggers
            HashSet::from([
                    (
                        Bytes::from_str("0xffa98a091331df4600f87c9164cd27e8a5cd2405").unwrap(),
                        AddressStorageLocation::new(Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000007").unwrap(), 12),
                    ),
                    (
                        Bytes::from_str("0xffa98a091331df4600f87c9164cd27e8a5cd2405").unwrap(),
                        AddressStorageLocation::new(Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000006").unwrap(), 12),
                    ),
                ]),
            // Accessed slots
            HashMap::from([
            (
                Bytes::from_str("0xffa98a091331df4600f87c9164cd27e8a5cd2405").unwrap(),
                HashSet::from([
                    Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000007").unwrap(),
                    Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000009").unwrap(),
                    Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000006").unwrap(),
                    Bytes::from_str("0x000000000000000000000000000000000000000000000000000000000000000c").unwrap(),
                    Bytes::from_str("0x000000000000000000000000000000000000000000000000000000000000000a").unwrap(),
                    Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000008").unwrap(),
                ])
            ),
            (Bytes::from_str("0x7a250d5630b4cf539739df2c5dacb4c659f2488d").unwrap(), HashSet::new()),
            (
                Bytes::from_str("0x83e6f1e41cdd28eaceb20cb649155049fac3d5aa").unwrap(),
                HashSet::from([
                    Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000003").unwrap(),
                    Bytes::from_str("0x563494035215327c9cc08a85694f34eab8bc22017bd383b01d83f2bb8c78aa91").unwrap(),
                    Bytes::from_str("0x6402d480789caf1f1824771fcdd31558cac90b7d044d14b2201c8ca95eae8955").unwrap(),
                    Bytes::from_str("0x517313a419aa2ecd2d81b1726218564c7f0e0ab3a7f7ab9d34edc89c63e5f354").unwrap(),
                ])
            ),
            (
                Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap(),
                HashSet::from([
                    Bytes::from_str("0xcafe3db63107f22b0a41ab8ae57012c28217ebfcf75e49a58208dc6968d7ff57").unwrap(),
                    Bytes::from_str("0x732054380c06f66b946fe3c55339b1fc707995878c89c46f3c874fa55acf3188").unwrap(),
                ])
            ),
            ]),
            ),
            },
            ],
        );
    }

    #[tokio::test]
    #[ignore = "requires a RPC connection"]
    async fn test_trace_balancer_v2_stable_pool() {
        let tracer = TestFixture::create_tracer();
        let entry_points = vec![EntryPointWithTracingParams::new(
            EntryPoint::new(
                "1a8f81c256aee9c640e14bb0453ce247ea0dfe6f:getRate()".to_string(),
                Bytes::from_str("1a8f81c256aee9c640e14bb0453ce247ea0dfe6f").unwrap(),
                "getRate()".to_string(),
            ),
            TracingParams::RPCTracer(RPCTracerParams::new(
                None,
                Bytes::from(&keccak256("getRate()").to_vec()[0..4]),
            )),
        )];
        let traced_entry_points = tracer
            .trace(
                // Block 22589134 hash
                Bytes::from_str(
                    "0xf5e2c5bc64ba61e1230e34b2d5d8906416633100919b477d17a7c6fd69cde31d",
                )
                .unwrap(),
                entry_points.clone(),
            )
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(
            traced_entry_points,
            vec![TracedEntryPoint {
                entry_point_with_params: entry_points[0].clone(),
                detection_block_hash: Bytes::from_str(
                    "0xf5e2c5bc64ba61e1230e34b2d5d8906416633100919b477d17a7c6fd69cde31d"
                )
                .unwrap(),
                tracing_result: TracingResult::new(
                    HashSet::from([
                        (
                            Bytes::from_str("0x1d8f8f00cfa6758d7be78336684788fb0ee0fa46")
                                .unwrap(),
                            AddressStorageLocation::new(
                                Bytes::from_str(
                                    "0x8c4f7d2b56bbfa7011cbc31adfb95271b91aa0cce467b7912ad0d1579d2335a7"
                                )
                                .unwrap(),
                                12
                            ),
                        ),
                        (
                            Bytes::from_str("0xae78736cd615f374d3085123a210448e74fc6393")
                                .unwrap(),
                            AddressStorageLocation::new(
                                Bytes::from_str(
                                    "0x0000000000000000000000000000000000000000000000000000000000000000"
                                )
                                .unwrap(),
                                11
                            ),
                        ),
                        (
                            Bytes::from_str("0x6cc65bf618f55ce2433f9d8d827fc44117d81399")
                                .unwrap(),
                            AddressStorageLocation::new(
                                Bytes::from_str(
                                    "0x0000000000000000000000000000000000000000000000000000000000000000"
                                )
                                .unwrap(),
                                11
                            ),
                        ),
                    ]),
                    HashMap::from([
                        (
                            Bytes::from_str("0xae78736cd615f374d3085123a210448e74fc6393")
                                .unwrap(),
                            HashSet::from([Bytes::from_str(
                                "0x0000000000000000000000000000000000000000000000000000000000000000"
                            )
                            .unwrap(),])
                        ),
                        (
                            Bytes::from_str("0x6cc65bf618f55ce2433f9d8d827fc44117d81399")
                                .unwrap(),
                            HashSet::from([Bytes::from_str(
                                "0x0000000000000000000000000000000000000000000000000000000000000000"
                            )
                            .unwrap(),])
                        ),
                        (
                            Bytes::from_str("0x1a8f81c256aee9c640e14bb0453ce247ea0dfe6f")
                                .unwrap(),
                            HashSet::new()
                        ),
                        (
                            Bytes::from_str("0x1d8f8f00cfa6758d7be78336684788fb0ee0fa46")
                                .unwrap(),
                            HashSet::from([
                                Bytes::from_str(
                                    "0x24661fa43cfcbbce20f500b5c12b4977ba40531f28ba0cac1a442597c665df81"
                                )
                                .unwrap(),
                                Bytes::from_str(
                                    "0x8c4f7d2b56bbfa7011cbc31adfb95271b91aa0cce467b7912ad0d1579d2335a7"
                                )
                                .unwrap(),
                                Bytes::from_str(
                                    "0x8a5e638174d8aca32e35588efcfb56d7d7c41fa53d63eea9543249b914ad2cf8"
                                )
                                .unwrap(),
                            ])
                        ),
                    ]),
                ),
            },]
        );
    }

    #[tokio::test]
    #[ignore = "requires a RPC connection"]
    async fn test_trace_failing_call() {
        let tracer = TestFixture::create_tracer();
        let entry_points = vec![EntryPointWithTracingParams::new(
            EntryPoint::new(
                "1a8f81c256aee9c640e14bb0453ce247ea0dfe6f:unknown()".to_string(),
                Bytes::from_str("1a8f81c256aee9c640e14bb0453ce247ea0dfe6f").unwrap(),
                "unknown()".to_string(),
            ),
            TracingParams::RPCTracer(RPCTracerParams::new(
                None,
                Bytes::from(&keccak256("unknown()").to_vec()[0..4]),
            )),
        )];
        let traced_entry_points = tracer
            .trace(
                // Block 22589134 hash
                Bytes::from_str(
                    "0xf5e2c5bc64ba61e1230e34b2d5d8906416633100919b477d17a7c6fd69cde31d",
                )
                .unwrap(),
                entry_points.clone(),
            )
            .await;

        assert_eq!(traced_entry_points.len(), 1);
        dbg!(&traced_entry_points[0]);
        assert!(matches!(traced_entry_points[0], Err(RPCError::TracingFailure(_))));
    }

    #[tokio::test]
    #[ignore = "requires a RPC connection"]
    async fn test_trace_failing_rpc() {
        let url = "https://fake_rpc.com/eth";
        let rpc = EthereumRpcClient::new(url).unwrap();
        let tracer = EVMEntrypointService::new(&rpc);
        let entry_points = vec![EntryPointWithTracingParams::new(
            EntryPoint::new(
                "1a8f81c256aee9c640e14bb0453ce247ea0dfe6f:unknown()".to_string(),
                Bytes::from_str("1a8f81c256aee9c640e14bb0453ce247ea0dfe6f").unwrap(),
                "unknown()".to_string(),
            ),
            TracingParams::RPCTracer(RPCTracerParams::new(
                None,
                Bytes::from(&keccak256("unknown()").to_vec()[0..4]),
            )),
        )];
        let traced_entry_points = tracer
            .trace(
                // Block 22589134 hash
                Bytes::from_str(
                    "0xf5e2c5bc64ba61e1230e34b2d5d8906416633100919b477d17a7c6fd69cde31d",
                )
                .unwrap(),
                entry_points.clone(),
            )
            .await;

        assert_eq!(traced_entry_points.len(), 1);
        assert!(matches!(traced_entry_points[0], Err(RPCError::RequestError(_))));
    }

    #[tokio::test]
    #[ignore = "requires a RPC connection"]
    // Test if the tracer catches account needed by some specific opcodes, such as BALANCE,
    // EXTCODESIZE, EXTCODECOPY, EXTCODEHASH. In this transaction, EXTCODESIZE is executed for
    // 0x0afbf798467f9b3b97f90d05bf7df592d89a6cf1.
    async fn test_trace_contains_eoa() {
        let tracer = TestFixture::create_tracer();

        let mut state_overrides = BTreeMap::new();

        // Insert simulation router code in overwrites
        let account_overrides = AccountOverrides {
            slots: None,
            native_balance: None,
            code: Some(Bytes::from_str("0x608060405234801561000f575f80fd5b506004361061004a575f3560e01c806309c5eabe1461004e57806391dd73461461006a578063d737d0c71461009a578063dc4c90d3146100b8575b5f80fd5b61006860048036038101906100639190611ac4565b6100d6565b005b610084600480360381019061007f9190611ac4565b6100e4565b6040516100919190611b7f565b60405180910390f35b6100a261017d565b6040516100af9190611bde565b60405180910390f35b6100c0610184565b6040516100cd9190611c52565b60405180910390f35b6100e082826101a8565b5050565b60607f000000000000000000000000000000000004444c5dc75cb358380d2e3de08a9073ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff161461016b576040517fae18210a00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b610175838361024b565b905092915050565b5f30905090565b7f000000000000000000000000000000000004444c5dc75cb358380d2e3de08a9081565b7f000000000000000000000000000000000004444c5dc75cb358380d2e3de08a9073ffffffffffffffffffffffffffffffffffffffff166348c8949183836040518363ffffffff1660e01b8152600401610203929190611ca5565b5f604051808303815f875af115801561021e573d5f803e3d5ffd5b505050506040513d5f823e3d601f19601f820116820180604052508101906102469190611de1565b505050565b6060365f365f61025b878761028a565b935093509350935061026f84848484610339565b60405180602001604052805f81525094505050505092915050565b365f365f604086351860608701945063ffffffff6040880135169350606063ffffffe0601f86011601806020890135188217915080880163ffffffff81351693506020810194508360051b805f5b8281101561030f578088013582811887179650808901602063ffffffe0601f833501160180850194505050506020810190506102d8565b508087018b8b011085171561032b57633b99b53d5f526004601cfd5b505050505092959194509250565b5f84849050905082829050811461037c576040517faaad13f700000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b5f5b818110156103e8575f86868381811061039a57610399611e28565b5b9050013560f81c60f81b60f81c60ff1690506103da818686858181106103c3576103c2611e28565b5b90506020028101906103d59190611e61565b6103f0565b50808060010191505061037e565b505050505050565b600b83101561048e576007830361041d573661040c83836106a9565b9050610417816106cc565b506106a4565b600683036104415736610430838361089f565b905061043b816108c3565b506106a4565b6009830361046557366104548383610a5a565b905061045f81610a7d565b506106a4565b6008830361048957366104788383610c58565b905061048381610c7c565b506106a4565b610667565b600c8303610513575f806104a28484610e13565b915091505f6104b083610e3d565b9050818111156104f95781816040517f12bacdd30000000000000000000000000000000000000000000000000000000081526004016104f0929190611edb565b60405180910390fd5b61050b8361050561017d565b83610ee3565b5050506106a4565b600f8303610598575f806105278484610e13565b915091505f610535836110cc565b90508181101561057e5781816040517f8b063d73000000000000000000000000000000000000000000000000000000008152600401610575929190611edb565b60405180910390fd5b6105908361058a61017d565b83611169565b5050506106a4565b600b83036105d7575f805f6105ad8585611201565b9250925092506105cf836105c083611233565b6105ca858761124f565b610ee3565b5050506106a4565b600e8303610616575f805f6105ec85856112d0565b92509250925061060e836105ff84611302565b610609848761138b565b611169565b5050506106a4565b60108303610666575f805f61062b85856112d0565b92509250925061065e8361063e84611302565b6106598461064b886110cc565b6113bf90919063ffffffff16565b611169565b5050506106a4565b5b826040517f5cda29d700000000000000000000000000000000000000000000000000000000815260040161069b9190611f02565b60405180910390fd5b505050565b3660a08210156106c057633b99b53d5f526004601cfd5b82358301905092915050565b5f8180602001906106dd9190611f1b565b905090505f80835f0160208101906106f59190611fa7565b90505f84604001602081019061070b9190612017565b90505f6fffffffffffffffffffffffffffffffff16816fffffffffffffffffffffffffffffffff160361074c57610749610744836110cc565b61141d565b90505b365f5b85811015610807578680602001906107679190611f1b565b8281811061077857610777611e28565b5b905060200281019061078a9190612042565b91505f806107a1868561146f90919063ffffffff16565b915091506107df6107d78383886fffffffffffffffffffffffffffffffff165f038880608001906107d29190611e61565b61156a565b600f0b61169b565b9650869450835f0160208101906107f69190611fa7565b95505050808060010191505061074f565b5085606001602081019061081b9190612017565b6fffffffffffffffffffffffffffffffff16846fffffffffffffffffffffffffffffffff161015610897578560600160208101906108599190612017565b846040517f8b063d7300000000000000000000000000000000000000000000000000000000815260040161088e929190612099565b60405180910390fd5b505050505050565b366101408210156108b757633b99b53d5f526004601cfd5b82358301905092915050565b5f8160c00160208101906108d79190612017565b90505f6fffffffffffffffffffffffffffffffff16816fffffffffffffffffffffffffffffffff160361095d5761095a6109558360a001602081019061091d91906120f5565b61093b57835f0160200160208101906109369190611fa7565b610950565b835f015f01602081019061094f9190611fa7565b5b6110cc565b61141d565b90505b5f6109c46109bc845f018036038101906109779190612256565b8560a001602081019061098a91906120f5565b856fffffffffffffffffffffffffffffffff166109a6906122b7565b878061010001906109b79190611e61565b61156a565b600f0b61169b565b90508260e00160208101906109d99190612017565b6fffffffffffffffffffffffffffffffff16816fffffffffffffffffffffffffffffffff161015610a55578260e0016020810190610a179190612017565b816040517f8b063d73000000000000000000000000000000000000000000000000000000008152600401610a4c929190612099565b60405180910390fd5b505050565b3660a0821015610a7157633b99b53d5f526004601cfd5b82358301905092915050565b5f818060200190610a8e9190611f1b565b905090505f80836040016020810190610aa79190612017565b90505f845f016020810190610abc9190611fa7565b9050365f6fffffffffffffffffffffffffffffffff16836fffffffffffffffffffffffffffffffff1603610afe57610afb610af683610e3d565b61141d565b92505b5f8590505b5f811115610bc057868060200190610b1b9190611f1b565b60018303818110610b2f57610b2e611e28565b5b9050602002810190610b419190612042565b91505f80610b58858561146f90919063ffffffff16565b91509150610b97610b8d838315896fffffffffffffffffffffffffffffffff16888060800190610b889190611e61565b61156a565b600f0b5f0361141d565b9650869550835f016020810190610bae9190611fa7565b94505050808060019003915050610b03565b50856060016020810190610bd49190612017565b6fffffffffffffffffffffffffffffffff16846fffffffffffffffffffffffffffffffff161115610c5057856060016020810190610c129190612017565b846040517f12bacdd3000000000000000000000000000000000000000000000000000000008152600401610c47929190612099565b60405180910390fd5b505050505050565b36610140821015610c7057633b99b53d5f526004601cfd5b82358301905092915050565b5f8160c0016020810190610c909190612017565b90505f6fffffffffffffffffffffffffffffffff16816fffffffffffffffffffffffffffffffff1603610d1657610d13610d0e8360a0016020810190610cd691906120f5565b610cf357835f015f016020810190610cee9190611fa7565b610d09565b835f016020016020810190610d089190611fa7565b5b610e3d565b61141d565b90505b5f610d7d610d6c845f01803603810190610d309190612256565b8560a0016020810190610d4391906120f5565b856fffffffffffffffffffffffffffffffff1687806101000190610d679190611e61565b61156a565b600f0b610d78906122b7565b61141d565b90508260e0016020810190610d929190612017565b6fffffffffffffffffffffffffffffffff16816fffffffffffffffffffffffffffffffff161115610e0e578260e0016020810190610dd09190612017565b816040517f12bacdd3000000000000000000000000000000000000000000000000000000008152600401610e05929190612099565b60405180910390fd5b505050565b5f806040831015610e2b57633b99b53d5f526004601cfd5b83359150602084013590509250929050565b5f80610e8a30847f000000000000000000000000000000000004444c5dc75cb358380d2e3de08a9073ffffffffffffffffffffffffffffffffffffffff166116df9092919063ffffffff16565b90505f811315610ed157826040517f3351b260000000000000000000000000000000000000000000000000000000008152600401610ec8919061231d565b60405180910390fd5b80610edb906122b7565b915050919050565b5f8103156110c7577f000000000000000000000000000000000004444c5dc75cb358380d2e3de08a9073ffffffffffffffffffffffffffffffffffffffff1663a5841194846040518263ffffffff1660e01b8152600401610f44919061231d565b5f604051808303815f87803b158015610f5b575f80fd5b505af1158015610f6d573d5f803e3d5ffd5b50505050610f908373ffffffffffffffffffffffffffffffffffffffff1661179e565b1561102b577f000000000000000000000000000000000004444c5dc75cb358380d2e3de08a9073ffffffffffffffffffffffffffffffffffffffff166311da60b4826040518263ffffffff1660e01b815260040160206040518083038185885af1158015611000573d5f803e3d5ffd5b50505050506040513d601f19601f820116820180604052508101906110259190612360565b506110c6565b6110368383836117d5565b7f000000000000000000000000000000000004444c5dc75cb358380d2e3de08a9073ffffffffffffffffffffffffffffffffffffffff166311da60b46040518163ffffffff1660e01b81526004016020604051808303815f875af11580156110a0573d5f803e3d5ffd5b505050506040513d601f19601f820116820180604052508101906110c49190612360565b505b5b505050565b5f8061111930847f000000000000000000000000000000000004444c5dc75cb358380d2e3de08a9073ffffffffffffffffffffffffffffffffffffffff166116df9092919063ffffffff16565b90505f81121561116057826040517f4c085bf1000000000000000000000000000000000000000000000000000000008152600401611157919061231d565b60405180910390fd5b80915050919050565b5f8103156111fc577f000000000000000000000000000000000004444c5dc75cb358380d2e3de08a9073ffffffffffffffffffffffffffffffffffffffff16630b0d9c098484846040518463ffffffff1660e01b81526004016111ce9392919061238b565b5f604051808303815f87803b1580156111e5575f80fd5b505af11580156111f7573d5f803e3d5ffd5b505050505b505050565b5f805f606084101561121a57633b99b53d5f526004601cfd5b8435925060208501359150604085013590509250925092565b5f8161123f5730611248565b61124761017d565b5b9050919050565b5f7f8000000000000000000000000000000000000000000000000000000000000000830361129d576112968273ffffffffffffffffffffffffffffffffffffffff16611882565b90506112ca565b5f6fffffffffffffffffffffffffffffffff1683036112c6576112bf82610e3d565b90506112ca565b8290505b92915050565b5f805f60608410156112e957633b99b53d5f526004601cfd5b8435925060208501359150604085013590509250925092565b5f600173ffffffffffffffffffffffffffffffffffffffff168273ffffffffffffffffffffffffffffffffffffffff16036113465761133f61017d565b9050611386565b600273ffffffffffffffffffffffffffffffffffffffff168273ffffffffffffffffffffffffffffffffffffffff160361138257309050611386565b8190505b919050565b5f806fffffffffffffffffffffffffffffffff1683036113b5576113ae826110cc565b90506113b9565b8290505b92915050565b5f6127108211156113fc576040517fdeaa01e600000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b612710828461140b91906123c0565b611415919061242e565b905092915050565b5f819050806fffffffffffffffffffffffffffffffff16821461146a576114696393dafdf160e01b7bffffffffffffffffffffffffffffffffffffffffffffffffffffffff191661192f565b5b919050565b6114776119de565b5f80845f01602081019061148b9190611fa7565b90505f806114998684611937565b6114a45782866114a7565b85835b915091506114b5868361196f565b93506040518060a001604052808373ffffffffffffffffffffffffffffffffffffffff1681526020018273ffffffffffffffffffffffffffffffffffffffff16815260200188602001602081019061150d919061245e565b62ffffff16815260200188604001602081019061152a9190612489565b60020b815260200188606001602081019061154591906124b4565b73ffffffffffffffffffffffffffffffffffffffff1681525094505050509250929050565b5f807f000000000000000000000000000000000004444c5dc75cb358380d2e3de08a9073ffffffffffffffffffffffffffffffffffffffff1663f3cd914c8860405180606001604052808a151581526020018981526020018a6115e457600173fffd8963efd1fc6a506488495d951d5263988d26036115ee565b60016401000276a3015b73ffffffffffffffffffffffffffffffffffffffff1681525087876040518563ffffffff1660e01b815260040161162894939291906125ff565b6020604051808303815f875af1158015611644573d5f803e3d5ffd5b505050506040513d601f19601f820116820180604052508101906116689190612669565b90505f851215158615151461168557611680816119a7565b61168f565b61168e816119b3565b5b91505095945050505050565b5f8082600f0b12156116d7576116d66393dafdf160e01b7bffffffffffffffffffffffffffffffffffffffffffffffffffffffff191661192f565b5b819050919050565b5f8073ffffffffffffffffffffffffffffffffffffffff84165f5273ffffffffffffffffffffffffffffffffffffffff831660205260405f2090508473ffffffffffffffffffffffffffffffffffffffff1663f135baaa826040518263ffffffff1660e01b815260040161175391906126ac565b602060405180830381865afa15801561176e573d5f803e3d5ffd5b505050506040513d601f19601f8201168201806040525081019061179291906126ef565b5f1c9150509392505050565b5f8073ffffffffffffffffffffffffffffffffffffffff168273ffffffffffffffffffffffffffffffffffffffff16149050919050565b7f000000000000000000000000000000000004444c5dc75cb358380d2e3de08a9073ffffffffffffffffffffffffffffffffffffffff1663f5298aca836118318673ffffffffffffffffffffffffffffffffffffffff166119bf565b846040518463ffffffff1660e01b81526004016118509392919061271a565b5f604051808303815f87803b158015611867575f80fd5b505af1158015611879573d5f803e3d5ffd5b50505050505050565b5f6118a28273ffffffffffffffffffffffffffffffffffffffff1661179e565b156118af5747905061192a565b8173ffffffffffffffffffffffffffffffffffffffff166370a08231306040518263ffffffff1660e01b81526004016118e89190611bde565b602060405180830381865afa158015611903573d5f803e3d5ffd5b505050506040513d601f19601f820116820180604052508101906119279190612360565b90505b919050565b805f5260045ffd5b5f8173ffffffffffffffffffffffffffffffffffffffff168373ffffffffffffffffffffffffffffffffffffffff1610905092915050565b5f8173ffffffffffffffffffffffffffffffffffffffff168373ffffffffffffffffffffffffffffffffffffffff1614905092915050565b5f8160801d9050919050565b5f81600f0b9050919050565b5f8173ffffffffffffffffffffffffffffffffffffffff169050919050565b6040518060a001604052805f73ffffffffffffffffffffffffffffffffffffffff1681526020015f73ffffffffffffffffffffffffffffffffffffffff1681526020015f62ffffff1681526020015f60020b81526020015f73ffffffffffffffffffffffffffffffffffffffff1681525090565b5f604051905090565b5f80fd5b5f80fd5b5f80fd5b5f80fd5b5f80fd5b5f8083601f840112611a8457611a83611a63565b5b8235905067ffffffffffffffff811115611aa157611aa0611a67565b5b602083019150836001820283011115611abd57611abc611a6b565b5b9250929050565b5f8060208385031215611ada57611ad9611a5b565b5b5f83013567ffffffffffffffff811115611af757611af6611a5f565b5b611b0385828601611a6f565b92509250509250929050565b5f81519050919050565b5f82825260208201905092915050565b8281835e5f83830152505050565b5f601f19601f8301169050919050565b5f611b5182611b0f565b611b5b8185611b19565b9350611b6b818560208601611b29565b611b7481611b37565b840191505092915050565b5f6020820190508181035f830152611b978184611b47565b905092915050565b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f611bc882611b9f565b9050919050565b611bd881611bbe565b82525050565b5f602082019050611bf15f830184611bcf565b92915050565b5f819050919050565b5f611c1a611c15611c1084611b9f565b611bf7565b611b9f565b9050919050565b5f611c2b82611c00565b9050919050565b5f611c3c82611c21565b9050919050565b611c4c81611c32565b82525050565b5f602082019050611c655f830184611c43565b92915050565b828183375f83830152505050565b5f611c848385611b19565b9350611c91838584611c6b565b611c9a83611b37565b840190509392505050565b5f6020820190508181035f830152611cbe818486611c79565b90509392505050565b5f80fd5b7f4e487b71000000000000000000000000000000000000000000000000000000005f52604160045260245ffd5b611d0182611b37565b810181811067ffffffffffffffff82111715611d2057611d1f611ccb565b5b80604052505050565b5f611d32611a52565b9050611d3e8282611cf8565b919050565b5f67ffffffffffffffff821115611d5d57611d5c611ccb565b5b611d6682611b37565b9050602081019050919050565b5f611d85611d8084611d43565b611d29565b905082815260208101848484011115611da157611da0611cc7565b5b611dac848285611b29565b509392505050565b5f82601f830112611dc857611dc7611a63565b5b8151611dd8848260208601611d73565b91505092915050565b5f60208284031215611df657611df5611a5b565b5b5f82015167ffffffffffffffff811115611e1357611e12611a5f565b5b611e1f84828501611db4565b91505092915050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52603260045260245ffd5b5f80fd5b5f80fd5b5f80fd5b5f8083356001602003843603038112611e7d57611e7c611e55565b5b80840192508235915067ffffffffffffffff821115611e9f57611e9e611e59565b5b602083019250600182023603831315611ebb57611eba611e5d565b5b509250929050565b5f819050919050565b611ed581611ec3565b82525050565b5f604082019050611eee5f830185611ecc565b611efb6020830184611ecc565b9392505050565b5f602082019050611f155f830184611ecc565b92915050565b5f8083356001602003843603038112611f3757611f36611e55565b5b80840192508235915067ffffffffffffffff821115611f5957611f58611e59565b5b602083019250602082023603831315611f7557611f74611e5d565b5b509250929050565b611f8681611bbe565b8114611f90575f80fd5b50565b5f81359050611fa181611f7d565b92915050565b5f60208284031215611fbc57611fbb611a5b565b5b5f611fc984828501611f93565b91505092915050565b5f6fffffffffffffffffffffffffffffffff82169050919050565b611ff681611fd2565b8114612000575f80fd5b50565b5f8135905061201181611fed565b92915050565b5f6020828403121561202c5761202b611a5b565b5b5f61203984828501612003565b91505092915050565b5f8235600160a00383360303811261205d5761205c611e55565b5b80830191505092915050565b5f61208361207e61207984611fd2565b611bf7565b611ec3565b9050919050565b61209381612069565b82525050565b5f6040820190506120ac5f83018561208a565b6120b9602083018461208a565b9392505050565b5f8115159050919050565b6120d4816120c0565b81146120de575f80fd5b50565b5f813590506120ef816120cb565b92915050565b5f6020828403121561210a57612109611a5b565b5b5f612117848285016120e1565b91505092915050565b5f80fd5b5f62ffffff82169050919050565b61213b81612124565b8114612145575f80fd5b50565b5f8135905061215681612132565b92915050565b5f8160020b9050919050565b6121718161215c565b811461217b575f80fd5b50565b5f8135905061218c81612168565b92915050565b5f61219c82611bbe565b9050919050565b6121ac81612192565b81146121b6575f80fd5b50565b5f813590506121c7816121a3565b92915050565b5f60a082840312156121e2576121e1612120565b5b6121ec60a0611d29565b90505f6121fb84828501611f93565b5f83015250602061220e84828501611f93565b602083015250604061222284828501612148565b60408301525060606122368482850161217e565b606083015250608061224a848285016121b9565b60808301525092915050565b5f60a0828403121561226b5761226a611a5b565b5b5f612278848285016121cd565b91505092915050565b5f819050919050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b5f6122c182612281565b91507f800000000000000000000000000000000000000000000000000000000000000082036122f3576122f261228a565b5b815f039050919050565b5f61230782611c21565b9050919050565b612317816122fd565b82525050565b5f6020820190506123305f83018461230e565b92915050565b61233f81611ec3565b8114612349575f80fd5b50565b5f8151905061235a81612336565b92915050565b5f6020828403121561237557612374611a5b565b5b5f6123828482850161234c565b91505092915050565b5f60608201905061239e5f83018661230e565b6123ab6020830185611bcf565b6123b86040830184611ecc565b949350505050565b5f6123ca82611ec3565b91506123d583611ec3565b92508282026123e381611ec3565b915082820484148315176123fa576123f961228a565b5b5092915050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601260045260245ffd5b5f61243882611ec3565b915061244383611ec3565b92508261245357612452612401565b5b828204905092915050565b5f6020828403121561247357612472611a5b565b5b5f61248084828501612148565b91505092915050565b5f6020828403121561249e5761249d611a5b565b5b5f6124ab8482850161217e565b91505092915050565b5f602082840312156124c9576124c8611a5b565b5b5f6124d6848285016121b9565b91505092915050565b6124e8816122fd565b82525050565b6124f781612124565b82525050565b6125068161215c565b82525050565b5f61251682611c21565b9050919050565b6125268161250c565b82525050565b60a082015f8201516125405f8501826124df565b50602082015161255360208501826124df565b50604082015161256660408501826124ee565b50606082015161257960608501826124fd565b50608082015161258c608085018261251d565b50505050565b61259b816120c0565b82525050565b6125aa81612281565b82525050565b6125b981611b9f565b82525050565b606082015f8201516125d35f850182612592565b5060208201516125e660208501826125a1565b5060408201516125f960408501826125b0565b50505050565b5f610120820190506126135f83018761252c565b61262060a08301866125bf565b818103610100830152612634818486611c79565b905095945050505050565b61264881612281565b8114612652575f80fd5b50565b5f815190506126638161263f565b92915050565b5f6020828403121561267e5761267d611a5b565b5b5f61268b84828501612655565b91505092915050565b5f819050919050565b6126a681612694565b82525050565b5f6020820190506126bf5f83018461269d565b92915050565b6126ce81612694565b81146126d8575f80fd5b50565b5f815190506126e9816126c5565b92915050565b5f6020828403121561270457612703611a5b565b5b5f612711848285016126db565b91505092915050565b5f60608201905061272d5f830186611bcf565b61273a6020830185611ecc565b6127476040830184611ecc565b94935050505056fea26469706673582212204c6ac0e1cf8966001eed95185841399443cddfa3281f357602e8ddeafa74ea5a64736f6c634300081a0033").unwrap()),
        };
        state_overrides.insert(
            Bytes::from_str("0x2e234dae75c793f67a35089c9d99245e1c58470b").unwrap(),
            account_overrides,
        );

        let params = RPCTracerParams::new(None, Bytes::from("0x09c5eabe00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000340000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000003060c0f00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000001e00000000000000000000000000000000000000000000000000000000000000240000000000000000000000000000000000000000000000000000000000000016000000000000000000000000000000000000000000000000000000000000000200000000000000000000000007f39c581f595b53c5cb19bd0b3f8da6c935e2ca0000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc20000000000000000000000000000000000000000000000000000000000000064000000000000000000000000000000000000000000000000000000000000000100000000000000000000000055dcf9455eee8fd3f5eed17606291272cde428a80000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000055b2aa381e13dc000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000120000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000400000000000000000000000007f39c581f595b53c5cb19bd0b3f8da6c935e2ca0000000000000000000000000000000000000000000000000055b2aa381e13dc00000000000000000000000000000000000000000000000000000000000000040000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc20000000000000000000000000000000000000000000000000000000000000000"))
            .with_state_overrides(state_overrides);

        let entry_points = vec![EntryPointWithTracingParams::new(
            EntryPoint::new(
                "0x2e234dae75c793f67a35089c9d99245e1c58470b:execute(bytes)".to_string(),
                Bytes::from_str("0x2e234dae75c793f67a35089c9d99245e1c58470b").unwrap(),
                "execute(bytes)".to_string(),
            ),
            TracingParams::RPCTracer(params.clone()),
        )];

        let traced_entry_points = tracer
            .trace(
                // Block 23125980 hash
                Bytes::from_str(
                    "0xa8aa1c7b24af7d4d181a9cc8901be98f7751ba62b071033605d4d3cf0861afaf",
                )
                .unwrap(),
                entry_points.clone(),
            )
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(traced_entry_points[0]
            .tracing_result
            .accessed_slots
            .contains_key(&Bytes::from_str("0x0afbf798467f9b3b97f90d05bf7df592d89a6cf1").unwrap()));
    }

    #[tokio::test]
    async fn test_retry_with_mock_rpc_server() {
        // Create a mock RPC server that fails the first few requests
        let mut server = Server::new_async().await;

        // First two attempts fail with 429 errors
        let _m1 = server
            .mock("POST", "/")
            .with_status(429)
            .expect(1)
            .create_async()
            .await;

        let _m2 = server
            .mock("POST", "/")
            .with_status(429)
            .expect(1)
            .create_async()
            .await;

        // Third attempt succeeds with proper JSON-RPC batch response
        // IDs are 4 and 5 because the id counter increments per request (0, 1, 2, 3, 4, 5...)
        let _m3 = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(
                r#"[
                {
                    "jsonrpc": "2.0",
                    "id": 4,
                    "result": {
                        "accessList": [],
                        "gasUsed": "0x5dc0"
                    }
                },
                {
                    "jsonrpc": "2.0",
                    "id": 5,
                    "result": {
                        "0x0000000000000000000000000000000000000000": {
                            "balance": "0x2fda439328c1d25c3c5"
                        },
                        "0x0000000000000000000000000000000000000001": {
                            "balance": "0x31535e82bbce260fd"
                        },
                        "0x4838b106fce9647bdf1e7877bf73ce8b0bad5f97": {
                            "balance": "0x4ba0584a354bc705",
                            "nonce": 1735918
                        }
                    }
                }
            ]"#,
            )
            .expect(1)
            .create_async()
            .await;

        // Create tracer with fast retries
        let rpc =
            EthereumRpcClient::new(&server.url()).expect("Failed to create EthereumRpcClient");
        let tracer = EVMEntrypointService::new_with_config(
            &rpc, 3,  // max_retries
            10, // retry_delay_ms
        );

        // Create test entry points
        let entry_points: Vec<EntryPointWithTracingParams> =
            vec![EntryPointWithTracingParams::new(
                EntryPoint::new(
                    "first:func()".to_string(),
                    Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
                    "func()".to_string(),
                ),
                TracingParams::RPCTracer(RPCTracerParams::new(
                    None,
                    Bytes::from(&keccak256("func()")[0..4]),
                )),
            )];

        let block_hash =
            Bytes::from_str("0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")
                .unwrap();

        let results = tracer
            .trace(block_hash, entry_points)
            .await;

        _m3.assert();

        // Should return exactly one result
        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok());
    }

    #[tokio::test]
    async fn test_ordering_with_network_failures() {
        // Test with completely unreachable server to ensure ordering is preserved
        let rpc = EthereumRpcClient::new("http://127.0.0.1:1")
            .expect("Failed to create EthereumRpcClient");
        let tracer = EVMEntrypointService::new_with_config(&rpc, 1, 10);

        // Create entry points with distinguishable signatures for order verification
        let entry_points: Vec<EntryPointWithTracingParams> = vec![
            EntryPointWithTracingParams::new(
                EntryPoint::new(
                    "first:func()".to_string(),
                    Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
                    "func()".to_string(),
                ),
                TracingParams::RPCTracer(RPCTracerParams::new(
                    None,
                    Bytes::from(&keccak256("func()")[0..4]),
                )),
            ),
            EntryPointWithTracingParams::new(
                EntryPoint::new(
                    "second:func()".to_string(),
                    Bytes::from_str("0x0000000000000000000000000000000000000002").unwrap(),
                    "func()".to_string(),
                ),
                TracingParams::RPCTracer(RPCTracerParams::new(
                    None,
                    Bytes::from(&keccak256("func()")[0..4]),
                )),
            ),
            EntryPointWithTracingParams::new(
                EntryPoint::new(
                    "third:func()".to_string(),
                    Bytes::from_str("0x0000000000000000000000000000000000000003").unwrap(),
                    "func()".to_string(),
                ),
                TracingParams::RPCTracer(RPCTracerParams::new(
                    None,
                    Bytes::from(&keccak256("func()")[0..4]),
                )),
            ),
        ];

        let block_hash =
            Bytes::from_str("0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")
                .unwrap();
        let results = tracer
            .trace(block_hash, entry_points.clone())
            .await;

        // All should fail but order should be preserved
        assert_eq!(results.len(), 3);

        // Verify all results are RequestError
        for result in &results {
            assert!(matches!(result, Err(RPCError::RequestError(RequestError::Reqwest(_)))));
        }

        // Verify ordering is preserved by checking that error messages contain the expected target
        // addresses
        for (i, result) in results.iter().enumerate() {
            if let Err(RPCError::RequestError(RequestError::Reqwest(ReqwestError {
                msg,
                source: _,
            }))) = result
            {
                let expected_target = &entry_points[i].entry_point.target;
                assert!(
                    msg.contains(&expected_target.to_string()),
                    "Error message '{msg}' should contain target address '{expected_target}' for entry point at index {i}",
                );
            }
        }
    }

    #[tokio::test]
    async fn test_ordering_with_partial_failures() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        // Create a mock RPC server that fails only the second request
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let call_count = call_count_clone.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0; 4096];
                    if let Ok(n) = socket.read(&mut buffer).await {
                        let request = String::from_utf8_lossy(&buffer[..n]);
                        let current_call = call_count.fetch_add(1, Ordering::SeqCst);

                        // Determine which entry point this request is for by looking at the target
                        // address
                        let is_second_request =
                            request.contains("0x0000000000000000000000000000000000000002");

                        let response = if is_second_request {
                            // Fail only the second entry point request
                            return; // Close connection to simulate network failure
                        } else {
                            // Success response for first and third requests
                            let first_id = current_call * 2;
                            let second_id = current_call * 2 + 1;

                            format!(
                                r#"[
                                {{
                                    "jsonrpc": "2.0",
                                    "id": {first_id},
                                    "result": {{
                                        "accessList": [],
                                        "gasUsed": "0x5dc0"
                                    }}
                                }},
                                {{
                                    "jsonrpc": "2.0",
                                    "id": {second_id},
                                    "result": {{
                                        "0x0000000000000000000000000000000000000000": {{
                                            "balance": "0x2fda439328c1d25c3c5"
                                        }},
                                        "0x0000000000000000000000000000000000000001": {{
                                            "balance": "0x31535e82bbce260fd"
                                        }}
                                    }}
                                }}
                            ]"#
                            )
                        };

                        let http_response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            response.len(),
                            response
                        );

                        let _ = socket
                            .write_all(http_response.as_bytes())
                            .await;
                    }
                });
            }
        });

        // Give the server a moment to start
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Create tracer with no retries to make the test faster
        let rpc = EthereumRpcClient::new(&format!("http://127.0.0.1:{}", addr.port()))
            .expect("Failed to create EthereumRpcClient");
        let tracer = EVMEntrypointService::new_with_config(
            &rpc, 0,  // no retries
            10, // retry_delay_ms (not used since max_retries=0)
        );

        // Create three test entry points - the middle one should fail
        let entry_points: Vec<EntryPointWithTracingParams> = vec![
            EntryPointWithTracingParams::new(
                EntryPoint::new(
                    "first:func()".to_string(),
                    Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
                    "func()".to_string(),
                ),
                TracingParams::RPCTracer(RPCTracerParams::new(
                    None,
                    Bytes::from(&keccak256("func()")[0..4]),
                )),
            ),
            EntryPointWithTracingParams::new(
                EntryPoint::new(
                    "second:func()".to_string(),
                    Bytes::from_str("0x0000000000000000000000000000000000000002").unwrap(),
                    "func()".to_string(),
                ),
                TracingParams::RPCTracer(RPCTracerParams::new(
                    None,
                    Bytes::from(&keccak256("func()")[0..4]),
                )),
            ),
            EntryPointWithTracingParams::new(
                EntryPoint::new(
                    "third:func()".to_string(),
                    Bytes::from_str("0x0000000000000000000000000000000000000003").unwrap(),
                    "func()".to_string(),
                ),
                TracingParams::RPCTracer(RPCTracerParams::new(
                    None,
                    Bytes::from(&keccak256("func()")[0..4]),
                )),
            ),
        ];

        let block_hash =
            Bytes::from_str("0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")
                .unwrap();

        let results = tracer
            .trace(block_hash, entry_points.clone())
            .await;

        // Should return exactly 3 results in the same order
        assert_eq!(results.len(), 3);

        // First result should be success
        match &results[0] {
            Ok(traced_entry_point) => {
                assert_eq!(
                    traced_entry_point
                        .entry_point_with_params
                        .entry_point
                        .target,
                    Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap()
                );
            }
            Err(e) => {
                panic!("Expected first request to succeed, but got error: {e:?}");
            }
        }

        // Second result should be a RequestError (network failure)
        match &results[1] {
            Ok(_) => {
                panic!("Expected second request to fail, but it succeeded");
            }
            Err(RPCError::RequestError(RequestError::Reqwest(ReqwestError { msg, source: _ }))) => {
                assert!(
                    msg.contains("0x0000000000000000000000000000000000000002"),
                    "Error message should contain the target address of the failed request"
                );
            }
            Err(e) => {
                panic!("Expected RequestError for second request, but got: {e:?}");
            }
        }

        // Third result should be success
        match &results[2] {
            Ok(traced_entry_point) => {
                assert_eq!(
                    traced_entry_point
                        .entry_point_with_params
                        .entry_point
                        .target,
                    Bytes::from_str("0x0000000000000000000000000000000000000003").unwrap()
                );
            }
            Err(e) => {
                panic!("Expected third request to succeed, but got error: {e:?}");
            }
        }
    }

    #[test]
    fn test_detect_retrigger_specific_example() {
        use std::str::FromStr;

        use alloy::primitives::B256;

        // User's specific example:
        // Called address: 0x001442309e82b3e69d9cf520e318c62a64fa190c
        // Packed slot: 0x00000bbd0f9dd77fc77b0000001442309e82b3e69d9cf520e318c62a64fa190c
        // Expected offset: 12

        let called_address =
            Address::from_str("0x001442309e82b3e69d9cf520e318c62a64fa190c").unwrap();
        let mut called_addresses = HashSet::new();
        called_addresses.insert(called_address);

        let slot =
            B256::from_str("0x0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let packed_value =
            B256::from_str("0x00000bbd0f9dd77fc77b0000001442309e82b3e69d9cf520e318c62a64fa190c")
                .unwrap();

        let result =
            EVMEntrypointService::detect_retrigger(&called_addresses, &slot, &packed_value);

        assert!(result.is_some());
        let storage_location = result.unwrap();
        assert_eq!(storage_location.offset, 12);
        assert_eq!(storage_location.key, tycho_common::Bytes::from(slot.as_slice()));
    }

    #[test]
    fn test_detect_retrigger_offset_zero() {
        use std::str::FromStr;

        use alloy::primitives::B256;

        // Address at the beginning of the slot (offset 0)
        let called_address =
            Address::from_str("0x1234567890123456789012345678901234567890").unwrap();
        let mut called_addresses = HashSet::new();
        called_addresses.insert(called_address);

        let slot =
            B256::from_str("0x0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        // Address at offset 0, followed by 12 bytes of zeros
        let packed_value =
            B256::from_str("0x1234567890123456789012345678901234567890000000000000000000000000")
                .unwrap();

        let result =
            EVMEntrypointService::detect_retrigger(&called_addresses, &slot, &packed_value);

        assert!(result.is_some());
        let storage_location = result.unwrap();
        assert_eq!(storage_location.offset, 0);
        assert_eq!(storage_location.key, tycho_common::Bytes::from(slot.as_slice()));
    }

    #[test]
    fn test_detect_retrigger_offset_twelve() {
        use std::str::FromStr;

        use alloy::primitives::B256;

        // Address at offset 12 (end of slot)
        let called_address =
            Address::from_str("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap();
        let mut called_addresses = HashSet::new();
        called_addresses.insert(called_address);

        let slot =
            B256::from_str("0x0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        // 12 bytes of data, then address at offset 12
        let packed_value =
            B256::from_str("0x000102030405060708090a0babcdefabcdefabcdefabcdefabcdefabcdefabcd")
                .unwrap();

        let result =
            EVMEntrypointService::detect_retrigger(&called_addresses, &slot, &packed_value);

        assert!(result.is_some());
        let storage_location = result.unwrap();
        assert_eq!(storage_location.offset, 12);
        assert_eq!(storage_location.key, tycho_common::Bytes::from(slot.as_slice()));
    }

    #[test]
    fn test_detect_retrigger_no_match() {
        use std::str::FromStr;

        use alloy::primitives::B256;

        // Address not present in the storage value
        let called_address =
            Address::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let mut called_addresses = HashSet::new();
        called_addresses.insert(called_address);

        let slot =
            B256::from_str("0x0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        // Storage value containing a different address
        let packed_value =
            B256::from_str("0x00000bbd0f9dd77fc77b00000022222222222222222222222222222222222222")
                .unwrap();

        let result =
            EVMEntrypointService::detect_retrigger(&called_addresses, &slot, &packed_value);

        assert!(result.is_none());
    }

    /// Test batch response handling with different response orderings
    /// This verifies that the client correctly matches responses by ID, not by array position
    #[rstest]
    #[case::correct_order(vec![0, 1])]
    #[case::reversed_order(vec![1, 0])]
    #[tokio::test]
    async fn test_batch_response_ordering(#[case] id_order: Vec<usize>) {
        // Create a mock RPC server that returns responses in various orderings
        let mut server = Server::new_async().await;

        // Define the two response bodies
        let access_list_response = r#"{
            "jsonrpc": "2.0",
            "id": 0,
            "result": {
                "accessList": [
                    {
                        "address": "0x0000000000000000000000000000000000000001",
                        "storageKeys": ["0x0000000000000000000000000000000000000000000000000000000000000001"]
                    }
                ],
                "gasUsed": "0x5dc0"
            }
        }"#;

        let trace_response = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "0x0000000000000000000000000000000000000001": {
                    "balance": "0x1"
                }
            }
        }"#;

        // Order responses based on the test case
        let responses = [access_list_response, trace_response];
        let ordered_responses: Vec<&str> = id_order
            .iter()
            .map(|&i| responses[i])
            .collect();
        let response_body = format!("[{}]", ordered_responses.join(","));

        let _m = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(response_body)
            .expect(1)
            .create_async()
            .await;

        let rpc =
            EthereumRpcClient::new(&server.url()).expect("Failed to create EthereumRpcClient");
        let tracer = EVMEntrypointService::new_with_config(&rpc, 0, 10);

        let entry_points = vec![EntryPointWithTracingParams::new(
            EntryPoint::new(
                "test:func()".to_string(),
                Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
                "func()".to_string(),
            ),
            TracingParams::RPCTracer(RPCTracerParams::new(
                None,
                Bytes::from(&keccak256("func()")[0..4]),
            )),
        )];

        let block_hash =
            Bytes::from_str("0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")
                .unwrap();

        let results = tracer
            .trace(block_hash, entry_points)
            .await;

        _m.assert();

        assert_eq!(results.len(), 1);
        assert!(
            results[0].is_ok(),
            "Expected success regardless of response order, got: {:?}",
            results[0]
        );
    }

    #[tokio::test]
    async fn test_batch_response_missing_id() {
        // Create a mock RPC server that returns a response with a missing ID
        // We send two requests (id=0 for access list, id=1 for trace) but only get back id=0
        let mut server = Server::new_async().await;

        // Mock response with only id=0, missing id=1
        let _m = server
            .mock("POST", "/")
            .with_status(200)
            .with_body(
                r#"[
                {
                    "jsonrpc": "2.0",
                    "id": 0,
                    "result": {
                        "accessList": [],
                        "gasUsed": "0x5dc0"
                    }
                }
            ]"#,
            )
            .expect(1)
            .create_async()
            .await;

        let rpc =
            EthereumRpcClient::new(&server.url()).expect("Failed to create EthereumRpcClient");
        let tracer = EVMEntrypointService::new_with_config(&rpc, 0, 10);

        let entry_points = vec![EntryPointWithTracingParams::new(
            EntryPoint::new(
                "test:func()".to_string(),
                Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
                "func()".to_string(),
            ),
            TracingParams::RPCTracer(RPCTracerParams::new(
                None,
                Bytes::from(&keccak256("func()")[0..4]),
            )),
        )];

        let block_hash =
            Bytes::from_str("0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef")
                .unwrap();

        let results = tracer
            .trace(block_hash, entry_points)
            .await;

        _m.assert();
        assert_eq!(results.len(), 1);
        // Should fail because batch response doesn't have correct length
        assert!(matches!(results[0], Err(RPCError::RequestError(_))));
    }

    /// Integration test using real RPC to verify the fix works end-to-end.
    /// We test with different contract combinations to increase confidence
    /// that the ID matching works correctly with real RPC responses.
    #[rstest]
    #[case::two_contracts(vec![
        ("0xEdf63cce4bA70cbE74064b7687882E71ebB0e988", "getRate()"),
        ("0x8f4E8439b970363648421C692dd897Fb9c0Bd1D9", "getRate()"),
    ])]
    #[case::single_contract_first(vec![
        ("0x8f4E8439b970363648421C692dd897Fb9c0Bd1D9", "getRate()"),
    ])]
    #[case::single_contract_second(vec![
        ("0xEdf63cce4bA70cbE74064b7687882E71ebB0e988", "getRate()"),
    ])]
    #[tokio::test]
    #[ignore = "requires a RPC connection"]
    async fn test_batch_response_with_real_rpc(#[case] contracts: Vec<(&str, &str)>) {
        let tracer = TestFixture::create_tracer();

        let block_hash =
            Bytes::from_str("0x283666c6c90091fa168ebf52c0c61043d6ada7a2ffe10dc303b0e4ff111e172e")
                .unwrap();

        let entry_points: Vec<EntryPointWithTracingParams> = contracts
            .iter()
            .map(|(addr, func)| {
                EntryPointWithTracingParams::new(
                    EntryPoint::new(
                        format!("{}:{}", addr, func),
                        Bytes::from_str(addr).unwrap(),
                        func.to_string(),
                    ),
                    TracingParams::RPCTracer(RPCTracerParams::new(
                        None,
                        Bytes::from(&keccak256(func)[0..4]),
                    )),
                )
            })
            .collect();

        let results = tracer
            .trace(block_hash.clone(), entry_points.clone())
            .await;

        assert_eq!(
            results.len(),
            contracts.len(),
            "Expected {} results, got {}",
            contracts.len(),
            results.len()
        );

        for (i, result) in results.iter().enumerate() {
            assert!(
                result.is_ok(),
                "Contract {}: Real RPC test failed. Error: {:?}",
                contracts[i].0,
                result
            );

            // Verify the result contains the expected contract address
            if let Ok(traced) = result {
                assert_eq!(
                    traced
                        .entry_point_with_params
                        .entry_point
                        .target,
                    Bytes::from_str(contracts[i].0).unwrap(),
                    "Result {} has wrong target address",
                    i
                );
            }
        }
    }

    #[test]
    fn test_normalize_hex_bytes() {
        // Test with leading zeros - should normalize
        let bytes_with_zeros =
            Bytes::from_str("0x0000000000000000000000000000000000000000000000000000002e8fbca300")
                .unwrap();
        let normalized = EVMEntrypointService::normalize_hex_bytes(&bytes_with_zeros);
        assert_eq!(normalized, "0x2e8fbca300");

        // Test with no leading zeros - should stay the same (but normalized format)
        let bytes_no_zeros = Bytes::from_str("0x2e8fbca300").unwrap();
        let normalized2 = EVMEntrypointService::normalize_hex_bytes(&bytes_no_zeros);
        assert_eq!(normalized2, "0x2e8fbca300");

        // Test with zero value
        let zero_bytes =
            Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000000")
                .unwrap();
        let normalized_zero = EVMEntrypointService::normalize_hex_bytes(&zero_bytes);
        assert_eq!(normalized_zero, "0x0");
    }

    /// Tests that trace calls with balance overrides work correctly with a real RPC node.
    /// This specifically validates that balance values with leading zeros (from U256::to_be_bytes)
    /// are properly normalized before being sent to the RPC.
    ///
    /// Run with: RPC_URL=<url> cargo test --package tycho-ethereum --lib \
    ///   test_trace_call_with_balance_override_integration -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "requires a RPC connection"]
    async fn test_trace_call_with_balance_override_integration() {
        use alloy::{primitives::U256 as AlloyU256, rpc::types::BlockNumberOrTag};

        // Create RPC client to fetch a recent block
        let fixture = TestFixture::new();
        let rpc_client = fixture.create_rpc_client(true);

        // Get a recent finalized block from the chain (works on any EVM chain)
        let recent_block = rpc_client
            .eth_get_block_by_number(BlockId::Number(BlockNumberOrTag::Finalized))
            .await
            .expect("Failed to get finalized block");
        let block_hash = Bytes::from(recent_block.header.hash.to_vec());

        let tracer = EVMEntrypointService::new(&rpc_client);

        // Create state overrides with a balance that has leading zeros when represented as 32 bytes
        // 10 ETH in wei = 10_000_000_000_000_000_000
        // When converted to 32 bytes big-endian, this will have many leading zeros
        let balance_wei = AlloyU256::from(10_000_000_000_000_000_000u128);
        let balance_bytes = Bytes::from(
            balance_wei
                .to_be_bytes::<32>()
                .as_slice(),
        );

        // Verify the balance bytes have leading zeros (this is the bug condition)
        assert!(
            balance_bytes
                .as_ref()
                .starts_with(&[0, 0, 0, 0]),
            "Test setup: balance should have leading zeros"
        );

        let mut state_overrides = BTreeMap::new();
        let zero_address = Bytes::from([0u8; 20].as_slice());

        let account_overrides =
            AccountOverrides { slots: None, native_balance: Some(balance_bytes), code: None };
        state_overrides.insert(zero_address, account_overrides);

        // Use a simple target address - we're just testing that the RPC call doesn't fail
        // due to leading zeros in balance. The actual call can fail/revert; we just need
        // to verify the balance serialization doesn't cause an RPC error.
        let target_address = Bytes::from([0x42u8; 20].as_slice());

        // Empty calldata - this will just be a simple call to the target address
        let calldata = Bytes::from("0x");

        let entry_points = vec![EntryPointWithTracingParams::new(
            EntryPoint::new(
                "test:balanceOverride".to_string(),
                target_address.clone(),
                "test".to_string(),
            ),
            TracingParams::RPCTracer(
                RPCTracerParams::new(None, calldata).with_state_overrides(state_overrides),
            ),
        )];

        let results = tracer
            .trace(block_hash, entry_points)
            .await;

        assert_eq!(results.len(), 1, "Expected 1 result");

        // The key assertion: the call should succeed without the "leading zero digits" error
        // Note: The trace itself may return an error (e.g., contract doesn't exist), but
        // we specifically check that it's NOT a balance normalization error
        match &results[0] {
            Ok(_) => {
                // Success - the trace completed
            }
            Err(e) => {
                let error_msg = format!("{:?}", e);
                assert!(
                    !error_msg.contains("leading zero"),
                    "Trace call failed due to leading zeros in balance. \
                     The balance normalization fix is not working correctly. Error: {}",
                    error_msg
                );
                // If it's another error (like contract not found), that's acceptable for this test
            }
        }
    }

    /// Unit test to verify that create_trace_call_params properly normalizes balance overrides
    #[test]
    fn test_create_trace_call_params_normalizes_balance() {
        use alloy::primitives::U256 as AlloyU256;

        // Create balance with leading zeros (32-byte big-endian representation)
        let balance_wei = AlloyU256::from(10_000_000_000_000_000_000u128);
        let balance_bytes = Bytes::from(
            balance_wei
                .to_be_bytes::<32>()
                .as_slice(),
        );

        let mut state_overrides = BTreeMap::new();
        let zero_address = Bytes::from([0u8; 20].as_slice());

        let account_overrides =
            AccountOverrides { slots: None, native_balance: Some(balance_bytes), code: None };
        state_overrides.insert(zero_address, account_overrides);

        let target = Bytes::from_str("0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D").unwrap();
        let block_hash =
            Bytes::from_str("0xfebbe1110db8fd453b7125860a1c909561d00872aedb40765f54356ac4d7cc40")
                .unwrap();
        let params =
            RPCTracerParams::new(None, Bytes::from("0x00")).with_state_overrides(state_overrides);

        let json_params = EVMEntrypointService::create_trace_call_params(
            &target,
            &params,
            BlockId::Hash(AlloyBlockHash::from_slice(block_hash.as_ref()).into()),
        );

        // Extract the stateOverrides from the JSON
        let tracing_options = json_params
            .as_array()
            .unwrap()
            .get(2)
            .unwrap();
        let state_overrides = tracing_options
            .get("stateOverrides")
            .unwrap();
        let zero_addr_override = state_overrides
            .get("0x0000000000000000000000000000000000000000")
            .unwrap();
        let balance = zero_addr_override
            .get("balance")
            .unwrap()
            .as_str()
            .unwrap();

        // The balance should NOT have leading zeros (except for the 0x prefix)
        // 10 ETH = 0x8ac7230489e80000
        assert_eq!(
            balance, "0x8ac7230489e80000",
            "Balance should be normalized without leading zeros"
        );
        assert!(
            !balance.starts_with("0x00"),
            "Balance should not start with leading zeros: {}",
            balance
        );
    }
}
