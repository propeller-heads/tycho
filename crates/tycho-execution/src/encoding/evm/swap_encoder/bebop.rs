use std::collections::HashMap;

use alloy::sol_types::SolValue;
use tokio::runtime::Handle;
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    Bytes,
};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{
        biguint_to_u256, bytes_to_address, create_encoding_runtime, on_blocking_thread, SafeRuntime,
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

fn parse_partial_fill_offset(partial_fill_offset: &Bytes) -> Result<u8, EncodingError> {
    // Bebop sends a big-endian u64; validate it before narrowing to the executor's u8.
    let offset_bytes: [u8; 8] = partial_fill_offset
        .as_ref()
        .try_into()
        .map_err(|_| {
            EncodingError::FatalError("Bebop partial_fill_offset must be a u64".to_string())
        })?;
    let offset = u64::from_be_bytes(offset_bytes);
    u8::try_from(offset)
        .map_err(|_| EncodingError::FatalError("Bebop partial_fill_offset exceeds u8".to_string()))
}

/// Encodes a swap on Bebop (PMM RFQ) through the given executor address.
///
/// Bebop uses a Request-for-Quote model where quotes are obtained off-chain
/// and settled on-chain. This encoder supports PMM RFQ execution.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct BebopSwapEncoder {
    executor_address: Bytes,
    runtime_handle: Handle,
    #[allow(dead_code)]
    runtime: SafeRuntime,
}

impl SwapEncoder for BebopSwapEncoder {
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
                EncodingError::FatalError("protocol_state is required for Bebop".to_string())
            })?;
        let (target, partial_fill_offset, original_filled_taker_amount, bebop_calldata) = {
            let indicatively_priced_state = protocol_state
                .as_indicatively_priced()
                .map_err(|e| {
                    EncodingError::FatalError(format!("State is not indicatively priced {e}"))
                })?;
            let estimated_amount_in = swap
                .estimated_amount_in()
                .clone()
                .ok_or(EncodingError::FatalError(
                    "Estimated amount in is mandatory for a Bebop swap".to_string(),
                ))?;
            let token_in = swap.token_in().address.clone();
            let token_out = swap.token_out().address.clone();
            let router_address = encoding_context
                .router_address
                .clone()
                .ok_or(EncodingError::FatalError(
                    "The router address is needed to perform a Bebop swap".to_string(),
                ))?;

            let params = GetAmountOutParams {
                amount_in: estimated_amount_in,
                token_in,
                token_out,
                sender: router_address.clone(),
                receiver: router_address,
            };
            let signed_quote = on_blocking_thread(|| {
                self.runtime_handle.block_on(async {
                    indicatively_priced_state
                        .request_signed_quote(params)
                        .await
                })
            })??;
            let bebop_calldata = signed_quote
                .quote_attributes
                .get("calldata")
                .ok_or(EncodingError::FatalError(
                    "Bebop quote must have a calldata attribute".to_string(),
                ))?;
            let partial_fill_offset = signed_quote
                .quote_attributes
                .get("partial_fill_offset")
                .ok_or(EncodingError::FatalError(
                    "Bebop quote must have a partial_fill_offset attribute".to_string(),
                ))?;
            let target = signed_quote
                .quote_attributes
                .get("tx_to")
                .ok_or(EncodingError::FatalError(
                    "Bebop quote must have a tx_to attribute".to_string(),
                ))?;
            let partial_fill_offset = parse_partial_fill_offset(partial_fill_offset)?;
            // The executor compares this with runtime amountIn, so both values must use taker/input
            // units.
            let original_filled_taker_amount = biguint_to_u256(&signed_quote.amount_in);
            (
                bytes_to_address(target)?,
                partial_fill_offset,
                original_filled_taker_amount,
                bebop_calldata.to_vec(),
            )
        };

        // Encode packed data for the executor
        // Format: token_in | token_out | target | partial_fill_offset |
        //         original_filled_taker_amount | bebop_calldata
        let args = (
            token_in,
            token_out,
            target,
            partial_fill_offset.to_be_bytes(),
            original_filled_taker_amount.to_be_bytes::<32>(),
            &bebop_calldata[..],
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

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::{swap_encoder::bebop::BebopSwapEncoder, testing_utils::MockRFQState},
        models::default_token,
    };

    fn encoded_partial_fill_offset(partial_fill_offset: u64) -> Bytes {
        Bytes::from(
            partial_fill_offset
                .to_be_bytes()
                .to_vec(),
        )
    }

    #[test]
    fn test_parse_partial_fill_offset_accepts_u8_values() {
        assert_eq!(parse_partial_fill_offset(&encoded_partial_fill_offset(0)).unwrap(), 0);
        assert_eq!(parse_partial_fill_offset(&encoded_partial_fill_offset(255)).unwrap(), 255);
    }

    #[test]
    fn test_parse_partial_fill_offset_rejects_values_over_u8() {
        let error = parse_partial_fill_offset(&encoded_partial_fill_offset(256)).unwrap_err();
        assert!(matches!(
            error,
            EncodingError::FatalError(message) if message == "Bebop partial_fill_offset exceeds u8"
        ));
    }

    #[test]
    fn test_parse_partial_fill_offset_rejects_malformed_values() {
        let error = parse_partial_fill_offset(&Bytes::from(vec![12u8])).unwrap_err();
        assert!(matches!(
            error,
            EncodingError::FatalError(message) if message == "Bebop partial_fill_offset must be a u64"
        ));
    }

    #[test]
    fn test_encode_bebop_single_with_protocol_state() {
        let bebop_calldata = Bytes::from_str("0x123456").unwrap();
        let partial_fill_offset = 12u64;
        let target = Bytes::from_str("0xbbbbbBB520d69a9775E85b458C58c648259FAD5F").unwrap();
        let estimated_amount_in = BigUint::from_str("19000000000000000000").unwrap();
        let quote_amount_in = BigUint::from_str("20000000000000000000").unwrap();
        let quote_amount_out = BigUint::from_str("20000000").unwrap();

        let bebop_component = ProtocolComponent {
            id: String::from("bebop-rfq"),
            protocol_system: String::from("rfq:bebop"),
            ..Default::default()
        };
        let bebop_state = MockRFQState {
            quote_amount_in: Some(quote_amount_in.clone()),
            quote_amount_out,
            quote_data: HashMap::from([
                ("calldata".to_string(), bebop_calldata.clone()),
                (
                    "partial_fill_offset".to_string(),
                    encoded_partial_fill_offset(partial_fill_offset),
                ),
                ("tx_to".to_string(), target.clone()),
            ]),
        };

        let token_in = Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let token_out = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

        let swap = Swap::new(
            bebop_component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(estimated_amount_in)
        .with_protocol_state(Arc::new(bebop_state));

        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };

        let encoder = BebopSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            None,
        )
        .unwrap();

        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();

        assert_eq!(&encoded_swap[0..20], token_in.as_ref());
        assert_eq!(&encoded_swap[20..40], token_out.as_ref());
        assert_eq!(&encoded_swap[40..60], target.as_ref());
        assert_eq!(encoded_swap[60], partial_fill_offset as u8);
        assert_eq!(&encoded_swap[61..93], &biguint_to_u256(&quote_amount_in).to_be_bytes::<32>());
        assert_eq!(&encoded_swap[93..], bebop_calldata.as_ref());
    }
}
