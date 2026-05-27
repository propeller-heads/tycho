use std::collections::HashMap;

use async_trait::async_trait;
use tracing::{debug, info};
use tycho_common::{
    models::{Address, BlockHash},
    traits::BalanceSlotDetector,
    Bytes,
};

use crate::services::entrypoint_tracer::slot_detector::{
    SlotDetectionStrategy, SlotDetector, SlotDetectorError,
};

/// Strategy for balance slot detection
#[derive(Clone)]
pub struct BalanceStrategy;

impl SlotDetectionStrategy for BalanceStrategy {
    type CacheKey = (Address, Address);
    type Params = Address;

    fn cache_key(token: &Address, params: &Self::Params) -> Self::CacheKey {
        (token.clone(), params.clone())
    }

    fn encode_calldata(params: &Self::Params) -> Bytes {
        // balanceOf selector: 0x70a08231
        let mut calldata = vec![0x70, 0xa0, 0x82, 0x31];

        // Pad address to 32 bytes (12 bytes of zeros + 20 bytes address)
        calldata.extend_from_slice(&[0u8; 12]);
        calldata.extend_from_slice(params.as_ref());

        Bytes::from(calldata)
    }
}

/// EVM-specific implementation of BalanceSlotDetector using debug_traceCall
pub type EVMBalanceSlotDetector = SlotDetector<BalanceStrategy>;

/// Implement the BalanceSlotDetector trait
#[async_trait]
impl BalanceSlotDetector for EVMBalanceSlotDetector {
    type Error = SlotDetectorError;

    /// Detect balance storage slots for multiple tokens using a combination of batched and async
    /// concurrent requests.
    async fn detect_balance_slots(
        &self,
        tokens: &[Address],
        holder: Address,
        block_hash: BlockHash,
    ) -> HashMap<Address, Result<(Address, Bytes), Self::Error>> {
        info!("Starting balance slot detection for {} tokens", tokens.len());

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

        let results = self
            .detect_slots_chunked(&filtered_tokens, &holder, &block_hash)
            .await;

        info!("Balance slot detection completed. Found results for {} tokens", results.len());
        results
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use alloy::{primitives::U256, transports::http::reqwest};
    use rstest::rstest;
    use serde_json::json;

    use super::{BalanceStrategy, SlotDetectionStrategy, *};
    use crate::test_fixtures::{
        TestFixture, ARB_USDC_HOLDER_ADDR, ARB_USDC_STR, ARB_WETH_STR, STETH_STR, USDC_HOLDER_ADDR,
        USDC_STR, USDT_STR, WETH_STR,
    };

    const BLOCK_HASH: &str = "0x658814e4cb074359f10dd71237cc57b1ae6791fc9de59fde570e724bd884cbb0";

    impl TestFixture {
        fn create_balance_detector() -> EVMBalanceSlotDetector {
            let fixture = TestFixture::new();

            let rpc = fixture.create_rpc_client(true);

            EVMBalanceSlotDetector::new(&rpc).with_max_token_batch_size(5)
        }

        fn create_arb_balance_detector() -> EVMBalanceSlotDetector {
            let fixture = TestFixture::new_arbitrum();

            let rpc = fixture.create_rpc_client(true);

            EVMBalanceSlotDetector::new(&rpc).with_max_token_batch_size(5)
        }
    }

    #[test]
    fn test_encode_balance_of_calldata() {
        let address = Address::from([0x12u8; 20]);
        let calldata = BalanceStrategy::encode_calldata(&address);

        // Verify selector
        assert_eq!(&calldata[0..4], &[0x70, 0xa0, 0x82, 0x31]);

        // Verify total length (4 bytes selector + 32 bytes padded address)
        assert_eq!(calldata.len(), 36);

        // Verify padding
        assert_eq!(&calldata[4..16], &[0u8; 12]);

        // Verify address
        assert_eq!(&calldata[16..36], address.as_ref());
    }

    #[rstest]
    #[case("0xf847a638E44186F3287ee9F8cAF73FF4d4B80784", "ZeroBalanceUser")]
    #[case(USDC_HOLDER_ADDR, "PoolManager")]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_detect_slots_integration(
        #[case] holder_address_hex: &str,
        #[case] holder_name: &str,
    ) {
        let weth = Address::from_str(WETH_STR).expect("Invalid WETH address");
        let usdc = Address::from_str(USDC_STR).expect("Invalid USDC address");
        let usdt = Address::from_str(USDT_STR).expect("Invalid USDT address");

        let holder_address = Address::from_str(holder_address_hex).expect("Invalid Holder address");

        let tokens = vec![weth.clone(), usdc.clone(), usdt.clone()];

        // Use a recent block
        let block_hash = BlockHash::from_str(BLOCK_HASH).expect("Invalid block hash");
        println!("Block hash: {block_hash}");

        let detector = TestFixture::create_balance_detector();
        let results = detector
            .detect_balance_slots(&tokens, holder_address, block_hash)
            .await;

        // We should get results for the tokens
        assert!(!results.is_empty(), "Expected results for at least one token, but got none");

        // Check individual tokens
        if let Some(weth_result) = results.get(&weth) {
            match weth_result {
                Ok((storage_addr, slot)) => {
                    println!(
                        "WETH slot detected for {holder_name} - Storage: {storage_addr}, Slot: {slot}",
                    );
                }
                Err(e) => panic!("Failed to detect WETH slot for {holder_name}: {e}"),
            }
        } else {
            panic!("No result for WETH token for {holder_name}");
        }

        if let Some(usdc_result) = results.get(&usdc) {
            match usdc_result {
                Ok((storage_addr, slot)) => {
                    println!(
                        "USDC slot detected for {holder_name} - Storage: {storage_addr}, Slot: {slot}",
                    );
                }
                Err(e) => panic!("Failed to detect USDC slot for {holder_name}: {e}"),
            }
        } else {
            panic!("No result for USDC token for {holder_name}");
        }

        if let Some(usdt_result) = results.get(&usdt) {
            match usdt_result {
                Ok((storage_addr, slot)) => {
                    println!(
                        "USDT slot detected for {holder_name} - Storage: {storage_addr}, Slot: {slot}",
                    );
                    assert_eq!(storage_addr, &usdt, "Storage address should match token address");
                }
                Err(e) => panic!("Failed to detect USDT slot for {holder_name}: {e}"),
            }
        } else {
            panic!("No result for USDT token for {holder_name}");
        }
    }

