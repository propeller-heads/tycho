use std::collections::HashMap;

use serde_json::{json, Value};
use tracing::warn;
use tycho_common::{
    models::{
        blockchain::{
            Block, EntryPoint, EntryPointWithTracingParams, RPCTracerParams, TracingParams,
        },
        protocol::ProtocolComponent,
    },
    Bytes,
};

use crate::extractor::dynamic_contract_indexer::hooks::component_metadata::{
    MetadataError, MetadataRequest, MetadataRequestGenerator, MetadataRequestType,
    MetadataResponseParser, MetadataValue, RpcTransport,
};

pub(super) struct EulerMetadataGenerator {
    rpc_url: String,
}

impl EulerMetadataGenerator {
    pub(super) fn new(rpc_url: String) -> Self {
        Self { rpc_url }
    }
}
const EULER_LENS_BYTECODE_BYTES: &[u8] = include_bytes!("./assets/EulerLimitsLens.evm.runtime");

impl MetadataRequestGenerator for EulerMetadataGenerator {
    fn generate_requests(
        &self,
        component: &ProtocolComponent,
        block: &Block,
    ) -> Result<Vec<MetadataRequest>, MetadataError> {
        let mut requests = vec![];

        let balance_request = self
            .generate_balance_only_requests(component, block)?
            .into_iter()
            .next()
            .expect("Balance request should be generated");

        requests.push(balance_request);

        let mut sorted_tokens = component.tokens.clone();
        sorted_tokens.sort_unstable();

        let token_0 = sorted_tokens[0].clone();
        let token_1 = sorted_tokens[1].clone();

        let token_0_str = token_0.to_string().split_off(2);
        let token_1_str = token_1.to_string().split_off(2);

        // Metadata is extracted by calling the hooks contract
        let target = component
            .static_attributes
            .get("hooks")
            .expect("Hooks attribute not found");

        // Use a deterministic lens contract address for state override
        // The lens contract bytecode will be deployed via state override during the eth_call
        // We set the hooks address to the slot 0 to allow identifying the desired hook address,
        // while maintaining the same interface as the original getLimits.
        let lens_address = "0x0000000000000000000000000000000000001337";
        let target_str = target.to_string().split_off(2);

        // Load the lens contract bytecode from the assets and convert to hex
        let lens_bytecode_hex = hex::encode(EULER_LENS_BYTECODE_BYTES);

        let limits_transport_0to1 = RpcTransport::new(
            self.rpc_url.clone(),
            "eth_call".to_string(),
            vec![
                json!({
                    "data": format!("0xaaed87a3000000000000000000000000{}000000000000000000000000{}", token_0_str, token_1_str),
                    "to": lens_address
                }),
                json!(format!("0x{:x}", block.number)),
                json!({
                    lens_address: {
                        "code": format!("0x{}", lens_bytecode_hex),
                        "state": {
                            "0x0000000000000000000000000000000000000000000000000000000000000000": format!("0x000000000000000000000000{}", target_str)
                        }
                    }
                }),
            ],
        );
        requests.push(MetadataRequest::new(
            "euler".to_string(),
            format!("euler_limits_{target}_{token_0}_to_{token_1}"),
            component.id.clone(),
            // Euler swap only has pools with 2 tokens
            MetadataRequestType::Limits { token_pair: vec![(token_0.clone(), token_1.clone())] },
            Box::new(limits_transport_0to1),
        ));

        let limits_transport_1to0 = RpcTransport::new(
            self.rpc_url.clone(),
            "eth_call".to_string(),
            vec![
                json!({
                  "data": format!("0xaaed87a3000000000000000000000000{}000000000000000000000000{}", token_1_str, token_0_str),
                  "to": lens_address
                }),
                json!(format!("0x{:x}", block.number)),
                json!({
                    lens_address: {
                        "code": format!("0x{}", lens_bytecode_hex),
                        "state": {
                            "0x0000000000000000000000000000000000000000000000000000000000000000": format!("0x000000000000000000000000{}", target_str)
                        }
                    }
                }),
            ],
        );
        requests.push(MetadataRequest::new(
            "euler".to_string(),
            format!("euler_limits_{target}_{token_1}_to_{token_0}"),
            component.id.clone(),
            // Euler swap only has pools with 2 tokens
            MetadataRequestType::Limits { token_pair: vec![(token_1, token_0)] },
            Box::new(limits_transport_1to0),
        ));

        Ok(requests)
    }

