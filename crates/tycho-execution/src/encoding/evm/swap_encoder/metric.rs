use std::collections::HashMap;

use num_bigint::BigUint;
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

#[derive(Clone)]
pub struct MetricSwapEncoder {
    executor_address: Bytes,
    request_oracle_update: bool,
    runtime_handle: Handle,
    #[allow(dead_code)]
    runtime: SafeRuntime,
}

impl SwapEncoder for MetricSwapEncoder {
    fn new(
        executor_address: Bytes,
        _chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let request_oracle_update =
            parse_bool_config(config.as_ref(), "request_oracle_update")?.unwrap_or(false);
        let (runtime_handle, runtime) = create_encoding_runtime()?;
        Ok(Self { executor_address, request_oracle_update, runtime_handle, runtime })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let token_in = swap.token_in().address.clone();
        let token_out = swap.token_out().address.clone();
        bytes_to_address(&token_in)?;
        bytes_to_address(&token_out)?;

        let pool = contract_address(swap, 0, "pool")?;
        let metric_router = contract_address(swap, 2, "Metric router")?;
        let zero_for_one = zero_for_one(swap)?;
        let price_limit_x64 =
            if zero_for_one { Bytes::zero(32) } else { u256_bytes(&BigUint::from(u128::MAX)) };
        let oracle_update = if self.request_oracle_update {
            Some(self.request_oracle_update(swap, encoding_context)?)
        } else {
            None
        };

        let mut encoded = Vec::with_capacity(114);
        encoded.extend_from_slice(&token_in);
        encoded.extend_from_slice(&token_out);
        encoded.extend_from_slice(&pool);
        encoded.extend_from_slice(&metric_router);
        encoded.push(u8::from(zero_for_one));
        encoded.extend_from_slice(&price_limit_x64);
        encoded.push(u8::from(oracle_update.is_some()));

        if let Some(update) = oracle_update {
            let calldata_len = u32::try_from(update.calldata.len()).map_err(|_| {
                EncodingError::InvalidInput(
                    "Metric oracle update calldata is too large to encode".to_string(),
                )
            })?;
            encoded.extend_from_slice(&update.target);
            encoded.extend_from_slice(&calldata_len.to_be_bytes());
            encoded.extend_from_slice(&update.calldata);
        }

        Ok(encoded)
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

#[derive(Debug)]
struct MetricOracleUpdate {
    target: Bytes,
    calldata: Bytes,
}

impl MetricSwapEncoder {
    fn request_oracle_update(
        &self,
        swap: &Swap,
        encoding_context: &EncodingContext,
    ) -> Result<MetricOracleUpdate, EncodingError> {
        let protocol_state = swap
            .protocol_state()
            .as_ref()
            .ok_or_else(|| {
                EncodingError::FatalError(
                    "protocol_state is required when Metric oracle updates are enabled".to_string(),
                )
            })?;
        let amount_in = swap
            .estimated_amount_in()
            .as_ref()
            .ok_or(EncodingError::FatalError(
                "Estimated amount in is mandatory when Metric oracle updates are enabled"
                    .to_string(),
            ))?
            .clone();
        let router_address = encoding_context
            .router_address
            .clone()
            .ok_or(EncodingError::FatalError(
                "The router address is needed to request Metric oracle updates".to_string(),
            ))?;

        let signed_oracle_update = on_blocking_thread(|| {
            self.runtime_handle.block_on(async {
                protocol_state
                    .as_indicatively_priced()?
                    .request_signed_quote(GetAmountOutParams {
                        amount_in,
                        token_in: swap.token_in().address.clone(),
                        token_out: swap.token_out().address.clone(),
                        sender: router_address.clone(),
                        receiver: router_address,
                    })
                    .await
            })
        })??;

        let target = signed_oracle_update
            .quote_attributes
            .get("oracle_update_target")
            .ok_or(EncodingError::FatalError(
                "Metric quote must have an oracle_update_target attribute".to_string(),
            ))?
            .clone();
        bytes_to_address(&target)?;

        let calldata = signed_oracle_update
            .quote_attributes
            .get("oracle_update_0_calldata")
            .ok_or(EncodingError::FatalError(
                "Metric quote must have an oracle_update_0_calldata attribute".to_string(),
            ))?
            .clone();
        if calldata.is_empty() {
            return Err(EncodingError::InvalidInput(
                "Metric oracle_update_0_calldata cannot be empty".to_string(),
            ));
        }

        Ok(MetricOracleUpdate { target, calldata })
    }
}

fn parse_bool_config(
    config: Option<&HashMap<String, String>>,
    key: &str,
) -> Result<Option<bool>, EncodingError> {
    let Some(value) = config.and_then(|config| config.get(key)) else {
        return Ok(None);
    };
    match value.as_str() {
        "true" | "1" => Ok(Some(true)),
        "false" | "0" => Ok(Some(false)),
        _ => Err(EncodingError::InvalidInput(format!(
            "Metric config {key} must be true/false or 1/0, got {value}"
        ))),
    }
}

fn contract_address(swap: &Swap, index: usize, name: &str) -> Result<Bytes, EncodingError> {
    let address = swap
        .component()
        .contract_addresses
        .get(index)
        .ok_or_else(|| {
            EncodingError::FatalError(format!("Metric component missing {name} address"))
        })?
        .clone();
    bytes_to_address(&address)?;
    Ok(address)
}

fn zero_for_one(swap: &Swap) -> Result<bool, EncodingError> {
    let tokens = &swap.component().tokens;
    if tokens.len() != 2 {
        return Err(EncodingError::FatalError(
            "Metric component must contain exactly two tokens".to_string(),
        ));
    }

    if swap.token_in().address == tokens[0] && swap.token_out().address == tokens[1] {
        Ok(true)
    } else if swap.token_in().address == tokens[1] && swap.token_out().address == tokens[0] {
        Ok(false)
    } else {
        Err(EncodingError::InvalidInput(format!(
            "Metric token pair mismatch: {} -> {} is not {} / {}",
            swap.token_in().address,
            swap.token_out().address,
            tokens[0],
            tokens[1]
        )))
    }
}

fn u256_bytes(value: &BigUint) -> Bytes {
    biguint_to_u256(value)
        .to_be_bytes::<32>()
        .to_vec()
        .into()
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::{swap_encoder::metric::MetricSwapEncoder, testing_utils::MockRFQState},
        models::default_token,
    };

    fn component(token0: &Bytes, token1: &Bytes) -> ProtocolComponent {
        ProtocolComponent {
            id: "metric-rfq".to_string(),
            protocol_system: "rfq:metric".to_string(),
            tokens: vec![token0.clone(), token1.clone()],
            contract_addresses: vec![
                Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap(),
                Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap(),
                Bytes::from_str("0x3333333333333333333333333333333333333333").unwrap(),
            ],
            ..Default::default()
        }
    }

    fn context() -> EncodingContext {
        EncodingContext {
            router_address: Some(
                Bytes::from_str("0x4444444444444444444444444444444444444444").unwrap(),
            ),
            group_token_in: Bytes::zero(20),
            group_token_out: Bytes::zero(20),
        }
    }

    fn config(request_oracle_update: bool) -> Option<HashMap<String, String>> {
        Some(HashMap::from([(
            "request_oracle_update".to_string(),
            request_oracle_update.to_string(),
        )]))
    }

    #[test]
    fn test_encode_metric_without_oracle_update() {
        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token_out = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let swap = Swap::new(
            component(&token_in, &token_out),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoder =
            MetricSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, config(false)).unwrap();

        let encoded_swap = encoder
            .encode_swap(&swap, &context())
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        let expected = String::from(concat!(
            "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            "1111111111111111111111111111111111111111",
            "3333333333333333333333333333333333333333",
            "01",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "00",
        ));
        assert_eq!(hex_swap, expected);
    }

    #[test]
    fn test_encode_metric_one_for_zero_uses_max_price_limit() {
        let token0 = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token1 = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let swap = Swap::new(
            component(&token0, &token1),
            default_token(token1.clone()),
            default_token(token0.clone()),
            BigUint::ZERO,
        );
        let encoder = MetricSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).unwrap();

        let encoded_swap = encoder
            .encode_swap(&swap, &context())
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(&hex_swap[160..162], "00");
        assert_eq!(
            &hex_swap[162..226],
            "00000000000000000000000000000000ffffffffffffffffffffffffffffffff"
        );
    }