    #[rstest]
    #[case(
        "0xf847a638E44186F3287ee9F8cAF73FF4d4B80784",
        "ZeroBalanceUser",
        "0xf37edb7186962a2f96b7645384a9919d11ea2c760622e9e423e3ff0fa39e9b5b"
    )]
    // Address extracted from stETH events. Verified that it has funds
    #[case(
        "0xef417FCE1883c6653E7dC6AF7c6F85CCDE84Aa09",
        "NonZeroBalanceUser",
        "0x28b290becf7be0019520d491d9cd869337f3d683be3e569e54f9044b94df94c0"
    )]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_detect_slots_rebasing_token(
        #[case] holder_address_hex: &str,
        #[case] holder_name: &str,
        #[case] expected_slot: &str,
    ) {
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");

        // stETH contract address (Lido Staked Ether)
        let steth = Address::from_str(STETH_STR).expect("Invalid stETH address");

        // Address extracted from stETH events. Verified that it has funds
        let balance_owner =
            Address::from_str(holder_address_hex).expect("Invalid balance owner address");

        let tokens = vec![steth.clone()];

        // Use a recent block where stETH has activity
        let block_hash = BlockHash::from_str(BLOCK_HASH).expect("Invalid block hash");

        let detector = TestFixture::create_balance_detector();
        let results = detector
            .detect_balance_slots(&tokens, balance_owner.clone(), block_hash.clone())
            .await;

        // For rebasing tokens like stETH, we expect multiple slots to be accessed
        // because balanceOf() needs to:
        // 1. Read the shares mapping for the holder
        // 2. Read total shares value (stored at TOTAL_SHARES_POSITION)
        // 3. Read total pooled ether to calculate the rate
        if let Some(result) = results.get(&steth) {
            if let Ok((storage_addr, detected_slot)) = result {
                println!(
                    "Detected stETH storage slot - Storage: {storage_addr}, Slot: {detected_slot}",
                );

                // Convert to hex string for verification
                println!("stETH slot hex: {detected_slot}");

                // Now verify the detected slot by setting it to a specific value and checking
                // balanceOf
                let target_balance = U256::from(5000000000000000000u64); // 5 ETH in wei. Without overrides
                let verified_balance = verify_storage_slot_manipulation(
                    &rpc_url,
                    &steth,
                    &balance_owner,
                    detected_slot,
                    target_balance,
                    &block_hash,
                )
                .await
                .expect("Storage slot verification should succeed");

                // Convert U256 to f64 for display
                let target_eth = target_balance.to::<u128>() as f64 / 1e18;
                let verified_eth = verified_balance.to::<u128>() as f64 / 1e18;
                println!("Target balance: {target_eth:.6} ETH");
                println!("Verified balance: {verified_eth:.6} ETH");

                // For stETH, due to the shares system, we expect the actual balance to be
                // equal to or higher than our target (shares are converted to ETH)
                // Expected 6.064202 ETH
                let expected_eth = U256::from(6064202338070893051u128);

                assert_eq!(
                    verified_balance, expected_eth,
                    "Verified balance ({}) should be == expected balance ({})",
                    verified_balance, expected_eth
                );

                println!("✓ Storage slot manipulation verified successfully!");

                // Check if this matches known stETH storage positions:
                assert_eq!(detected_slot.to_string(), expected_slot);
            } else if let Err(e) = result {
                // If no slot detected, print debug info
                println!("Failed to detect slots for stETH: {e} for {holder_name}.");
            }
        } else {
            panic!("No result for stETH token");
        }
    }

    /// Tests balance slot detection for Arbitrum USDC (FiatToken proxy) with a zero-balance
    /// user address. This reproduces the production failure where Arbitrum precompile storage
    /// slots appear in the trace and cause detection to fail.
    ///
    /// The prestateTracer returns slots from Arbitrum precompile addresses (0xa4b05fff...)
    /// alongside the actual USDC contract slots. When the user has zero balance, both the
    /// precompile zero-value slot and the actual balance slot have value 0. If the precompile
    /// slot is tested first, the eth_call override fails with "overriding address ... not
    /// allowed", and the current code treats this as a terminal error instead of trying the
    /// next candidate.
    #[rstest]
    #[case("0xf847a638E44186F3287ee9F8cAF73FF4d4B80784", "ZeroBalanceUser")]
    #[case(ARB_USDC_HOLDER_ADDR, "NonZeroBalanceUser")]
    #[tokio::test]
    #[ignore = "require ARB_RPC_URL"]
    async fn test_detect_arb_usdc_balance_slots(
        #[case] holder_address_hex: &str,
        #[case] holder_name: &str,
    ) {
        let arb_usdc = Address::from_str(ARB_USDC_STR).expect("Invalid ARB USDC address");
        let arb_weth = Address::from_str(ARB_WETH_STR).expect("Invalid ARB WETH address");

        let holder_address = Address::from_str(holder_address_hex).expect("Invalid holder address");

        let tokens = vec![arb_usdc.clone(), arb_weth.clone()];

        // Use latest block — Arbitrum fixture uses block 0 / default hash
        let rpc_url = std::env::var("ARB_RPC_URL").expect("ARB_RPC_URL must be set");
        let client = reqwest::Client::new();
        let response: serde_json::Value = client
            .post(&rpc_url)
            .header("Content-Type", "application/json")
            .json(&json!({
                "jsonrpc": "2.0",
                "method": "eth_getBlockByNumber",
                "params": ["latest", false],
                "id": 1
            }))
            .send()
            .await
            .expect("Failed to fetch block")
            .json()
            .await
            .expect("Failed to parse response");

        let block_hash_str = response["result"]["hash"]
            .as_str()
            .expect("Missing block hash");
        let block_hash = BlockHash::from_str(block_hash_str).expect("Invalid block hash");
        println!("Arbitrum block hash: {block_hash}");

        let detector = TestFixture::create_arb_balance_detector();
        let results = detector
            .detect_balance_slots(&tokens, holder_address, block_hash)
            .await;

        assert!(!results.is_empty(), "Expected results for at least one token, but got none");

        // Check USDC result
        if let Some(usdc_result) = results.get(&arb_usdc) {
            match usdc_result {
                Ok((storage_addr, slot)) => {
                    println!(
                        "ARB USDC slot detected for {holder_name} - Storage: {storage_addr}, Slot: {slot}"
                    );
                    assert_eq!(
                        storage_addr, &arb_usdc,
                        "Storage address should be the USDC proxy, not an Arbitrum precompile"
                    );
                }
                Err(e) => panic!(
                    "Failed to detect ARB USDC balance slot for {holder_name}: {e}. \
                     This is the production bug: slot detection fails for USDC on Arbitrum \
                     because Arbitrum precompile slots interfere with candidate selection."
                ),
            }
        } else {
            panic!("No result for ARB USDC token for {holder_name}");
        }

        // Check WETH result (should always work as it's not a proxy)
        if let Some(weth_result) = results.get(&arb_weth) {
            match weth_result {
                Ok((storage_addr, slot)) => {
                    println!(
                        "ARB WETH slot detected for {holder_name} - Storage: {storage_addr}, Slot: {slot}"
                    );
                }
                Err(e) => panic!("Failed to detect ARB WETH balance slot for {holder_name}: {e}"),
            }
        } else {
            panic!("No result for ARB WETH token for {holder_name}");
        }
    }

    /// Verify that a detected storage slot can be manipulated to achieve a target balance
    async fn verify_storage_slot_manipulation(
        rpc_url: &str,
        token: &Address,
        balance_owner: &Address,
        detected_slot: &Bytes,
        target_balance: U256,
        block_hash: &BlockHash,
    ) -> Result<U256, SlotDetectorError> {
        let target_hex = format!("0x{:064x}", target_balance);

        println!("Setting storage slot {detected_slot} to value {target_hex}");

        let calldata = BalanceStrategy::encode_calldata(balance_owner);

        // This would need to be enhanced to return the actual call result
        // For now, fall back to direct RPC call
        let request = json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [
                {
                    "to": token.to_string(),
                    "data": calldata.to_string()
                },
                block_hash.to_string(),
                {
                    token.to_string(): {
                        "stateDiff": {
                            detected_slot.to_string(): target_hex
                        }
                    }
                }
            ],
            "id": 1
        });

        let client = reqwest::Client::new();
        let response = client
            .post(rpc_url)
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(&request).unwrap())
            .send()
            .await
            .map_err(|e| SlotDetectorError::RequestError(e.to_string()))?;

        let response_json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| SlotDetectorError::InvalidResponse(e.to_string()))?;

        if let Some(error) = response_json.get("error") {
            return Err(SlotDetectorError::RequestError(format!("RPC error: {}", error)));
        }

        let result = response_json
            .get("result")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SlotDetectorError::InvalidResponse("Missing result".into()))?;

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
}
