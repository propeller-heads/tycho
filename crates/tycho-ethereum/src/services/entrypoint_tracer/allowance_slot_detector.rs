use std::collections::HashMap;

use async_trait::async_trait;
use tracing::{debug, info};
use tycho_common::{models::Address, traits::AllowanceSlotDetector, Bytes};

use crate::services::entrypoint_tracer::slot_detector::{
    SlotDetectionStrategy, SlotDetector, SlotDetectorError,
};

/// Strategy for allowance slot detection
#[derive(Clone)]
pub struct AllowanceStrategy;

impl SlotDetectionStrategy for AllowanceStrategy {
    type CacheKey = (Address, Address, Address);
    type Params = (Address, Address);

    fn cache_key(token: &Address, params: &Self::Params) -> Self::CacheKey {
        let (owner, spender) = params;
        (token.clone(), owner.clone(), spender.clone())
    }

    fn encode_calldata(params: &Self::Params) -> Bytes {
        let (owner, spender) = params;
        // allowance selector: 0xdd62ed3e
        let mut calldata = vec![0xdd, 0x62, 0xed, 0x3e];

        // Pad owner address to 32 bytes
        calldata.extend_from_slice(&[0u8; 12]);
        calldata.extend_from_slice(owner.as_ref());

        // Pad spender address to 32 bytes
        calldata.extend_from_slice(&[0u8; 12]);
        calldata.extend_from_slice(spender.as_ref());

        Bytes::from(calldata)
    }
}

/// EVM-specific implementation of AllowanceSlotDetector using debug_traceCall
pub type EVMAllowanceSlotDetector = SlotDetector<AllowanceStrategy>;

/// Implement the AllowanceSlotDetector trait
#[async_trait]
impl AllowanceSlotDetector for EVMAllowanceSlotDetector {
    type Error = SlotDetectorError;

