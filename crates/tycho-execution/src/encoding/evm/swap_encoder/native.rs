use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::{primitives::Address, sol_types::SolValue};
use num_bigint::BigUint;
use tokio::runtime::Handle;
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    simulation::indicatively_priced::SignedQuote,
    Bytes,
};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{bytes_to_address, create_encoding_runtime, on_blocking_thread, SafeRuntime},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

#[derive(Clone)]
pub struct NativeSwapEncoder {
    executor_address: Bytes,
    router_v4: Address,
    runtime_handle: Handle,
    #[allow(dead_code)]
    runtime: SafeRuntime,
}

fn validate_quote_amount(
    signed_quote: &SignedQuote,
    requested_amount: &BigUint,
) -> Result<(), EncodingError> {
    if &signed_quote.amount_in != requested_amount {
        return Err(EncodingError::InvalidInput(format!(
            "Native quote amount {} does not match requested amount {}",
            signed_quote.amount_in, requested_amount
        )));
    }

    Ok(())
}

impl SwapEncoder for NativeSwapEncoder {
    fn new(
        executor_address: Bytes,
        _chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let config = config
            .ok_or_else(|| EncodingError::FatalError("Native config is empty".to_string()))?;
        let router_v4 = config
            .get("router_v4")
            .ok_or_else(|| {
                EncodingError::FatalError("Missing router_v4 in Native config".to_string())
            })?
            .parse::<Address>()
            .map_err(|e| {
                EncodingError::FatalError(format!("Invalid router_v4 in Native config: {e}"))
            })?;
        if router_v4 == Address::ZERO {
            return Err(EncodingError::FatalError(
                "Native router_v4 cannot be the zero address".to_string(),
            ));
        }

        let (runtime_handle, runtime) = create_encoding_runtime()?;
        Ok(Self { executor_address, router_v4, runtime_handle, runtime })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let protocol_state = swap
            .protocol_state()
            .as_ref()
            .ok_or_else(|| {
                EncodingError::FatalError("protocol_state is required for Native".to_string())
            })?;

        let amount_in = swap
            .estimated_amount_in()
            .as_ref()
            .ok_or(EncodingError::FatalError(
                "Estimated amount in is mandatory for a Native swap".to_string(),
            ))?
            .clone();

        let sender = encoding_context
            .router_address
            .clone()
            .ok_or(EncodingError::FatalError(
                "The router address is needed to perform a Native swap".to_string(),
            ))?;

        let signed_quote = on_blocking_thread(|| {
            self.runtime_handle.block_on(async {
                protocol_state
                    .as_indicatively_priced()?
                    .request_signed_quote(GetAmountOutParams {
                        amount_in: amount_in.clone(),
                        token_in: swap.token_in().address.clone(),
                        token_out: swap.token_out().address.clone(),
                        sender: sender.clone(),
                        receiver: sender,
                    })
                    .await
            })
        })??;
        // NativeClient already bound response.amountIn and order.sellerTokenAmount to the requested
        // amount. Store that validated quote baseline in the executor header so it can detect
        // under- or over-delivery at execution time.
        validate_quote_amount(&signed_quote, &amount_in)?;
        let target_bytes = signed_quote
            .quote_attributes
            .get("target")
            .ok_or(EncodingError::FatalError(
                "Native quote must have a target attribute".to_string(),
            ))?;

        let target = bytes_to_address(target_bytes)?;
        if target != self.router_v4 {
            return Err(EncodingError::InvalidInput(format!(
                "Native quote target {target} is not configured for this chain"
            )));
        }

        let calldata = signed_quote
            .quote_attributes
            .get("calldata")
            .ok_or(EncodingError::FatalError(
                "Native quote must have a calldata attribute".to_string(),
            ))?;

        let deadline_timestamp = signed_quote
            .quote_attributes
            .get("deadline_timestamp")
            .ok_or(EncodingError::FatalError(
                "Native quote must have a deadline_timestamp attribute".to_string(),
            ))?;
        let deadline = u64::from_be_bytes(
            deadline_timestamp
                .as_ref()
                .try_into()
                .map_err(|_| {
                    EncodingError::InvalidInput(format!(
                        "Native deadline_timestamp must be 8 bytes, got {}",
                        deadline_timestamp.len()
                    ))
                })?,
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| EncodingError::FatalError("SystemTime before UNIX EPOCH".to_string()))?
            .as_secs();
        if deadline <= now {
            return Err(EncodingError::RecoverableError(format!(
                "Native quote expired at {deadline}"
            )));
        }

        let quoted_amount_bytes = amount_in.to_bytes_be();
        if quoted_amount_bytes.len() > 32 {
            return Err(EncodingError::InvalidInput(
                "Native requested amount exceeds uint256".to_string(),
            ));
        }
        let mut encoded_quoted_amount = [0u8; 32];
        encoded_quoted_amount[32 - quoted_amount_bytes.len()..]
            .copy_from_slice(&quoted_amount_bytes);

        // We must translate Tycho's internal `address(0)` representation to the standard
        // EVM `0xEeeee...` address used by TychoRouter so the Executor correctly processes Native
        // ETH.
        let token_in = crate::encoding::evm::utils::convert_to_router_token(bytes_to_address(
            &swap.token_in().address,
        )?);
        let token_out = crate::encoding::evm::utils::convert_to_router_token(bytes_to_address(
            &swap.token_out().address,
        )?);

        // Encode packed data for the executor
        // Format: tokenIn | tokenOut | target | signedAmountIn | native_calldata[..]
        // 20 + 20 + 20 + 32 bytes + dynamic length. Native V4's mutable ABI
        // argument positions are fixed by tradeRFQT's selector and are therefore
        // not accepted from quote metadata.
        // We pack tokenIn and tokenOut at the very beginning so the Solidity NativeExecutor
        // can easily slice them out in `getTransferData` without parsing the opaque
        // `native_calldata`.
        let args = (token_in, token_out, target);
        let mut encoded = args.abi_encode_packed();
        encoded.extend_from_slice(&encoded_quoted_amount);
        encoded.extend_from_slice(calldata);
        Ok(encoded)
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod test {
    use std::{str::FromStr, sync::Arc};

    use alloy::hex::encode;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::{swap_encoder::native::NativeSwapEncoder, testing_utils::MockRFQState},
        models::{default_token, Swap},
    };

    fn native_config() -> Option<HashMap<String, String>> {
        Some(HashMap::from([(
            "router_v4".to_string(),
            "0x8a2ddc0461Fcf96F81a05529Bed540d4f1eb2a00".to_string(),
        )]))
    }

    #[test]
    fn test_encode_native_single_fails_without_protocol_data() {
        let native_component = ProtocolComponent {
            id: String::from("native-rfq"),
            protocol_system: String::from("rfq:native"),
            ..Default::default()
        };

        let token_in = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let token_out = Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

        let swap = Swap::new(
            native_component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from_str("3000000000").unwrap());

        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };

        let encoder = NativeSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            native_config(),
        )
        .unwrap();
        let error = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap_err();