    #[test]
    fn test_encode_metric_with_oracle_update() {
        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token_out = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let oracle_target = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let first_oracle_calldata = Bytes::from_str("0x78ce3ae1aabbccdd").unwrap();
        let second_oracle_calldata = Bytes::from_str("0x78ce3ae111223344").unwrap();
        let quote_state = MockRFQState {
            quote_amount_out: BigUint::from(1u64),
            quote_data: HashMap::from([
                ("oracle_update_target".to_string(), oracle_target.clone()),
                ("oracle_update_0_calldata".to_string(), first_oracle_calldata.clone()),
                ("oracle_update_1_calldata".to_string(), second_oracle_calldata.clone()),
            ]),
        };
        let swap = Swap::new(
            component(&token_in, &token_out),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(3_000_000_000u64))
        .with_protocol_state(Arc::new(quote_state));
        let encoder =
            MetricSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, config(true)).unwrap();

        let encoded_swap = encoder
            .encode_swap(&swap, &context())
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        let expected_suffix = String::from(concat!(
            "01",
            "2222222222222222222222222222222222222222",
            "00000008",
            "78ce3ae1aabbccdd",
        ));
        assert!(hex_swap.ends_with(&expected_suffix));
        assert!(!hex_swap.contains("78ce3ae111223344"));
    }

    #[test]
    fn test_metric_oracle_update_requires_protocol_state_when_enabled() {
        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token_out = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let swap = Swap::new(
            component(&token_in, &token_out),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoder =
            MetricSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, config(true)).unwrap();

        let err = encoder
            .encode_swap(&swap, &context())
            .unwrap_err();

        assert!(matches!(err, EncodingError::FatalError(_)));
    }
}