    /// Detect allowance storage slots for multiple tokens using batched and async concurrent
    /// requests.
    async fn detect_allowance_slots(
        &self,
        tokens: &[Address],
        owner: Address,
        spender: Address,
    ) -> HashMap<Address, Result<(Address, Bytes), Self::Error>> {
        info!("Starting allowance slot detection for {} tokens", tokens.len());

        let filtered_tokens = tokens
            .iter()
            .filter_map(|token| {
                if !token.is_zero() {
                    Some(token.clone())
                } else {
                    debug!("Skipping zero token: {token}");
                    None
                }
            })
            .collect::<Vec<_>>();

        if filtered_tokens.is_empty() {
            return HashMap::new();
        }

        let params = (owner, spender);
        let results = self
            .detect_slots_chunked(&filtered_tokens, &params)
            .await;

        info!("Allowance slot detection completed. Found results for {} tokens", results.len());
        results
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;

    use super::*;
    use crate::{
        rpc::EthereumRpcClient,
        test_fixtures::{USDC_STR, USDT_STR, WETH_STR},
    };

    #[test]
    fn test_encode_allowance_calldata() {
        let owner = Address::from([0x11u8; 20]);
        let spender = Address::from([0x22u8; 20]);
        let calldata = AllowanceStrategy::encode_calldata(&(owner.clone(), spender.clone()));

        // Verify selector
        assert_eq!(&calldata[0..4], &[0xdd, 0x62, 0xed, 0x3e]);

        // Verify total length (4 bytes selector + 32 bytes padded owner + 32 bytes padded spender)
        assert_eq!(calldata.len(), 68);

        // Verify owner padding and address
        assert_eq!(&calldata[4..16], &[0u8; 12]);
        assert_eq!(&calldata[16..36], owner.as_ref());

        // Verify spender padding and address
        assert_eq!(&calldata[36..48], &[0u8; 12]);
        assert_eq!(&calldata[48..68], spender.as_ref());
    }

    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_detect_truf_allowance_slot() {
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");

        let rpc = EthereumRpcClient::new(&rpc_url).expect("failed to create RPC client");
        let detector = EVMAllowanceSlotDetector::new(&rpc).with_max_token_batch_size(5);

        // TRUF
        let token = Address::from_str("0x38c2a4a7330b22788374b8ff70bba513c8d848ca").unwrap();

        let owner = Address::from_str("0xcd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2").unwrap();
        let spender = Address::from_str("0xfd0b31d2e955fa55e3fa641fe90e08b677188d35").unwrap();

        let results = detector
            .detect_allowance_slots(std::slice::from_ref(&token), owner, spender)
            .await;

        match results.get(&token) {
            Some(Ok((storage_addr, slot))) => {
                assert_eq!(storage_addr, &token);
                let expected_slot = Bytes::from_str(
                    "0x4e4b5f80f87725e40fd825bd7b26188e05acd6dbf57e82d1bd0f2bd067293504",
                )
                .unwrap();
                assert_eq!(slot, &expected_slot);
            }
            Some(Err(e)) => panic!("Failed to detect slot: {e:?}"),
            None => panic!("No result returned for TRUF"),
        }
    }

    #[rstest]
    // Random EOA - Tycho Router
    #[case(
        "f847a638E44186F3287ee9F8cAF73FF4d4B80784",
        "fD0b31d2E955fA55e3fa641Fe90e08b677188d35",
        "ZeroAllowanceUser"
    )]
    // TychoRouter - Balancer Vault
    #[case(
        "fD0b31d2E955fA55e3fa641Fe90e08b677188d35",
        "BA12222222228d8Ba445958a75a0704d566BF2C8",
        "NonZeroAllowanceUser"
    )]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_detect_allowance_slots_integration(
        #[case] owner_address_hex: &str,
        #[case] spender_address_hex: &str,
        #[case] user_name: &str,
    ) {
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");
        let rpc = EthereumRpcClient::new(&rpc_url).expect("Failed to create RPC client");
        println!("Using RPC URL: {}", rpc_url);

        let weth = Address::from_str(WETH_STR).expect("Invalid WETH address");
        let usdc = Address::from_str(USDC_STR).expect("Invalid USDC address");
        let usdt = Address::from_str(USDT_STR).expect("Invalid USDT address");

        let owner_address = Address::from_str(owner_address_hex).expect("Invalid owner address");
        let spender_address =
            Address::from_str(spender_address_hex).expect("Invalid spender address");

        let tokens = vec![weth.clone(), usdc.clone(), usdt.clone()];

        let detector = EVMAllowanceSlotDetector::new(&rpc).with_max_token_batch_size(5);
        let results = detector
            .detect_allowance_slots(&tokens, owner_address, spender_address)
            .await;

        // We should get results for the tokens
        assert!(!results.is_empty(), "Expected results for at least one token, but got none");

        // Check individual tokens
        if let Some(weth_result) = results.get(&weth) {
            match weth_result {
                Ok((storage_addr, slot)) => {
                    println!(
                        "WETH slot detected for {user_name} - Storage: {storage_addr}, Slot: {slot}",
                    );
                }
                Err(e) => panic!("Failed to detect WETH allowance slot for {}: {}", user_name, e),
            }
        } else {
            panic!("No result for WETH token for {}", user_name);
        }

        if let Some(usdc_result) = results.get(&usdc) {
            match usdc_result {
                Ok((storage_addr, slot)) => {
                    println!(
                        "USDC slot detected for {user_name} - Storage: {storage_addr}, Slot: {slot}",

                    );
                }
                Err(e) => panic!("Failed to detect USDC allowance slot for {}: {}", user_name, e),
            }
        } else {
            panic!("No result for USDC token for {}", user_name);
        }

        if let Some(usdt_result) = results.get(&usdt) {
            match usdt_result {
                Ok((storage_addr, slot)) => {
                    println!(
                          "USDT slot detected for {user_name} - Storage: {storage_addr}, Slot: {slot}",

                    );
                    assert_eq!(storage_addr, &usdt, "Storage address should match token address");
                }
                Err(e) => panic!("Failed to detect USDT allowance slot for {}: {}", user_name, e),
            }
        } else {
            panic!("No result for USDT token for {}", user_name);
        }
    }
}