        assert!(matches!(
            error,
            EncodingError::FatalError(message) if message.contains(
                "protocol_state is required for Native"
            )
        ));
    }

    #[test]
    fn test_native_quote_amount_binding_rejects_mismatch() {
        let requested_amount = BigUint::from(1_000_000u64);
        let signed_quote = SignedQuote {
            base_token: Bytes::default(),
            quote_token: Bytes::default(),
            amount_in: BigUint::from(999_999u64),
            amount_out: BigUint::ZERO,
            quote_attributes: HashMap::new(),
        };

        let error = validate_quote_amount(&signed_quote, &requested_amount).unwrap_err();

        assert!(matches!(
            error,
            EncodingError::InvalidInput(message) if message.contains(
                "Native quote amount 999999 does not match requested amount 1000000"
            )
        ));
    }

    #[test]
    fn test_encode_native_single_with_protocol_state() {
        let quote_amount_out = BigUint::from_str("1000000000000000000").unwrap();

        let native_component = ProtocolComponent {
            id: String::from("native-rfq"),
            protocol_system: String::from("rfq:native"),
            ..Default::default()
        };

        let target_address = "0x8a2ddc0461Fcf96F81a05529Bed540d4f1eb2a00";
        let target_bytes = Bytes::from_str(target_address).unwrap();
        let calldata_hex = format!("0947c2d9{:064x}{:064x}{:064x}", 0x60u8, 0u8, 0u8);
        let calldata_bytes = Bytes::from(hex::decode(&calldata_hex).unwrap());
        let native_quote_data = vec![
            ("target".to_string(), target_bytes.clone()),
            ("calldata".to_string(), calldata_bytes.clone()),
            ("deadline_timestamp".to_string(), Bytes::from(u64::MAX.to_be_bytes().to_vec())),
        ];

        let native_state = MockRFQState {
            quote_amount_in: None,
            quote_amount_out,
            quote_data: native_quote_data.into_iter().collect(),
            ..Default::default()
        };

        let token_in = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let token_out = Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

        let swap = Swap::new(
            native_component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from_str("3000000000").unwrap())
        .with_protocol_state(Arc::new(native_state));

        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };

        let encoder = NativeSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            native_config(),
        )
        .unwrap();

        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        let expected_swap = format!(
            "{}{}{}{}{}",
            "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token_in (20 bytes, lowercase, no 0x)
            "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token_out (20 bytes)
            target_address
                .to_lowercase()
                .trim_start_matches("0x"), // target (20 bytes)
            "00000000000000000000000000000000000000000000000000000000b2d05e00", /* signedAmountIn
                                                         * (32 bytes) */
            calldata_hex
        );
        assert_eq!(hex_swap, expected_swap);
    }

    #[test]
    fn test_encode_native_rejects_expired_quote() {
        let target = Bytes::from_str("0x8a2ddc0461Fcf96F81a05529Bed540d4f1eb2a00").unwrap();
        let calldata = Bytes::from(
            hex::decode(format!("0947c2d9{:064x}{:064x}{:064x}", 0x60u8, 0u8, 0u8)).unwrap(),
        );
        let native_state = MockRFQState {
            quote_amount_in: None,
            quote_amount_out: BigUint::from(1_000_000u64),
            quote_data: HashMap::from([
                ("target".to_string(), target),
                ("calldata".to_string(), calldata),
                ("deadline_timestamp".to_string(), Bytes::from(0u64.to_be_bytes().to_vec())),
            ]),
            ..Default::default()
        };
        let token_in = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let token_out = Bytes::from("0xc02aaa39b223fe8d0a0e5C4F27eAD9083C756Cc2");
        let swap = Swap::new(
            ProtocolComponent { protocol_system: "rfq:native".to_string(), ..Default::default() },
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(3_000_000_000u64))
        .with_protocol_state(Arc::new(native_state));
        let context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };
        let encoder = NativeSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            native_config(),
        )
        .unwrap();

        let error = encoder
            .encode_swap(&swap, &context)
            .unwrap_err();

        assert!(matches!(
            error,
            EncodingError::RecoverableError(message) if message.contains("quote expired")
        ));
    }

    #[test]
    fn test_encode_native_rejects_unconfigured_quote_target() {
        let target_bytes = Bytes::from_str("0xb2d1F342D2049684Fb2f8c4eF320633415598333").unwrap();
        let native_state = MockRFQState {
            quote_amount_in: None,
            quote_amount_out: BigUint::from(1_000_000u64),
            quote_data: HashMap::from([
                ("target".to_string(), target_bytes),
                ("calldata".to_string(), Bytes::from(vec![0x09, 0x47, 0xc2, 0xd9])),
            ]),
            ..Default::default()
        };
        let token_in = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let token_out = Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let swap = Swap::new(
            ProtocolComponent { protocol_system: "rfq:native".to_string(), ..Default::default() },
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(3_000_000_000u64))
        .with_protocol_state(Arc::new(native_state));
        let context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };
        let encoder = NativeSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            native_config(),
        )
        .unwrap();

        let error = encoder
            .encode_swap(&swap, &context)
            .unwrap_err();

        assert!(matches!(error, EncodingError::InvalidInput(message) if message.contains(
            "is not configured for this chain"
        )));
    }

    #[test]
    fn test_native_config_requires_non_zero_router() {
        let executor = Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4");

        assert!(matches!(
            NativeSwapEncoder::new(executor.clone(), Chain::Ethereum, None),
            Err(EncodingError::FatalError(message)) if message.contains("config is empty")
        ));

        let mut config = native_config().unwrap();
        config.remove("router_v4");
        assert!(matches!(
            NativeSwapEncoder::new(executor.clone(), Chain::Ethereum, Some(config)),
            Err(EncodingError::FatalError(message)) if message.contains("Missing router_v4")
        ));

        let mut config = native_config().unwrap();
        config.insert(
            "router_v4".to_string(),
            "0x0000000000000000000000000000000000000000".to_string(),
        );
        assert!(matches!(
            NativeSwapEncoder::new(executor, Chain::Ethereum, Some(config)),
            Err(EncodingError::FatalError(message)) if message.contains(
                "router_v4 cannot be the zero address"
            )
        ));
    }
}
