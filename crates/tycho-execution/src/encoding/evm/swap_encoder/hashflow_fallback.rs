use std::{collections::HashMap, str::FromStr};

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        swap_encoder::{fallback::FallbackSwap, hashflow::HashflowSwapEncoder},
        utils::bytes_to_address,
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Protocol config key of the chain's `HashflowFallbackRouter` address.
const FALLBACK_ROUTER_CONFIG_KEY: &str = "fallback_router";

/// Encodes a Hashflow swap for `HashflowFallbackRouter`. The quote is requested with that router
/// as trader.
#[derive(Clone)]
pub struct HashflowFallbackSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
    fallback_router: Bytes,
    hashflow: HashflowSwapEncoder,
}

impl SwapEncoder for HashflowFallbackSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let fallback_router = config
            .as_ref()
            .and_then(|config| config.get(FALLBACK_ROUTER_CONFIG_KEY))
            .ok_or_else(|| {
                EncodingError::FatalError(format!(
                    "Missing {FALLBACK_ROUTER_CONFIG_KEY} in the fallback:rfq:hashflow config for \
                     {chain}: add the chain's HashflowFallbackRouter address to \
                     protocol_specific_addresses.json"
                ))
            })?;
        let fallback_router = Bytes::from_str(fallback_router).map_err(|_| {
            EncodingError::FatalError(format!(
                "Invalid {FALLBACK_ROUTER_CONFIG_KEY} address for {chain}: {fallback_router}"
            ))
        })?;
        let hashflow = HashflowSwapEncoder::new(executor_address.clone(), chain, None)?;
        Ok(Self { executor_address, chain, fallback_router, hashflow })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let fallback = FallbackSwap::from_user_data(swap.user_data())?;
        if !fallback
            .protocol
            .supported_on(self.chain)
        {
            return Err(EncodingError::InvalidInput(format!(
                "Fallback protocol {} is not supported on {}: the chain's HashflowFallbackRouter \
                 would revert the swap",
                fallback.protocol.user_data_name(),
                self.chain
            )));
        }

        let quote_context = EncodingContext {
            router_address: Some(self.fallback_router.clone()),
            ..encoding_context.clone()
        };
        let quote = self
            .hashflow
            .encode_swap(swap, &quote_context)?;
        let token_in = bytes_to_address(&swap.token_in().address)?;
        let token_out = bytes_to_address(&swap.token_out().address)?;

        let mut data = (token_in, token_out).abi_encode_packed();
        data.extend(quote);
        data.extend(fallback.encode()?);
        Ok(data)
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn blocks_on_quote(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::{
            testing_utils::MockRFQState,
            utils::{biguint_to_u256, write_calldata_to_file},
        },
        models::default_token,
    };

    const WETH: &str = "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const USDC: &str = "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const USDC_WETH_USV3: &str = "88e6a0c2ddd26feeb64f039a2c41296fcb3f5640";
    /// The trader of `HashflowFallbackRouterTest`'s signed quote.
    const TRADER: &str = "cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2";

    fn uint(value: u64) -> Bytes {
        Bytes::from(
            biguint_to_u256(&BigUint::from(value))
                .to_be_bytes::<32>()
                .to_vec(),
        )
    }

    fn address(hex: &str) -> Bytes {
        Bytes::from_str(hex).unwrap()
    }

    fn encoder() -> HashflowFallbackSwapEncoder {
        HashflowFallbackSwapEncoder::new(
            Bytes::default(),
            Chain::Ethereum,
            Some(HashMap::from([(FALLBACK_ROUTER_CONFIG_KEY.to_string(), format!("0x{TRADER}"))])),
        )
        .unwrap()
    }

    /// `HashflowExecutorECR20Test`'s signed quote: 1 WETH for 4 286 117 034 USDC units.
    fn weth_usdc_swap(user_data: &str) -> Swap {
        let state = MockRFQState {
            quote_amount_in: None,
            quote_amount_out: BigUint::from(4_286_117_034u64),
            quote_data: HashMap::from([
                ("pool".to_string(), address("0x5d8853028fbF6a2da43c7A828cc5f691E9456B44")),
                (
                    "external_account".to_string(),
                    address("0x9bA0CF1588E1DFA905eC948F7FE5104dD40EDa31"),
                ),
                ("trader".to_string(), address(&format!("0x{TRADER}"))),
                ("effective_trader".to_string(), address(&format!("0x{TRADER}"))),
                ("base_token".to_string(), address(&format!("0x{WETH}"))),
                ("quote_token".to_string(), address(&format!("0x{USDC}"))),
                ("base_token_amount".to_string(), uint(1_000_000_000_000_000_000)),
                ("quote_token_amount".to_string(), uint(4_286_117_034)),
                ("quote_expiry".to_string(), uint(1_755_766_775)),
                ("nonce".to_string(), uint(1_755_766_744_988)),
                (
                    "tx_id".to_string(),
                    address("0x12500006400064000186078c183380ffffffffffffff00296d737ff6ae950000"),
                ),
                (
                    "signature".to_string(),
                    address(
                        "0x649d31cd74f1b11b4a3b32bd38c2525d78ce8f23bc2eaf7700899c3a396d3a137c861737\
                         dc780fa154699eafb3108a34cbb2d4e31a6f0623c169cc19e0fa296a1c",
                    ),
                ),
            ]),
            ..Default::default()
        };
        Swap::new(
            ProtocolComponent {
                id: String::from("hashflow-rfq"),
                protocol_system: String::from("fallback:rfq:hashflow"),
                ..Default::default()
            },
            default_token(address(&format!("0x{WETH}"))),
            default_token(address(&format!("0x{USDC}"))),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(1_000_000_000_000_000_000u64))
        .with_protocol_state(Arc::new(state))
        .with_user_data(Bytes::from(user_data.as_bytes()))
    }

    fn context() -> EncodingContext {
        EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: address(&format!("0x{WETH}")),
            group_token_out: address(&format!("0x{USDC}")),
        }
    }

    /// `HashflowFallbackRouterTest` runs this swap data against the quote on a mainnet fork.
    #[test]
    fn test_encode_hashflow_fallback_for_solidity() {
        let swap = weth_usdc_swap(&format!(
            r#"{{"fallback_protocol":"uniswap_v3","pool":"0x{USDC_WETH_USV3}"}}"#
        ));

        let hex_swap = encode(
            encoder()
                .encode_swap(&swap, &context())
                .unwrap(),
        );

        assert!(
            hex_swap.starts_with(&format!("{WETH}{USDC}5d8853028fbf6a2da43c7a828cc5f691e9456b44"))
        );
        assert!(hex_swap.ends_with(&format!("01{USDC_WETH_USV3}")));
        assert_eq!(hex_swap.len() / 2, 40 + 345 + 21);
        write_calldata_to_file("test_encode_hashflow_fallback_for_solidity", &hex_swap);
    }

    #[test]
    fn test_new_requires_the_fallback_router_config() {
        let err = HashflowFallbackSwapEncoder::new(Bytes::default(), Chain::Ethereum, None)
            .err()
            .unwrap();

        assert!(
            matches!(err, EncodingError::FatalError(message) if message.contains("fallback_router"))
        );
    }

    #[test]
    fn test_rejects_fallback_the_chain_does_not_run() {
        let swap = weth_usdc_swap(&format!(
            r#"{{"fallback_protocol":"aerodrome_v1","pool":"0x{USDC_WETH_USV3}"}}"#
        ));

        let err = encoder()
            .encode_swap(&swap, &context())
            .unwrap_err();

        assert!(
            matches!(err, EncodingError::InvalidInput(message) if message.contains("ethereum"))
        );
    }
}
