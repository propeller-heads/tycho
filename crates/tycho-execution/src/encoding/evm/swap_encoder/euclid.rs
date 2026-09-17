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
    // Euclid sends a big-endian u64; validate it before narrowing to the executor's u8.
    let offset_bytes: [u8; 8] = partial_fill_offset
        .as_ref()
        .try_into()
        .map_err(|_| {
            EncodingError::FatalError("Euclid partial_fill_offset must be a u64".to_string())
        })?;
    let offset = u64::from_be_bytes(offset_bytes);
    u8::try_from(offset)
        .map_err(|_| EncodingError::FatalError("Euclid partial_fill_offset exceeds u8".to_string()))
}

/// Encodes a swap on Euclid (RFQ) through the given executor address.
///
/// Euclid uses a Request-for-Quote model: binding quotes are obtained
/// off-chain from the Euclid RFQ API and settled on-chain from the maker's
/// inventory via a signed order. The firm quote carries ready-to-execute
/// settlement calldata.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct EuclidSwapEncoder {
    executor_address: Bytes,
    runtime_handle: Handle,
    #[allow(dead_code)]
    runtime: SafeRuntime,
}

impl SwapEncoder for EuclidSwapEncoder {
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
                EncodingError::FatalError("protocol_state is required for Euclid".to_string())
            })?;
        let (target, partial_fill_offset, original_filled_taker_amount, euclid_calldata) = {
            let indicatively_priced_state = protocol_state
                .as_indicatively_priced()
                .map_err(|e| {
                    EncodingError::FatalError(format!("State is not indicatively priced {e}"))
                })?;
            let estimated_amount_in = swap
                .estimated_amount_in()
                .clone()
                .ok_or(EncodingError::FatalError(
                    "Estimated amount in is mandatory for a Euclid swap".to_string(),
                ))?;
            let token_in = swap.token_in().address.clone();
            let token_out = swap.token_out().address.clone();
            let router_address = encoding_context
                .router_address
                .clone()
                .ok_or(EncodingError::FatalError(
                    "The router address is needed to perform a Euclid swap".to_string(),
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
            let euclid_calldata = signed_quote
                .quote_attributes
                .get("calldata")
                .ok_or(EncodingError::FatalError(
                    "Euclid quote must have a calldata attribute".to_string(),
                ))?;
            let partial_fill_offset = signed_quote
                .quote_attributes
                .get("partial_fill_offset")
                .ok_or(EncodingError::FatalError(
                    "Euclid quote must have a partial_fill_offset attribute".to_string(),
                ))?;
            let target = signed_quote
                .quote_attributes
                .get("tx_to")
                .ok_or(EncodingError::FatalError(
                    "Euclid quote must have a tx_to attribute".to_string(),
                ))?;
            let partial_fill_offset = parse_partial_fill_offset(partial_fill_offset)?;
            // The executor compares this with runtime amountIn, so both values must use taker/input
            // units.
            let original_filled_taker_amount = biguint_to_u256(&signed_quote.amount_in);
            (
                bytes_to_address(target)?,
                partial_fill_offset,
                original_filled_taker_amount,
                euclid_calldata.to_vec(),
            )
        };

        // Encode packed data for the executor
        // Format: token_in | token_out | target | partial_fill_offset |
        //         original_filled_taker_amount | euclid_calldata
        let args = (
            token_in,
            token_out,
            target,
            partial_fill_offset.to_be_bytes(),
            original_filled_taker_amount.to_be_bytes::<32>(),
            &euclid_calldata[..],
        );

        Ok(args.abi_encode_packed())
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
    use std::{collections::HashMap, str::FromStr, sync::Arc};

    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::{swap_encoder::euclid::EuclidSwapEncoder, testing_utils::MockRFQState},
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
            EncodingError::FatalError(message) if message == "Euclid partial_fill_offset exceeds u8"
        ));
    }

    #[test]
    fn test_parse_partial_fill_offset_rejects_malformed_values() {
        let error = parse_partial_fill_offset(&Bytes::from(vec![12u8])).unwrap_err();
        assert!(matches!(
            error,
            EncodingError::FatalError(message) if message == "Euclid partial_fill_offset must be a u64"
        ));
    }

    #[test]
    fn test_encode_fails_without_protocol_state() {
        let swap = Swap::new(
            ProtocolComponent {
                id: "euclid-rfq".into(),
                protocol_system: "rfq:euclid".into(),
                ..Default::default()
            },
            default_token(Bytes::from(vec![1u8; 20])),
            default_token(Bytes::from(vec![2u8; 20])),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(1u64));
        let encoder = EuclidSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).unwrap();
        let ctx = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: Bytes::from(vec![1u8; 20]),
            group_token_out: Bytes::from(vec![2u8; 20]),
        };
        let err = encoder
            .encode_swap(&swap, &ctx)
            .unwrap_err();
        assert!(format!("{err:?}").contains("protocol_state"));
    }

    #[test]
    fn test_encode_fails_without_estimated_amount_in() {
        let state = MockRFQState::default();
        let swap = Swap::new(
            ProtocolComponent {
                id: "euclid-rfq".into(),
                protocol_system: "rfq:euclid".into(),
                ..Default::default()
            },
            default_token(Bytes::from(vec![1u8; 20])),
            default_token(Bytes::from(vec![2u8; 20])),
            BigUint::ZERO,
        )
        .with_protocol_state(Arc::new(state));
        let encoder = EuclidSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).unwrap();
        let ctx = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: Bytes::from(vec![1u8; 20]),
            group_token_out: Bytes::from(vec![2u8; 20]),
        };
        let err = encoder
            .encode_swap(&swap, &ctx)
            .unwrap_err();
        assert!(format!("{err:?}").contains("Estimated amount in"));
    }

    #[test]
    fn test_encode_fails_without_router_address() {
        let state = MockRFQState::default();
        let swap = Swap::new(
            ProtocolComponent {
                id: "euclid-rfq".into(),
                protocol_system: "rfq:euclid".into(),
                ..Default::default()
            },
            default_token(Bytes::from(vec![1u8; 20])),
            default_token(Bytes::from(vec![2u8; 20])),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(1u64))
        .with_protocol_state(Arc::new(state));
        let encoder = EuclidSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).unwrap();
        let ctx = EncodingContext {
            router_address: None,
            group_token_in: Bytes::from(vec![1u8; 20]),
            group_token_out: Bytes::from(vec![2u8; 20]),
        };
        let err = encoder
            .encode_swap(&swap, &ctx)
            .unwrap_err();
        assert!(format!("{err:?}").contains("router address"));
    }

    #[test]
    fn test_encode_fails_when_quote_lacks_attributes() {
        // Quote missing tx_to/calldata/offset — every one must be a hard error.
        for missing in ["calldata", "partial_fill_offset", "tx_to"] {
            let mut quote_data = HashMap::from([
                ("calldata".to_string(), Bytes::from_str("0x5a099843").unwrap()),
                ("partial_fill_offset".to_string(), encoded_partial_fill_offset(8)),
                ("tx_to".to_string(), Bytes::from(vec![3u8; 20])),
            ]);
            quote_data.remove(missing);
            let state = MockRFQState {
                quote_amount_in: Some(BigUint::from(1u64)),
                quote_amount_out: BigUint::from(1u64),
                quote_data,
                ..Default::default()
            };
            let swap = Swap::new(
                ProtocolComponent {
                    id: "euclid-rfq".into(),
                    protocol_system: "rfq:euclid".into(),
                    ..Default::default()
                },
                default_token(Bytes::from(vec![1u8; 20])),
                default_token(Bytes::from(vec![2u8; 20])),
                BigUint::ZERO,
            )
            .with_estimated_amount_in(BigUint::from(1u64))
            .with_protocol_state(Arc::new(state));
            let encoder = EuclidSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).unwrap();
            let ctx = EncodingContext {
                router_address: Some(Bytes::zero(20)),
                group_token_in: Bytes::from(vec![1u8; 20]),
                group_token_out: Bytes::from(vec![2u8; 20]),
            };
            let err = encoder
                .encode_swap(&swap, &ctx)
                .unwrap_err();
            assert!(format!("{err:?}").contains(missing), "expected error naming {missing}");
        }
    }

    #[test]
    fn test_trait_accessors() {
        let addr = Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4");
        let encoder = EuclidSwapEncoder::new(addr.clone(), Chain::Ethereum, None).unwrap();
        assert_eq!(encoder.executor_address(), &addr);
        assert!(encoder.blocks_on_quote());
        let _cloned = encoder.clone_box();
    }

    #[test]
    fn test_encode_euclid_swap_with_protocol_state() {
        let euclid_calldata = Bytes::from_str("0x5a099843deadbeef").unwrap();
        let partial_fill_offset = 8u64;
        // 1inch Aggregation Router v5 — Euclid's default settlement target.
        let target = Bytes::from_str("0x1111111254EEB25477B68fb85Ed929f73A960582").unwrap();
        let estimated_amount_in = BigUint::from_str("1000000000000000000").unwrap();
        let quote_amount_in = BigUint::from_str("1000000000000000000").unwrap();
        let quote_amount_out = BigUint::from_str("3496000000").unwrap();

        let euclid_component = ProtocolComponent {
            id: String::from("euclid-rfq"),
            protocol_system: String::from("rfq:euclid"),
            ..Default::default()
        };
        let euclid_state = MockRFQState {
            quote_amount_in: Some(quote_amount_in.clone()),
            quote_amount_out,
            quote_data: HashMap::from([
                ("calldata".to_string(), euclid_calldata.clone()),
                (
                    "partial_fill_offset".to_string(),
                    encoded_partial_fill_offset(partial_fill_offset),
                ),
                ("tx_to".to_string(), target.clone()),
            ]),
            ..Default::default()
        };

        let token_in = Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let token_out = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

        let swap = Swap::new(
            euclid_component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(estimated_amount_in)
        .with_protocol_state(Arc::new(euclid_state));

        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };

        let encoder = EuclidSwapEncoder::new(
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
        assert_eq!(&encoded_swap[93..], euclid_calldata.as_ref());
    }
}
