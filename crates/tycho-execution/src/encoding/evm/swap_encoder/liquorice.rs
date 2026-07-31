use std::collections::HashMap;

use alloy::sol_types::SolValue;
use tokio::runtime::Handle;
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    Bytes,
};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{bytes_to_address, create_encoding_runtime, on_blocking_thread, SafeRuntime},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on Liquorice (RFQ) through the given executor address.
///
/// Liquorice uses a Request-for-Quote model where quotes are obtained
/// off-chain and settled on-chain. The executor receives pre-encoded
/// calldata from the API.
///
/// # Fields
/// * `executor_address` - The address of the executor contract.
#[derive(Clone)]
pub struct LiquoriceSwapEncoder {
    executor_address: Bytes,
    runtime_handle: Handle,
    #[allow(dead_code)]
    runtime: SafeRuntime,
}

impl SwapEncoder for LiquoriceSwapEncoder {
    fn new(
        executor_address: Bytes,
        _chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let (runtime_handle, runtime) = create_encoding_runtime()?;
        Ok(Self { executor_address, runtime_handle, runtime })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let token_in = bytes_to_address(&swap.token_in().address)?;
        let token_out = bytes_to_address(&swap.token_out().address)?;

        let protocol_state = swap
            .protocol_state()
            .as_ref()
            .ok_or_else(|| {
                EncodingError::FatalError("protocol_state is required for Liquorice".to_string())
            })?;

        let estimated_amount_in = swap
            .estimated_amount_in()
            .clone()
            .ok_or(EncodingError::FatalError(
                "Estimated amount in is mandatory for a Liquorice swap".to_string(),
            ))?;

        let router_address = encoding_context
            .router_address
            .clone()
            .ok_or(EncodingError::FatalError(
                "The router address is needed to perform a Liquorice swap".to_string(),
            ))?;

        let params = GetAmountOutParams {
            amount_in: estimated_amount_in,
            token_in: swap.token_in().address.clone(),
            token_out: swap.token_out().address.clone(),
            sender: router_address.clone(),
            receiver: router_address.clone(),
        };

        let signed_quote = on_blocking_thread(|| {
            self.runtime_handle.block_on(async {
                protocol_state
                    .as_indicatively_priced()
                    .map_err(|e| {
                        EncodingError::FatalError(format!("State is not indicatively priced {e}"))
                    })?
                    .request_signed_quote(params)
                    .await
                    .map_err(|e| EncodingError::FatalError(e.to_string()))
            })
        })??;

        let liquorice_calldata = signed_quote
            .quote_attributes
            .get("calldata")
            .ok_or(EncodingError::FatalError(
                "Liquorice quote must have a calldata attribute".to_string(),
            ))?;

        let base_token_amount = signed_quote
            .quote_attributes
            .get("base_token_amount")
            .ok_or(EncodingError::FatalError(
                "Liquorice quote must have a base_token_amount attribute".to_string(),
            ))?;

        // Defaults to 0 if not present (partial fill not available)
        let partial_fill_offset: [u8; 4] = signed_quote
            .quote_attributes
            .get("partial_fill_offset")
            .map(|b| {
                let mut padded = [0u8; 4];
                if b.len() >= 4 {
                    padded.copy_from_slice(&b[b.len() - 4..]);
                } else {
                    let start = 4 - b.len();
                    padded[start..].copy_from_slice(b);
                }
                padded
            })
            .unwrap_or([0u8; 4]);

        // Defaults to original base token amount if partial fill not
        // available
        let min_base_token_amount = signed_quote
            .quote_attributes
            .get("min_base_token_amount")
            .unwrap_or(base_token_amount);

        let original_base_token_amount = pad_to_32_bytes(base_token_amount);
        let min_base_token_amount = pad_to_32_bytes(min_base_token_amount);

        // Encode packed data for the executor
        // Format: token_in | token_out | partial_fill_offset |
        //         original_base_token_amount | min_base_token_amount |
        //         liquorice_calldata
        let args = (
            token_in,
            token_out,
            partial_fill_offset,
            original_base_token_amount,
            min_base_token_amount,
            liquorice_calldata.as_ref(),
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

fn pad_to_32_bytes(data: &Bytes) -> [u8; 32] {
    let mut padded = [0u8; 32];
    if data.len() >= 32 {
        padded.copy_from_slice(&data[data.len() - 32..]);
    } else {
        let start = 32 - data.len();
        padded[start..].copy_from_slice(data);
    }
    padded
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::{
            swap_encoder::liquorice::LiquoriceSwapEncoder, testing_utils::MockRFQState,
            utils::biguint_to_u256,
        },
        models::default_token,
    };

    fn liquorice_config() -> Option<HashMap<String, String>> {
        Some(HashMap::from([(
            "balance_manager_address".to_string(),
            "0xb87bAE43a665EB5943A5642F81B26666bC9E5C95".to_string(),
        )]))
    }

    #[test]
    fn test_encode_liquorice_single_with_protocol_state() {
        let quote_amount_out = BigUint::from_str("1000000000000000000").unwrap();
        let liquorice_calldata = Bytes::from_str("0xdeadbeef1234567890").unwrap();
        let base_token_amount = biguint_to_u256(&BigUint::from(3_000_000_000_u64))
            .to_be_bytes::<32>()
            .to_vec();

        let liquorice_component = ProtocolComponent {
            id: String::from("liquorice-rfq"),
            protocol_system: String::from("rfq:liquorice"),
            ..Default::default()
        };

        let min_base_token_amount = biguint_to_u256(&BigUint::from(2_500_000_000_u64))
            .to_be_bytes::<32>()
            .to_vec();

        let liquorice_state = MockRFQState {
            quote_amount_out,
            quote_data: HashMap::from([
                ("calldata".to_string(), liquorice_calldata.clone()),
                ("base_token_amount".to_string(), Bytes::from(base_token_amount)),
                ("min_base_token_amount".to_string(), Bytes::from(min_base_token_amount)),
                ("partial_fill_offset".to_string(), Bytes::from(vec![12u8])),
            ]),
        };

        let token_in = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let token_out = Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

        let swap = Swap::new(
            liquorice_component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from_str("3000000000").unwrap())
        .with_protocol_state(Arc::new(liquorice_state));

        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };

        let encoder = LiquoriceSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            liquorice_config(),
        )
        .unwrap();

        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        // Expected format:
        // token_in (20) | token_out (20) | partial_fill_offset (4) |
        // original_base_token_amount (32) | min_base_token_amount (32)
        // | calldata (variable)
        let expected_swap = String::from(concat!(
            // token_in (USDC)
            "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            // token_out (WETH)
            "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            // partial_fill_offset
            "0000000c",
            // original_base_token_amount (3000000000 as U256)
            "00000000000000000000000000000000",
            "000000000000000000000000b2d05e00",
            // min_base_token_amount (2500000000 as U256)
            "00000000000000000000000000000000",
            "0000000000000000000000009502f900",
        ));
        assert_eq!(hex_swap, expected_swap + &liquorice_calldata.to_string()[2..]);
    }
}