    fn generate_balance_only_requests(
        &self,
        component: &ProtocolComponent,
        block: &Block,
    ) -> Result<Vec<MetadataRequest>, MetadataError> {
        let mut requests = vec![];
        // Balance is extracted by calling the hooks contract
        let target = component
            .static_attributes
            .get("hooks")
            .expect("Hooks attribute not found");

        let balance_transport = RpcTransport::new(
            self.rpc_url.clone(),
            "eth_call".to_string(),
            vec![
                json!({
                    "data": "0x0902f1ac", // getReserves()
                    "to": target
                }),
                json!(format!("0x{:x}", block.number)),
            ],
        );
        requests.push(MetadataRequest::new(
            "euler".to_string(),
            format!("euler_balance_{target}"),
            component.id.clone(),
            MetadataRequestType::ComponentBalance,
            Box::new(balance_transport),
        ));
        Ok(requests)
    }
}

pub(super) struct EulerMetadataResponseParser;

impl MetadataResponseParser for EulerMetadataResponseParser {
    fn parse_response(
        &self,
        component: &ProtocolComponent,
        request: &MetadataRequest,
        response: &Value,
    ) -> Result<MetadataValue, MetadataError> {
        match &request.request_type() {
            MetadataRequestType::ComponentBalance => {
                if component.tokens.len() < 2 {
                    return Err(MetadataError::MissingData(
                        "component must have at least 2 tokens".to_string(),
                        component.id.clone(),
                    ));
                }

                let mut sorted_tokens = component.tokens.clone();
                sorted_tokens.sort_unstable();
                let token_0 = &sorted_tokens[0];
                let token_1 = &sorted_tokens[1];

                let res_string = serde_json::from_value::<String>(response.clone()).unwrap();
                let res_str = res_string
                    .strip_prefix("0x")
                    .unwrap_or(&res_string);

                // Check if response has enough data
                if res_str.len() < 128 {
                    return Err(MetadataError::GenerationFailed(format!(
                        "Balance response too short: expected at least 128 characters, got {}",
                        res_str.len()
                    )));
                }

                let balance_0 = Bytes::from(&res_str[0..64]);
                let balance_1 = Bytes::from(&res_str[64..128]);

                let mut balances = HashMap::new();
                balances.insert(token_0.clone(), balance_0);
                balances.insert(token_1.clone(), balance_1);

                let res = MetadataValue::Balances(balances);
                Ok(res)
            }
            MetadataRequestType::Limits { token_pair } => {
                let res_string = serde_json::from_value::<String>(response.clone()).unwrap();
                let res_str = res_string
                    .strip_prefix("0x")
                    .unwrap_or(&res_string);

                let entrypoint = (|| {
                    let request = request
                        .transport()
                        .as_any()
                        .downcast_ref::<RpcTransport>()
                        .ok_or_else(|| {
                            MetadataError::UnknownError("Not RpcTransport".to_string())
                        })?;

                    let params = &request.params()[0];

                    let target = component
                        .static_attributes
                        .get("hooks")
                        .expect("Hooks attribute not found");

                    let calldata = params["data"].as_str().ok_or_else(|| {
                        MetadataError::UnknownError("calldata not found".to_string())
                    })?;

                    Ok(EntryPointWithTracingParams {
                        entry_point: EntryPoint {
                            external_id: format!("{target}:getLimits(address,address)"),
                            target: target.clone(),
                            signature: "getLimits(address,address)".into(),
                        },
                        params: TracingParams::RPCTracer(RPCTracerParams {
                            calldata: Bytes::from(calldata),
                            caller: None,
                            state_overrides: None,
                            prune_addresses: None,
                        }),
                    })
                })()
                .map_err(|e: MetadataError| {
                    warn!("Entrypoint error for {}: {}", component.id, e);
                    e
                })
                .ok();

                // Check if response has enough data
                if res_str.len() < 128 {
                    return Err(MetadataError::GenerationFailed(format!(
                        "Limits response too short: expected at least 128 characters, got {}",
                        res_str.len()
                    )));
                }

                let limit_0 = Bytes::from(&res_str[0..64]);
                let limit_1 = Bytes::from(&res_str[64..128]);

                Ok(MetadataValue::Limits(vec![(
                    token_pair[0].clone(),
                    (limit_0, limit_1, entrypoint),
                )]))
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::NaiveDateTime;
    use mockito::Server;
    use tycho_common::{
        models::{Chain, ChangeType},
        Bytes,
    };

    use super::*;
    use crate::extractor::dynamic_contract_indexer::hooks::{
        component_metadata::{RequestProvider, RequestTransport},
        rpc_metadata_provider::RPCMetadataProvider,
    };

    fn create_test_component() -> ProtocolComponent {
        let mut static_attributes = HashMap::new();
        static_attributes
            .insert("hooks".to_string(), Bytes::from("0x000000000000000000000000000000000000beef"));

        ProtocolComponent {
            id: "0x000000000000000000000000000000000000beef".to_string(),
            tokens: vec![
                Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), // USDC
                Bytes::from("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), // WETH
            ],
            static_attributes,
            ..Default::default()
        }
    }

    fn create_test_block() -> Block {
        Block::new(
            12345,
            Chain::Ethereum,
            Bytes::from("0x1234567890123456789012345678901234567890123456789012345678901234"),
            Bytes::from("0x1234567890123456789012345678901234567890123456789012345678901233"),
            NaiveDateTime::parse_from_str("2023-01-01T00:00:00", "%Y-%m-%dT%H:%M:%S").unwrap(),
        )
    }

    #[test]
    fn test_generate_requests() {
        let generator =
            EulerMetadataGenerator::new("https://eth-mainnet.alchemyapi.io/v2/test".to_string());
        let component = create_test_component();
        let block = create_test_block();

        let requests = generator
            .generate_requests(&component, &block)
            .unwrap();

        assert_eq!(requests.len(), 3);

        // Balance request
        assert_eq!(requests[0].component_id(), &component.id);
        assert_eq!(requests[0].request_type(), &MetadataRequestType::ComponentBalance);
        assert_eq!(
            requests[0].request_id(),
            "euler_balance_0x000000000000000000000000000000000000beef"
        );
        assert_eq!(requests[0].transport().routing_key(), "rpc_default".to_string());
        assert_eq!(
            requests[0].transport().deduplication_id(),
            "eth_call_[{\"data\":\"0x0902f1ac\",\"to\":\"0x000000000000000000000000000000000000beef\"},\"0x3039\"]".to_string()
        );

        // Limits request 0 to 1
        assert_eq!(requests[1].component_id(), &component.id);
        assert_eq!(
            requests[1].request_type(),
            &MetadataRequestType::Limits {
                token_pair: vec![(component.tokens[0].clone(), component.tokens[1].clone())]
            }
        );
        assert_eq!(
            requests[1].request_id(),
            "euler_limits_0x000000000000000000000000000000000000beef_0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48_to_0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
        );
        assert_eq!(requests[1].transport().routing_key(), "rpc_default".to_string());

        // The deduplication_id will now include the state & code override parameters
        assert!(requests[1]
            .transport()
            .deduplication_id()
            .starts_with("eth_call_"));

        dbg!(&requests[1]
            .transport()
            .deduplication_id());

        assert!(requests[1]
            .transport()
            .deduplication_id()
            .contains("code"));

        assert!(requests[1]
            .transport()
            .deduplication_id()
            .contains("state"));

        // Limits request 1 to 0
        assert_eq!(requests[2].component_id(), &component.id);
        assert_eq!(
            requests[2].request_type(),
            &MetadataRequestType::Limits {
                token_pair: vec![(component.tokens[1].clone(), component.tokens[0].clone())]
            }
        );
        assert_eq!(
            requests[2].request_id(),
            "euler_limits_0x000000000000000000000000000000000000beef_0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2_to_0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
        assert_eq!(requests[2].transport().routing_key(), "rpc_default".to_string());

        // The deduplication_id will now include the state & code override parameters
        assert!(requests[2]
            .transport()
            .deduplication_id()
            .starts_with("eth_call_"));

        assert!(requests[2]
            .transport()
            .deduplication_id()
            .contains("code"));

        assert!(requests[2]
            .transport()
            .deduplication_id()
            .contains("state"));
    }

    #[test]
    fn test_generate_balance_only_requests() {
        let generator =
            EulerMetadataGenerator::new("https://eth-mainnet.alchemyapi.io/v2/test".to_string());
        let component = create_test_component();
        let block = create_test_block();

        let requests = generator
            .generate_balance_only_requests(&component, &block)
            .unwrap();

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].component_id(), &component.id);
        assert_eq!(requests[0].request_type(), &MetadataRequestType::ComponentBalance);
        assert_eq!(
            requests[0].request_id(),
            &"euler_balance_0x000000000000000000000000000000000000beef".to_string()
        );
        assert_eq!(requests[0].transport().routing_key(), "rpc_default");
        assert_eq!(
            requests[0].transport().deduplication_id(),
            "eth_call_[{\"data\":\"0x0902f1ac\",\"to\":\"0x000000000000000000000000000000000000beef\"},\"0x3039\"]".to_string()
        );
    }

    #[tokio::test]
    #[ignore = "This test requires a real RPC connection"]
    async fn test_euler_metadata_roundtrip() {
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");
        let generator = EulerMetadataGenerator::new(rpc_url);
        let rpc_provider = RPCMetadataProvider::new(10);
        let parser = EulerMetadataResponseParser;

        let component = ProtocolComponent {
            id: "0xc88b618c2c670c2e2a42e06b466b6f0e82a6e8a8".to_string(),
            protocol_system: "euler_swap".to_string(),
            protocol_type_name: "swap".to_string(),
            chain: Chain::Ethereum,
            tokens: vec![
                Bytes::from("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
                Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
            ],
            contract_addresses: vec![],
            static_attributes: HashMap::from([
                ("hooks".to_string(), Bytes::from("0xc88b618c2c670c2e2a42e06b466b6f0e82a6e8a8")),
                ("token_0".to_string(), Bytes::from("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")),
                ("token_1".to_string(), Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")),
            ]),
            change: ChangeType::Creation,
            creation_tx: Bytes::from(
                "0x316209797c061712713d61cfa30c200dbe0211bec67d8ef3ddfef38c733bb1c0",
            ),
            created_at: NaiveDateTime::parse_from_str("2025-07-07T08:42:47", "%Y-%m-%dT%H:%M:%S")
                .unwrap(),
        };
        let block = Block {
            number: 22923196,
            chain: Chain::Ethereum,
            hash: Bytes::from("0x5dae08576aa4a6d8a84a677b93f6892c82e53bb05f6df6a2f968fc012b37136e"),
            parent_hash: Bytes::from(
                "0x274607019d5d34329809c455db8230c3ef2cf038c15ddacc79e36beb645da02d",
            ),
            ts: NaiveDateTime::parse_from_str("2025-07-15T07:49:59", "%Y-%m-%dT%H:%M:%S").unwrap(),
        };

        let requests = generator
            .generate_requests(&component, &block)
            .unwrap();

        let id_to_request = requests
            .iter()
            .map(|request| (request.transport().deduplication_id(), request.clone()))
            .collect::<HashMap<String, MetadataRequest>>();

        let rpc_requests: Vec<Box<dyn RequestTransport>> = requests
            .iter()
            .map(|request| request.transport().clone_box())
            .collect();

        let results = rpc_provider
            .execute_batch(&rpc_requests)
            .await;

        assert_eq!(results.len(), requests.len());

        let mut parsed_results = vec![];
        for (request_id, result) in results {
            let request = id_to_request
                .get(&request_id)
                .expect("Request ID should be present in the request map");

            let parsed_result = parser
                .parse_response(&component, request, &result.expect("Request should be successful"))
                .unwrap();

            parsed_results.push(parsed_result);
        }

        assert_eq!(parsed_results.len(), requests.len());

        let expected_balances = {
            let mut map = HashMap::new();
            map.insert(
                Bytes::from("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
                Bytes::from("0x0000000000000000000000000000000000000000000000000000000030598d13"),
            );
            map.insert(
                Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
                Bytes::from("0x00000000000000000000000000000000000000000000000000013ccb6410e36b"),
            );
            map
        };

        let expected_value = MetadataValue::Balances(expected_balances);

        assert!(
            parsed_results.contains(&expected_value),
            "Expected balances not found in parsed_results"
        );

        assert!(parsed_results.contains(&MetadataValue::Limits(vec![(
            (
                Bytes::from("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
                Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
            ),
            (
                Bytes::from("0x0000000000000000000000000000000000000000000000000000267d3cdc9cbf"),
                Bytes::from("0x00000000000000000000000000000000000000000000000000013ccb6410e36b"),
                Some(EntryPointWithTracingParams {
                    entry_point: EntryPoint {
                        external_id: "0x0000000000000000000000000000000000000000000000000000000000000000:getLimits(address,address)".to_string(),
                        target: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000000"),
                        signature: "getLimits(address,address)".to_string(),
                    },
                    params: tycho_common::models::blockchain::TracingParams::RPCTracer(
                        RPCTracerParams {
                            caller: None,
                            calldata: Bytes::from("0xaaed87a3000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
                            state_overrides: None,
                            prune_addresses: None,
                        },
                    ),
                }),
            )
        )]),));

        assert!(parsed_results.contains(&MetadataValue::Limits(vec![(
            (
                Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
                Bytes::from("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
            ),
            (
                Bytes::from("0x000000000000000000000000000000000000000000000ecaa543f127a7d92880"),
                Bytes::from("0x0000000000000000000000000000000000000000000000000000000030598d13"),
                Some(EntryPointWithTracingParams {
                    entry_point: EntryPoint {
                        external_id: "0x0000000000000000000000000000000000000000000000000000000000000000:getLimits(address,address)".to_string(),
                        target: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000000"),
                        signature: "getLimits(address,address)".to_string(),
                    },
                    params: tycho_common::models::blockchain::TracingParams::RPCTracer(
                        RPCTracerParams {
                            caller: None,
                            calldata: Bytes::from("0xaaed87a3000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
                            state_overrides: None,
                            prune_addresses: None,
                        },
                    ),
                }),
            )
        )]),));
    }

    #[tokio::test]
    async fn test_euler_metadata_roundtrip_mock() {
        let mut server = Server::new_async().await;
        let endpoint = server.url();

        let generator = EulerMetadataGenerator::new(endpoint);
        let rpc_provider = RPCMetadataProvider::new(10);
        let parser = EulerMetadataResponseParser;

        let component = ProtocolComponent {
            id: "0xc88b618c2c670c2e2a42e06b466b6f0e82a6e8a8".to_string(),
            protocol_system: "euler_swap".to_string(),
            protocol_type_name: "swap".to_string(),
            chain: Chain::Ethereum,
            tokens: vec![
                Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
                Bytes::from("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
            ],
            contract_addresses: vec![],
            static_attributes: HashMap::from([(
                "hooks".to_string(),
                Bytes::from("0xc88b618c2c670c2e2a42e06b466b6f0e82a6e8a8"),
            )]),
            change: ChangeType::Creation,
            creation_tx: Bytes::from(
                "0x316209797c061712713d61cfa30c200dbe0211bec67d8ef3ddfef38c733bb1c0",
            ),
            created_at: NaiveDateTime::parse_from_str("2025-07-07T08:42:47", "%Y-%m-%dT%H:%M:%S")
                .unwrap(),
        };
        let block = Block {
            number: 22923196,
            chain: Chain::Ethereum,
            hash: Bytes::from("0x5dae08576aa4a6d8a84a677b93f6892c82e53bb05f6df6a2f968fc012b37136e"),
            parent_hash: Bytes::from(
                "0x274607019d5d34329809c455db8230c3ef2cf038c15ddacc79e36beb645da02d",
            ),
            ts: NaiveDateTime::parse_from_str("2025-07-15T07:49:59", "%Y-%m-%dT%H:%M:%S").unwrap(),
        };

        let requests = generator
            .generate_requests(&component, &block)
            .unwrap();

        // Create mock responses for each request
        let mut mock_responses = vec![];
        for request in &requests {
            let transport = request
                .transport()
                .as_any()
                .downcast_ref::<RpcTransport>()
                .unwrap();
            let id = transport.id();

            let response = match request.request_id().as_str() {
                "euler_balance_0xc88b618c2c670c2e2a42e06b466b6f0e82a6e8a8" => {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": "0x0000000000000000000000000000000000000000000000000000000030598d1300000000000000000000000000000000000000000000000000013ccb6410e36b0000000000000000000000000000000000000000000000000000000000000001"
                    })
                }
                "euler_limits_0xc88b618c2c670c2e2a42e06b466b6f0e82a6e8a8_0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48_to_0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2" => {
                    // Mock limits response - empty for now
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": "0x0000000000000000000000000000000000000000000000000000267d3cdc9cbf00000000000000000000000000000000000000000000000000013ccb6410e36b"
                    })
                }
                "euler_limits_0xc88b618c2c670c2e2a42e06b466b6f0e82a6e8a8_0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2_to_0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48" => {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": "0x000000000000000000000000000000000000000000000ecaa543f127a7d928800000000000000000000000000000000000000000000000000000000030598d13"
                    })
                }
                _ => panic!("Unexpected request ID: {}", request.request_id()),
            };

            mock_responses.push(response);
        }
        let batch_responses: Vec<serde_json::Value> = mock_responses;

        let mock = server
            .mock("POST", "/")
            .with_body(serde_json::to_string(&batch_responses).unwrap())
            .expect(1)
            .create_async()
            .await;

        let id_to_request = requests
            .iter()
            .map(|request| (request.transport().deduplication_id(), request.clone()))
            .collect::<HashMap<String, MetadataRequest>>();

        let rpc_requests: Vec<Box<dyn RequestTransport>> = requests
            .iter()
            .map(|request| request.transport().clone_box())
            .collect();

        let results = rpc_provider
            .execute_batch(&rpc_requests)
            .await;

        // Assertions
        mock.assert();

        let mut parsed_results = vec![];
        for (request_id, result) in results {
            let request = id_to_request
                .get(&request_id)
                .expect("Request ID should be present in the request map");

            let parsed_result = parser
                .parse_response(&component, request, &result.expect("Request should be successful"))
                .unwrap();

            parsed_results.push(parsed_result);
        }

        assert_eq!(parsed_results.len(), requests.len());

        let expected_balances = {
            let mut map = HashMap::new();
            map.insert(
                Bytes::from("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
                Bytes::from("0x0000000000000000000000000000000000000000000000000000000030598d13"),
            );
            map.insert(
                Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
                Bytes::from("0x00000000000000000000000000000000000000000000000000013ccb6410e36b"),
            );
            map
        };

        let expected_value = MetadataValue::Balances(expected_balances);

        assert!(
            parsed_results.contains(&expected_value),
            "Expected balances not found in parsed_results"
        );

        // Verify we got limits responses (exact content may vary due to entrypoint details)
        let limits_found = parsed_results
            .iter()
            .any(|result| matches!(result, MetadataValue::Limits(_)));
        assert!(limits_found, "Limits responses should be found");
    }

    #[tokio::test]
    #[ignore = "This test requires a real RPC connection"]
    async fn test_euler_metadata_with_lens_contract() {
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");
        let generator = EulerMetadataGenerator::new(rpc_url);
        let rpc_provider = RPCMetadataProvider::new(10);
        let parser = EulerMetadataResponseParser;

        let component = ProtocolComponent {
            id: "0x55dcf9455eee8fd3f5eed17606291272cde428a8".to_string(),
            protocol_system: "euler_swap".to_string(),
            protocol_type_name: "swap".to_string(),
            chain: Chain::Ethereum,
            tokens: vec![
                Bytes::from("0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0"), // asset0
                Bytes::from("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), // asset1 (WETH)
            ],
            contract_addresses: vec![],
            static_attributes: HashMap::from([(
                "hooks".to_string(),
                Bytes::from("0x55dcf9455eee8fd3f5eed17606291272cde428a8"),
            )]),
            change: ChangeType::Creation,
            creation_tx: Bytes::from(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ),
            created_at: NaiveDateTime::parse_from_str("2025-01-01T00:00:00", "%Y-%m-%dT%H:%M:%S")
                .unwrap(),
        };
        let block = Block {
            number: 23003136,
            chain: Chain::Ethereum,
            hash: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000000"),
            parent_hash: Bytes::from(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ),
            ts: NaiveDateTime::parse_from_str("2025-01-01T00:00:00", "%Y-%m-%dT%H:%M:%S").unwrap(),
        };

        let requests = generator
            .generate_requests(&component, &block)
            .unwrap();

        let id_to_request = requests
            .iter()
            .map(|request| (request.transport().deduplication_id(), request.clone()))
            .collect::<HashMap<String, MetadataRequest>>();

        let rpc_requests: Vec<Box<dyn RequestTransport>> = requests
            .iter()
            .map(|request| request.transport().clone_box())
            .collect();

        let results = rpc_provider
            .execute_batch(&rpc_requests)
            .await;

        assert_eq!(results.len(), requests.len());

        // With state override, both balance and limits calls should succeed
        let mut balance_request_success = false;
        let mut limits_request_success = false;

        for (request_id, result) in results {
            let request = id_to_request
                .get(&request_id)
                .expect("Request ID should be present in the request map");

            match &request.request_type() {
                MetadataRequestType::ComponentBalance => {
                    if let Ok(result_value) = result {
                        let parsed_result =
                            parser.parse_response(&component, request, &result_value);
                        if parsed_result.is_ok() {
                            balance_request_success = true;
                        }
                    } else {
                        println!("Balance request failed: {:?}", result);
                    }
                }
                MetadataRequestType::Limits { .. } => {
                    if let Ok(result_value) = result {
                        let parsed_result =
                            parser.parse_response(&component, request, &result_value);
                        if let Ok(_parsed) = parsed_result {
                            limits_request_success = true;
                        } else {
                            println!("Limits parsing failed: {:?}", parsed_result);
                        }
                    } else {
                        println!("Limits request failed: {:?}", result);
                    }
                }
            }
        }

        // Verify that both balance and limits requests succeeded with state override
        assert!(balance_request_success, "Balance request should succeed");
        assert!(limits_request_success, "Limits requests should succeed with state override");
    }

    #[test]
    fn test_entry_point_target_uses_hooks_address() {
        let generator =
            EulerMetadataGenerator::new("https://eth-mainnet.alchemyapi.io/v2/test".to_string());
        let parser = EulerMetadataResponseParser;

        // Create a test component with different hooks and ID addresses to ensure we use the right
        // one
        let hooks_address = Bytes::from("0x55dcf9455eee8fd3f5eed17606291272cde428a8");
        let different_id = "0x1111111111111111111111111111111111111111".to_string();

        let component = ProtocolComponent {
            id: different_id, // Different from hooks address
            protocol_system: "euler_swap".to_string(),
            protocol_type_name: "swap".to_string(),
            chain: Chain::Ethereum,
            tokens: vec![
                Bytes::from("0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0"),
                Bytes::from("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            ],
            contract_addresses: vec![],
            static_attributes: HashMap::from([("hooks".to_string(), hooks_address.clone())]),
            change: ChangeType::Creation,
            creation_tx: Bytes::from(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ),
            created_at: NaiveDateTime::parse_from_str("2025-01-01T00:00:00", "%Y-%m-%dT%H:%M:%S")
                .unwrap(),
        };

        let block = Block {
            number: 23003136,
            chain: Chain::Ethereum,
            hash: Bytes::from("0x0000000000000000000000000000000000000000000000000000000000000000"),
            parent_hash: Bytes::from(
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ),
            ts: NaiveDateTime::parse_from_str("2025-01-01T00:00:00", "%Y-%m-%dT%H:%M:%S").unwrap(),
        };

        // Mock response data for limits call (doesn't matter for this test, just needs to be valid
        // hex)
        let mock_response = serde_json::Value::String("0x0000000000000000000000000000000000000000000000000000000030598d130000000000000000000000000000000000000000000000000000000030598d13".to_string());

        // Generate requests and find the limits request
        let requests = generator
            .generate_requests(&component, &block)
            .unwrap();
        let limits_request = requests
            .iter()
            .find(|r| matches!(r.request_type(), MetadataRequestType::Limits { .. }))
            .expect("Should have limits request");

        // Parse the response to get the entry point
        let parsed_result = parser
            .parse_response(&component, limits_request, &mock_response)
            .unwrap();

        if let MetadataValue::Limits(limits_data) = parsed_result {
            let (_, (_, _, entry_point_opt)) = &limits_data[0];
            if let Some(entry_point_with_params) = entry_point_opt {
                // Verify that the target address in the entry point is the hooks address, NOT the
                // lens address
                let entry_point_target = entry_point_with_params
                    .entry_point
                    .target
                    .clone();
                assert_eq!(
                    entry_point_target,
                    hooks_address,
                    "Entry point target should be the hooks address (0x55dcf9455eee8fd3f5eed17606291272cde428a8), not the lens address (0x0000000000000000000000000000000000001337)"
                );

                // Also verify the external_id includes the hooks address
                let expected_external_id = format!("{}:getLimits(address,address)", hooks_address);
                assert_eq!(
                    entry_point_with_params
                        .entry_point
                        .external_id,
                    expected_external_id,
                    "Entry point external_id should reference the hooks address"
                );

                // Verify the signature is correct
                assert_eq!(
                    entry_point_with_params
                        .entry_point
                        .signature,
                    "getLimits(address,address)",
                    "Entry point signature should be getLimits(address,address)"
                );
            } else {
                panic!("Expected entry point to be present in limits data");
            }
        } else {
            panic!("Expected MetadataValue::Limits, got: {:?}", parsed_result);
        }
    }
}
