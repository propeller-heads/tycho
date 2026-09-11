use std::{collections::HashMap, str::FromStr, sync::Arc};

use alloy::{
    primitives::{Address, Bytes as AlloyBytes},
    sol_types::SolValue,
};
use serde::{Deserialize, Serialize};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        swap_encoder::angstrom::AttestationCache,
        utils::{
            bytes_to_address, convert_to_router_token, get_static_attribute,
            pad_or_truncate_to_size,
        },
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// The Angstrom hook deployed on this chain, together with its attestation cache.
#[derive(Clone)]
struct AngstromConfig {
    hook_address: Bytes,
    attestations: Arc<AttestationCache>,
}

/// Encodes a swap on a Uniswap V4 pool through the given executor address.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
/// * `angstrom` - Set on chains where Angstrom is deployed. Swaps on a pool using the Angstrom hook
///   carry a pool unlock attestation as their hook data.
#[derive(Clone)]
pub struct UniswapV4SwapEncoder {
    executor_address: Bytes,
    angstrom: Option<AngstromConfig>,
}

impl UniswapV4SwapEncoder {
    fn get_zero_to_one(sell_token_address: Address, buy_token_address: Address) -> bool {
        sell_token_address < buy_token_address
    }
}

impl SwapEncoder for UniswapV4SwapEncoder {
    fn new(
        executor_address: Bytes,
        _chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        // Allow for no config, since Angstrom is not on every chain
        let hook_address = config
            .as_ref()
            .and_then(|cfg| cfg.get("angstrom_hook_address"));

        let angstrom = match hook_address {
            None => None,
            Some(address) => {
                let hook_address = Bytes::from_str(address).map_err(|_| {
                    EncodingError::FatalError("Invalid Angstrom hook address".to_string())
                })?;
                // Starting the cache here warms it up while the encoder is built, so that the
                // first encoded Angstrom swap does not wait on the Angstrom API.
                let attestations = Arc::clone(AttestationCache::global());
                Some(AngstromConfig { hook_address, attestations })
            }
        };

        Ok(Self { executor_address, angstrom })
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

        let angstrom = self
            .angstrom
            .as_ref()
            .filter(|angstrom| **hook_address == *angstrom.hook_address);

        let hook_data = match angstrom {
            // Angstrom pools are locked at the start of every block, so they need an unlock
            // attestation as their hook data.
            Some(angstrom) => angstrom.attestations.hook_data()?,
            None => v4_user_data.hook_data.to_vec(),
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
            swap_encoder::uniswap_v4::UniswapV4SwapEncoder, utils::ple_encode,
        };

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

            // Both hops carry the same attestation window, on top of 140 bytes of pool params:
            // 90 for the first swap and 50 for the length-prefixed second one. The API decides
            // how many attestations a window holds, so assert the shape rather than the count.
            const POOL_PARAMS_HEX: usize = 280;
            const ATTESTATION_HEX_PER_HOP: usize = 186;
            let attestation_hex = combined_hex
                .len()
                .checked_sub(POOL_PARAMS_HEX)
                .expect("calldata is shorter than the pool params alone");

            assert!(attestation_hex > 0, "no attestation data encoded");
            assert_eq!(
                attestation_hex % (2 * ATTESTATION_HEX_PER_HOP),
                0,
                "attestation data is not a whole number of attestations on both hops"
            );
        }
    }
}
