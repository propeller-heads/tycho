use std::{collections::HashMap, str::FromStr};

use alloy::{
    primitives::{Address, Bytes as AlloyBytes},
    sol_types::SolValue,
};
use serde::{Deserialize, Serialize};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::ANGSTROM_DEFAULT_BLOCKS_IN_FUTURE,
        utils::{
            bytes_to_address, convert_to_router_token, get_static_attribute,
            pad_or_truncate_to_size,
        },
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on a Uniswap V4 pool through the given executor address.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct UniswapV4SwapEncoder {
    executor_address: Bytes,
    angstrom_hook_address: Bytes,
}

impl UniswapV4SwapEncoder {
    fn get_zero_to_one(sell_token_address: Address, buy_token_address: Address) -> bool {
        sell_token_address < buy_token_address
    }

    /// Fetches attestations from the Angstrom API (blocking)
    fn fetch_angstrom_attestations() -> Result<AttestationResponse, EncodingError> {
        let client = reqwest::blocking::Client::new();

        let api_url = std::env::var("ANGSTROM_API_URL")
            .unwrap_or("https://attestations.angstrom.xyz/getAttestations".to_string());

        let api_key = std::env::var("ANGSTROM_API_KEY").map_err(|_| {
            EncodingError::FatalError(
                "ANGSTROM_API_KEY environment variable is required for Angstrom swaps".to_string(),
            )
        })?;
        let blocks_in_future = std::env::var("ANGSTROM_BLOCKS_IN_FUTURE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(ANGSTROM_DEFAULT_BLOCKS_IN_FUTURE);

        let request_body = serde_json::json!({
            "blocks_in_future": blocks_in_future
        });

        let response = client
            .post(&api_url)
            .header("accept", "application/json")
            .header("X-Api-Key", api_key)
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .map_err(|e| {
                EncodingError::FatalError(format!("Failed to fetch attestations: {}", e))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response
                .text()
                .unwrap_or_else(|_| "Unknown error".to_string());
            return Err(EncodingError::FatalError(format!(
                "Angstrom API request failed with status {}: {}",
                status, error_text
            )));
        }

        let attestation_response: AttestationResponse = response.json().map_err(|e| {
            EncodingError::FatalError(format!("Failed to parse attestation response: {}", e))
        })?;

        if !attestation_response.success {
            return Err(EncodingError::FatalError(
                "Angstrom API returned success=false".to_string(),
            ));
        }

        Ok(attestation_response)
    }

    /// Encodes attestations into bytes
    ///
    /// Uses fixed-length format: each attestation is exactly 93 bytes
    /// (8 bytes block number + 85 bytes attestation)
    fn encode_angstrom_attestations(
        attestations: &AttestationResponse,
    ) -> Result<Vec<u8>, EncodingError> {
        let mut encoded = Vec::new();
        for att_data in &attestations.attestations {
            // Encode block number (first 8 bytes)
            encoded.extend_from_slice(&att_data.block_number.to_be_bytes());

            let attestation_hex = att_data
                .attestation
                .strip_prefix("0x")
                .unwrap_or(&att_data.attestation);

            let attestation_bytes = hex::decode(attestation_hex).map_err(|e| {
                EncodingError::FatalError(format!("Failed to decode attestation hex: {}", e))
            })?;

            // Encode attestation data for block (next 85 bytes)
            encoded.extend_from_slice(&attestation_bytes);
        }

        Ok(encoded)
    }
}

impl SwapEncoder for UniswapV4SwapEncoder {
    fn new(
        executor_address: Bytes,
        _chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let angstrom_hook_address = match config {
            // Allow for no config, since Angstrom is not on every chain
            None => Bytes::new(),
            Some(cfg) => cfg
                .get("angstrom_hook_address")
                .map_or(Ok(Bytes::new()), |s| {
                    Bytes::from_str(s).map_err(|_| {
                        EncodingError::FatalError("Invalid Angstrom hook address".to_string())
                    })
                })?,
        };
        Ok(Self { executor_address, angstrom_hook_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let v4_user_data = UniswapV4UserData::from_swap_user_data(swap.user_data())?;

        let fee = get_static_attribute(swap, "key_lp_fee")?;

        let pool_fee_u24 = pad_or_truncate_to_size::<3>(&fee)
            .map_err(|_| EncodingError::FatalError("Failed to pad fee bytes".to_string()))?;

        let tick_spacing = get_static_attribute(swap, "tick_spacing")?;

        let pool_tick_spacing_u24 = pad_or_truncate_to_size::<3>(&tick_spacing).map_err(|_| {
            EncodingError::FatalError("Failed to pad tick spacing bytes".to_string())
        })?;

        let hook_address = match get_static_attribute(swap, "hooks") {
            Ok(hook) => Address::from_slice(&hook),
            Err(_) => Address::ZERO,
        };

        let is_angstrom_hook = **hook_address == *self.angstrom_hook_address;
        let hook_data = if is_angstrom_hook {
            // Angstrom hook - obtain hook data from API
            let attestations = std::thread::scope(|s| {
                s.spawn(Self::fetch_angstrom_attestations)
                    .join()
                    .expect("attestation fetch panicked")
            })?;
            Self::encode_angstrom_attestations(&attestations)?
        } else {
            v4_user_data.hook_data.to_vec()
        };

        let hook_data_length = (hook_data.len() as u16).to_be_bytes();

        // Early check if this is not the first swap
        if encoding_context.group_token_in != *swap.token_in().address {
            let token_out = convert_to_router_token(bytes_to_address(&swap.token_out().address)?);
            return Ok((
                token_out,
                pool_fee_u24,
                pool_tick_spacing_u24,
                hook_address,
                hook_data_length,
                AlloyBytes::from(hook_data),
            )
                .abi_encode_packed());
        }

        // This is the first swap, compute all necessary values
        let token_in_address = bytes_to_address(&swap.token_in().address)?;
        let token_out_address = bytes_to_address(&swap.token_out().address)?;

        // Compute zero_to_one with protocol-native addresses (before translation)
        // because the V4 executor translates back to address(0) for pool key construction.
        let zero_to_one = Self::get_zero_to_one(token_in_address, token_out_address);

        // Translate for encoding: Tycho uses ETH_ADDRESS
        let group_token_in_encoded =
            convert_to_router_token(bytes_to_address(&encoding_context.group_token_in)?);
        let group_token_out_encoded =
            convert_to_router_token(bytes_to_address(&encoding_context.group_token_out)?);
        let token_out_encoded = convert_to_router_token(token_out_address);

        let pool_params = (
            token_out_encoded,
            pool_fee_u24,
            pool_tick_spacing_u24,
            hook_address,
            hook_data_length,
            AlloyBytes::from(hook_data),
        )
            .abi_encode_packed();

        let args = (
            group_token_in_encoded,
            group_token_out_encoded,
            zero_to_one,
            v4_user_data.skip_unlock,
            pool_params,
        );

        Ok(args.abi_encode_packed())
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

/// Per-swap V4 config, JSON-encoded into `Swap::user_data`. All fields
/// optional; absent/empty `user_data` is equivalent to `default()`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct UniswapV4UserData {
    /// When true, the V4 executor skips `poolManager.unlock()` and
    /// assumes the caller has already unlocked the PM (used by
    /// external-settler flows that take a PM loan around the swap).
    /// Defaults to `false`; producers can omit the field entirely
    /// and the field is skipped from the serialized JSON when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) skip_unlock: bool,
    /// Bytes forwarded as `hookData` to the V4 pool's hook on this
    /// swap. Ignored for Angstrom hooks, where the attestation bytes
    /// are fetched separately.
    #[serde(default)]
    pub(super) hook_data: Bytes,
}

impl UniswapV4UserData {
    fn from_swap_user_data(user_data: &Option<Bytes>) -> Result<Self, EncodingError> {
        match user_data.as_ref() {
            Some(bytes) if !bytes.is_empty() => serde_json::from_slice(bytes).map_err(|e| {
                EncodingError::FatalError(format!("Invalid UniswapV4 user_data JSON: {}", e))
            }),
            _ => Ok(Self::default()),
        }
    }
}

/// Attestation data for Angstrom swaps
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttestationData {
    #[serde(rename = "blockNumber")]
    pub block_number: u64,
    #[serde(rename = "unlockData")]
    pub attestation: String,
}

/// Response from Angstrom attestation API
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttestationResponse {
    pub success: bool,
    pub attestations: Vec<AttestationData>,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::hex::encode;
    use num_bigint::{BigInt, BigUint};
    use tycho_common::{
        models::{protocol::ProtocolComponent, Chain},
        Bytes,
    };

    use super::*;
    use crate::encoding::{
        evm::utils::{ple_encode, write_calldata_to_file},
        models::{default_token, Swap},
    };

    #[test]
    fn test_user_data_defaults_when_absent_or_empty() {
        let from_none = UniswapV4UserData::from_swap_user_data(&None).unwrap();
        assert!(!from_none.skip_unlock);
        assert!(from_none.hook_data.is_empty());

        let from_empty = UniswapV4UserData::from_swap_user_data(&Some(Bytes::from(""))).unwrap();
        assert!(!from_empty.skip_unlock);
        assert!(from_empty.hook_data.is_empty());
    }

    #[test]
    fn test_user_data_parses_both_fields() {
        let raw = br#"{"skip_unlock":true,"hook_data":"0xdeadbeef"}"#;
        let parsed =
            UniswapV4UserData::from_swap_user_data(&Some(Bytes::from(raw.as_slice()))).unwrap();
        assert!(parsed.skip_unlock);
        assert_eq!(parsed.hook_data, Bytes::from("0xdeadbeef"));
    }

    #[test]
    fn test_user_data_partial_fields_use_defaults() {
        let only_skip = UniswapV4UserData::from_swap_user_data(&Some(Bytes::from(
            br#"{"skip_unlock":true}"#.as_slice(),
        )))
        .unwrap();
        assert!(only_skip.skip_unlock);
        assert!(only_skip.hook_data.is_empty());

        let only_hook = UniswapV4UserData::from_swap_user_data(&Some(Bytes::from(
            br#"{"hook_data":"0x1234"}"#.as_slice(),
        )))
        .unwrap();
        assert!(!only_hook.skip_unlock);
        assert_eq!(only_hook.hook_data, Bytes::from("0x1234"));
    }

    #[test]
    fn test_user_data_invalid_json_errors() {
        let err =
            UniswapV4UserData::from_swap_user_data(&Some(Bytes::from(b"not json".as_slice())))
                .unwrap_err();
        assert!(format!("{:?}", err).contains("Invalid UniswapV4 user_data JSON"));
    }

    #[test]
    fn test_encode_uniswap_v4_simple_swap() {
        let fee = BigInt::from(100);
        let tick_spacing = BigInt::from(1);
        let token_in = Bytes::from("0x4c9EDD5852cd905f086C759E8383e09bff1E68B3"); // USDE
        let token_out = Bytes::from("0xdAC17F958D2ee523a2206206994597C13D831ec7"); // USDT

        let mut static_attributes: HashMap<String, Bytes> = HashMap::new();
        static_attributes.insert("key_lp_fee".into(), Bytes::from(fee.to_signed_bytes_be()));
        static_attributes
            .insert("tick_spacing".into(), Bytes::from(tick_spacing.to_signed_bytes_be()));

        let usv4_pool = ProtocolComponent {
            // Pool manager
            id: String::from("0x000000000004444c5dc75cB358380D2e3dE08A90"),
            static_attributes,
            ..Default::default()
        };
        let swap = Swap::new(
            usv4_pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            // Same as the executor address
            router_address: Some(Bytes::from("0x5615deb798bb3e4dfa0139dfa1b3d433cc23b72f")),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = UniswapV4SwapEncoder::new(
            Bytes::from("0xF62849F9A0B5Bf2913b396098F7c7019b51A820a"),
            Chain::Ethereum,
            None,
        )
        .unwrap();
        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(
            hex_swap,
            String::from(concat!(
                // group token in
                "4c9edd5852cd905f086c759e8383e09bff1e68b3",
                // group token out
                "dac17f958d2ee523a2206206994597c13d831ec7",
                // zero for one
                "01",
                // skip unlock
                "00",
                // pool params:
                // - intermediary token
                "dac17f958d2ee523a2206206994597c13d831ec7",
                // - fee
                "000064",
                // - tick spacing
                "000001",
                // hook address (not set, so zero)
                "0000000000000000000000000000000000000000",
                // hook data length (0)
                "0000"
            ))
        );
        write_calldata_to_file("test_encode_uniswap_v4_simple_swap", hex_swap.as_str());
    }

    #[test]
    fn test_encode_uniswap_v4_second_swap() {
        let fee = BigInt::from(3000);
        let tick_spacing = BigInt::from(60);
        let group_token_in = Bytes::from("0x4c9EDD5852cd905f086C759E8383e09bff1E68B3"); // USDE
        let token_in = Bytes::from("0xdAC17F958D2ee523a2206206994597C13D831ec7"); // USDT
        let token_out = Bytes::from("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599"); // WBTC

        let mut static_attributes: HashMap<String, Bytes> = HashMap::new();
        static_attributes.insert("key_lp_fee".into(), Bytes::from(fee.to_signed_bytes_be()));
        static_attributes
            .insert("tick_spacing".into(), Bytes::from(tick_spacing.to_signed_bytes_be()));

        let usv4_pool = ProtocolComponent {
            id: String::from("0x000000000004444c5dc75cB358380D2e3dE08A90"),
            static_attributes,
            ..Default::default()
        };

        let swap = Swap::new(
            usv4_pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );

        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: group_token_in.clone(),
            // Token out is the same as the group token out
            group_token_out: token_out.clone(),
        };

        let encoder = UniswapV4SwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            None,
        )
        .unwrap();
        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(
            hex_swap,
            String::from(concat!(
                // pool params:
                // - intermediary token (20 bytes)
                "2260fac5e5542a773aa44fbcfedf7c193bc2c599",
                // - fee (3 bytes)
                "000bb8",
                // - tick spacing (3 bytes)
                "00003c",
                // hook address (not set, so zero)
                "0000000000000000000000000000000000000000",
                // hook data length (0)
                "0000"
            ))
        );
    }

    #[test]
    fn test_encode_uniswap_v4_sequential_swap() {
        let usde_address = Bytes::from("0x4c9EDD5852cd905f086C759E8383e09bff1E68B3");
        let usdt_address = Bytes::from("0xdAC17F958D2ee523a2206206994597C13D831ec7");
        let wbtc_address = Bytes::from("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599");
        let router_address = Bytes::from("0x5615deb798bb3e4dfa0139dfa1b3d433cc23b72f");

        // The context is the same for both swaps, since the group token in and out are the same
        let context = EncodingContext {
            router_address: Some(router_address.clone()),
            group_token_in: usde_address.clone(),
            group_token_out: wbtc_address.clone(),
        };

        // Setup - First sequence: USDE -> USDT
        let usde_usdt_fee = BigInt::from(100);
        let usde_usdt_tick_spacing = BigInt::from(1);

        let mut usde_usdt_static_attributes: HashMap<String, Bytes> = HashMap::new();
        usde_usdt_static_attributes
            .insert("key_lp_fee".into(), Bytes::from(usde_usdt_fee.to_signed_bytes_be()));
        usde_usdt_static_attributes.insert(
            "tick_spacing".into(),
            Bytes::from(usde_usdt_tick_spacing.to_signed_bytes_be()),
        );

        let usde_usdt_component = ProtocolComponent {
            id: String::from("0x000000000004444c5dc75cB358380D2e3dE08A90"),
            static_attributes: usde_usdt_static_attributes,
            ..Default::default()
        };

        // Setup - Second sequence: USDT -> WBTC
        let usdt_wbtc_fee = BigInt::from(3000);
        let usdt_wbtc_tick_spacing = BigInt::from(60);

        let mut usdt_wbtc_static_attributes: HashMap<String, Bytes> = HashMap::new();
        usdt_wbtc_static_attributes
            .insert("key_lp_fee".into(), Bytes::from(usdt_wbtc_fee.to_signed_bytes_be()));
        usdt_wbtc_static_attributes.insert(
            "tick_spacing".into(),
            Bytes::from(usdt_wbtc_tick_spacing.to_signed_bytes_be()),
        );

        let usdt_wbtc_component = ProtocolComponent {
            id: String::from("0x000000000004444c5dc75cB358380D2e3dE08A90"),
            static_attributes: usdt_wbtc_static_attributes,
            ..Default::default()
        };

        let initial_swap = Swap::new(
            usde_usdt_component,
            default_token(usde_address.clone()),
            default_token(usdt_address.clone()),
            BigUint::ZERO,
        );
        let second_swap = Swap::new(
            usdt_wbtc_component,
            default_token(usdt_address.clone()),
            default_token(wbtc_address.clone()),
            BigUint::ZERO,
        );

        let encoder = UniswapV4SwapEncoder::new(
            Bytes::from("0xF62849F9A0B5Bf2913b396098F7c7019b51A820a"),
            Chain::Ethereum,
            None,
        )
        .unwrap();
        let initial_encoded_swap = encoder
            .encode_swap(&initial_swap, &context)
            .unwrap();
        let second_encoded_swap = encoder
            .encode_swap(&second_swap, &context)
            .unwrap();

        let combined_hex = format!(
            "{}{}",
            encode(&initial_encoded_swap),
            encode(ple_encode(vec![second_encoded_swap]))
        );

        assert_eq!(
            combined_hex,
            String::from(concat!(
                // group_token in
                "4c9edd5852cd905f086c759e8383e09bff1e68b3",
                // group_token out
                "2260fac5e5542a773aa44fbcfedf7c193bc2c599",
                // zero for one
                "01",
                // skip unlock
                "00",
                // pool params:
                // - intermediary token USDT
                "dac17f958d2ee523a2206206994597c13d831ec7",
                // - fee
                "000064",
                // - tick spacing
                "000001",
                // hook address (not set, so zero)
                "0000000000000000000000000000000000000000",
                // hook data length (0)
                "0000",
                // Second swap
                // ple encoding
                "0030",
                // - intermediary token WBTC
                "2260fac5e5542a773aa44fbcfedf7c193bc2c599",
                // - fee
                "000bb8",
                // - tick spacing
                "00003c",
                // hook address (not set, so zero)
                "0000000000000000000000000000000000000000",
                // hook data length (0)
                "0000"
            ))
        );
        write_calldata_to_file("test_encode_uniswap_v4_sequential_swap", combined_hex.as_str());
    }

    mod uniswap_v4_angstrom {
        use super::*;
        use crate::encoding::evm::{
            swap_encoder::uniswap_v4::{
                AttestationData, AttestationResponse, UniswapV4SwapEncoder,
            },
            utils::ple_encode,
        };

        #[test]
        fn test_encode_attestations_format() {
            // Create mock attestation data with real attestations retrieved in the past
            let attestations = AttestationResponse {
            success: true,
            attestations: vec![
                AttestationData {
                    block_number: 12345678,
                    attestation: "0xd437f3372f3add2c2bc3245e6bd6f9c202e61bb367c79a6f740c7c12ca9c54a760bead943516fafaf8a4fe65a907b31d45c2ab4b525f9f32ec2771033e0832359ceb2e38d9288a755c7c366ce889b0df24b5821b1c".to_string(),
                },
                AttestationData {
                    block_number: 12345679,
                    attestation: "0xd437f3372f3add2c2bc3245e6bd6f9c202e61bb30c337ddae661e68cc6986c7784cd0aaec455b1f7514b6cd91bff26f002ce7cb42b3b1e2092ea4d1c1fb1e0641cbccfb021b31de25462f25b355cc99c7d509cdc1b".to_string(),
                },
            ],
        };

            let encoded =
                UniswapV4SwapEncoder::encode_angstrom_attestations(&attestations).unwrap();

            // Verify the structure with fixed-length format:
            // - For each attestation:
            //   - 8 bytes: block number
            //   - 85 bytes: attestation data
            // Total: 93 bytes per attestation, 186 bytes for 2 attestations

            assert_eq!(encoded.len(), 186);
            let encoded_hex = hex::encode(&encoded);

            assert_eq!(
            encoded_hex,
            String::from(concat!(
            // First attestation block number (12345678)
            "0000000000bc614e",
            // First attestation data
            "d437f3372f3add2c2bc3245e6bd6f9c202e61bb367c79a6f740c7c12ca9c54a760bead943516fafaf8a4fe65a907b31d45c2ab4b525f9f32ec2771033e0832359ceb2e38d9288a755c7c366ce889b0df24b5821b1c",
            // Second attestation block number (12345679)
            "0000000000bc614f",
            // Second attestation data
            "d437f3372f3add2c2bc3245e6bd6f9c202e61bb30c337ddae661e68cc6986c7784cd0aaec455b1f7514b6cd91bff26f002ce7cb42b3b1e2092ea4d1c1fb1e0641cbccfb021b31de25462f25b355cc99c7d509cdc1b"
            ))
        );
        }

        #[test]
        #[ignore] // Performs real Angstrom API call
        fn test_encode_grouped_swap_integration() {
            // This test performs a grouped swap: USDC -> WETH -> USDT on two consecutive Angstrom
            // pools
            let usdc_address = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
            let weth_address = Bytes::from("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
            let usdt_address = Bytes::from("0xdAC17F958D2ee523a2206206994597C13D831ec7");
            let angstrom_hook = Bytes::from("0x0000000aa232009084Bd71A5797d089AA4Edfad4");

            // Context for the grouped swap
            let context = EncodingContext {
                router_address: Some(Bytes::from("0x5615deb798bb3e4dfa0139dfa1b3d433cc23b72f")),
                group_token_in: usdc_address.clone(),
                group_token_out: usdt_address.clone(),
            };

            // Setup first pool: USDC -> WETH (use real tick spacing and fee from on-chain)
            let mut usdc_weth_attributes: HashMap<String, Bytes> = HashMap::new();
            usdc_weth_attributes.insert("key_lp_fee".into(), Bytes::from("0x800000")); // 8388608
            usdc_weth_attributes.insert("tick_spacing".into(), Bytes::from("0x0a")); // 10
            usdc_weth_attributes.insert("hooks".into(), angstrom_hook.clone());

            let usdc_weth_pool = ProtocolComponent {
                id: String::from("0x000000000004444c5dc75cB358380D2e3dE08A90"),
                static_attributes: usdc_weth_attributes,
                ..Default::default()
            };

            // Setup second pool: WETH -> USDT (use real tick spacing and fee from on-chain)
            let mut weth_usdt_attributes: HashMap<String, Bytes> = HashMap::new();
            weth_usdt_attributes.insert("key_lp_fee".into(), Bytes::from("0x800000")); // 8388608
            weth_usdt_attributes.insert("tick_spacing".into(), Bytes::from("0x0a")); // 10
            weth_usdt_attributes.insert("hooks".into(), angstrom_hook.clone());

            let weth_usdt_pool = ProtocolComponent {
                id: String::from("0x000000000004444c5dc75cB358380D2e3dE08A90"),
                static_attributes: weth_usdt_attributes,
                ..Default::default()
            };

            let first_swap = Swap::new(
                usdc_weth_pool,
                default_token(usdc_address.clone()),
                default_token(weth_address.clone()),
                BigUint::ZERO,
            );
            let second_swap = Swap::new(
                weth_usdt_pool,
                default_token(weth_address.clone()),
                default_token(usdt_address.clone()),
                BigUint::ZERO,
            );

            // Encoder reads Angstrom config from environment variables:
            // - ANGSTROM_API_KEY (required)
            // - ANGSTROM_API_URL (optional)
            // - ANGSTROM_BLOCKS_IN_FUTURE (optional)
            let encoder = UniswapV4SwapEncoder::new(
                Bytes::from("0xF62849F9A0B5Bf2913b396098F7c7019b51A820a"),
                Chain::Ethereum,
                Some(HashMap::from([(
                    "angstrom_hook_address".to_string(),
                    "0x0000000aa232009084Bd71A5797d089AA4Edfad4".to_string(),
                )])),
            )
            .unwrap();

            // Encode both swaps and combine using prefix-length encoding for the second swap
            let first_encoded = encoder
                .encode_swap(&first_swap, &context)
                .unwrap();
            let second_encoded = encoder
                .encode_swap(&second_swap, &context)
                .unwrap();
            let combined_hex =
                format!("{}{}", encode(&first_encoded), encode(ple_encode(vec![second_encoded])));

            write_calldata_to_file("test_encode_angstrom_grouped_swap", combined_hex.as_str());
            // Any different length could indicate we didn't encode attestation data
            assert!(combined_hex.len() == 2510);
        }
    }
}
