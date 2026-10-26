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
        biguint_to_u256, bytes_to_address, check_signed_quote, create_encoding_runtime,
        on_blocking_thread, SafeRuntime,
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on Euclid (RFQ) through the given executor address.
///
/// Euclid's gateway signs a 1inch limit-order-protocol v3 RFQ order for each binding quote and
/// returns the `fillOrderRFQTo` calldata that fills it through the 1inch Aggregation Router v5.
/// The executor pins that router and that call, so the swap data carries the calldata as is,
/// after the amount the quote was signed for: the executor lowers the fill amount when an
/// earlier leg of the route delivered less.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct EuclidSwapEncoder {
    executor_address: Bytes,
    runtime_handle: Handle,
    #[expect(
        dead_code,
        reason = "owns the runtime behind `runtime_handle`; dropping it would shut the runtime down"
    )]
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
        let indicatively_priced_state = protocol_state
            .as_indicatively_priced()
            .map_err(|e| {
                EncodingError::FatalError(format!("State is not indicatively priced {e}"))
            })?;
        let estimated_amount_in = swap
            .estimated_amount_in()
            .clone()
            .ok_or_else(|| {
                EncodingError::FatalError(
                    "Estimated amount in is mandatory for a Euclid swap".to_string(),
                )
            })?;
        let router_address = encoding_context
            .router_address
            .clone()
            .ok_or_else(|| {
                EncodingError::FatalError(
                    "The router address is needed to perform a Euclid swap".to_string(),
                )
            })?;

        // The router fills the order and receives its output, so the gateway signs the order
        // for it and pays to it.
        let params = GetAmountOutParams {
            amount_in: estimated_amount_in,
            token_in: swap.token_in().address.clone(),
            token_out: swap.token_out().address.clone(),
            sender: router_address.clone(),
            receiver: router_address,
            origin: encoding_context.origin.clone(),
        };
        let signed_quote = on_blocking_thread(|| {
            self.runtime_handle.block_on(async {
                indicatively_priced_state
                    .request_signed_quote(params)
                    .await
            })
        })??;
        check_signed_quote(swap, protocol_state.as_ref(), &signed_quote)?;
        let euclid_calldata = signed_quote
            .quote_attributes
            .get("calldata")
            .ok_or_else(|| {
                EncodingError::FatalError("Euclid quote must have a calldata attribute".to_string())
            })?;
        // The amount the order was signed for, in taker units: the executor compares the amount
        // it is given with this and lowers the fill when it has less.
        let original_filled_taker_amount = biguint_to_u256(&signed_quote.amount_in);

        // token_in | token_out | original_filled_taker_amount | euclid_calldata
        let args = (
            token_in,
            token_out,
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

    use alloy::hex;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::testing_utils::MockRFQState, models::default_token};

    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

    fn euclid_swap(state: Option<MockRFQState>, estimated_amount_in: Option<u64>) -> Swap {
        let mut swap = Swap::new(
            ProtocolComponent {
                id: "euclid-rfq".into(),
                protocol_system: "rfq:euclid".into(),
                ..Default::default()
            },
            default_token(Bytes::from_str(WETH).unwrap()),
            default_token(Bytes::from_str(USDC).unwrap()),
            BigUint::ZERO,
        );
        if let Some(amount) = estimated_amount_in {
            swap = swap.with_estimated_amount_in(BigUint::from(amount));
        }
        if let Some(state) = state {
            swap = swap.with_protocol_state(Arc::new(state));
        }
        swap
    }

    fn context(router_address: Option<Bytes>) -> EncodingContext {
        EncodingContext {
            router_address,
            group_token_in: Bytes::from_str(WETH).unwrap(),
            group_token_out: Bytes::from_str(USDC).unwrap(),
            origin: None,
        }
    }

    fn encoder() -> EuclidSwapEncoder {
        EuclidSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).unwrap()
    }

    /// A quote for 1 WETH -> 3496 USDC whose levels price it at `level_amount_out`.
    fn quote(level_amount_out: u64) -> MockRFQState {
        MockRFQState {
            quote_amount_in: Some(BigUint::from(1_000_000_000_000_000_000u64)),
            quote_amount_out: BigUint::from(3_496_000_000u64),
            quote_data: HashMap::from([(
                "calldata".to_string(),
                Bytes::from_str("0x5a099843deadbeef").unwrap(),
            )]),
            level_amount_out: Some(BigUint::from(level_amount_out)),
            ..Default::default()
        }
    }

    /// The whole encoded swap: the two tokens, the signed amount as a 32-byte word, then the
    /// gateway's calldata untouched.
    #[test]
    fn test_encode_euclid_swap_with_protocol_state() {
        let encoded = encoder()
            .encode_swap(
                &euclid_swap(Some(quote(3_500_000_000)), Some(1_000_000_000_000_000_000)),
                &context(Some(Bytes::zero(20))),
            )
            .unwrap();

        assert_eq!(
            hex::encode(encoded),
            concat!(
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "0000000000000000000000000000000000000000000000000de0b6b3a7640000",
                "5a099843deadbeef",
            )
        );
    }

    /// The signed quote is held to the swap's shortfall limit against the levels it was priced
    /// on, like every RFQ venue's: 3496 against 3500 is 12 bps.
    #[test]
    fn test_encode_refuses_a_quote_too_far_below_its_levels() {
        let swap = euclid_swap(Some(quote(3_500_000_000)), Some(1_000_000_000_000_000_000));

        let error = encoder()
            .encode_swap(
                &swap
                    .clone()
                    .with_max_signed_quote_shortfall_bps(10),
                &context(Some(Bytes::zero(20))),
            )
            .unwrap_err();
        assert!(
            matches!(error, EncodingError::RecoverableError(ref message) if message.contains("rfq:euclid")),
            "{error:?}"
        );

        assert!(encoder()
            .encode_swap(
                &swap.with_max_signed_quote_shortfall_bps(12),
                &context(Some(Bytes::zero(20)))
            )
            .is_ok());
    }

    #[test]
    fn test_encode_fails_without_protocol_state() {
        let err = encoder()
            .encode_swap(&euclid_swap(None, Some(1)), &context(Some(Bytes::zero(20))))
            .unwrap_err();
        assert!(format!("{err:?}").contains("protocol_state"));
    }

    #[test]
    fn test_encode_fails_without_estimated_amount_in() {
        let err = encoder()
            .encode_swap(
                &euclid_swap(Some(MockRFQState::default()), None),
                &context(Some(Bytes::zero(20))),
            )
            .unwrap_err();
        assert!(format!("{err:?}").contains("Estimated amount in"));
    }

    #[test]
    fn test_encode_fails_without_router_address() {
        let err = encoder()
            .encode_swap(&euclid_swap(Some(MockRFQState::default()), Some(1)), &context(None))
            .unwrap_err();
        assert!(format!("{err:?}").contains("router address"));
    }

    #[test]
    fn test_encode_fails_when_quote_lacks_calldata() {
        let state = MockRFQState {
            quote_amount_in: Some(BigUint::from(1u64)),
            quote_amount_out: BigUint::from(1u64),
            level_amount_out: Some(BigUint::from(1u64)),
            ..Default::default()
        };
        let err = encoder()
            .encode_swap(&euclid_swap(Some(state), Some(1)), &context(Some(Bytes::zero(20))))
            .unwrap_err();
        assert!(format!("{err:?}").contains("calldata"));
    }

    #[test]
    fn test_trait_accessors() {
        let addr = Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4");
        let encoder = EuclidSwapEncoder::new(addr.clone(), Chain::Ethereum, None).unwrap();
        assert_eq!(encoder.executor_address(), &addr);
        assert!(encoder.blocks_on_quote());
        let _cloned = encoder.clone_box();
    }
}
