use std::collections::HashMap;

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

const ORACLE_UPDATE_POLICY_ATTR: &str = "oracle_update_policy";
const ORACLE_UPDATE_ARGS_ATTR: &str = "oracle_update_0_args";

#[derive(Clone)]
pub struct MetricSwapEncoder {
    executor_address: Bytes,
    runtime_handle: Handle,
    #[allow(dead_code)]
    runtime: SafeRuntime,
}

// Encoded into MetricExecutor calldata as one byte. Keep these values in sync with
// tycho-simulation's MetricOracleUpdatePolicy and MetricExecutor's mode constants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum MetricOracleUpdatePolicy {
    Never = 0,
    Always = 1,
    RetryOnRevert = 2,
}

impl SwapEncoder for MetricSwapEncoder {
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

        let component = swap.component();
        let pool_address = component
            .id
            .parse::<Bytes>()
            .map_err(|_| {
                EncodingError::FatalError(format!(
                    "Metric component id is not a pool address: {}",
                    component.id
                ))
            })?;
        let pool = bytes_to_address(&pool_address)?;

        let tokens = &component.tokens;
        if tokens.len() != 2 {
            return Err(EncodingError::FatalError(
                "Metric component must contain exactly two tokens".to_string(),
            ));
        }
        let zero_for_one = if swap.token_in().address == tokens[0] &&
            swap.token_out().address == tokens[1]
        {
            true
        } else if swap.token_in().address == tokens[1] && swap.token_out().address == tokens[0] {
            false
        } else {
            return Err(EncodingError::InvalidInput(format!(
                "Metric token pair mismatch: {} -> {} is not {} / {}",
                swap.token_in().address,
                swap.token_out().address,
                tokens[0],
                tokens[1]
            )));
        };

        let oracle_update_policy_attr = component
            .static_attributes
            .get(ORACLE_UPDATE_POLICY_ATTR)
            .ok_or_else(|| {
                EncodingError::FatalError(format!(
                    "Metric component missing {ORACLE_UPDATE_POLICY_ATTR} static attribute"
                ))
            })?;
        if oracle_update_policy_attr.len() != 1 {
            return Err(EncodingError::InvalidInput(format!(
                "Metric {ORACLE_UPDATE_POLICY_ATTR} attribute must be exactly one byte"
            )));
        }
        let oracle_update_policy = match oracle_update_policy_attr[0] {
            0 => MetricOracleUpdatePolicy::Never,
            1 => MetricOracleUpdatePolicy::Always,
            2 => MetricOracleUpdatePolicy::RetryOnRevert,
            value => {
                return Err(EncodingError::InvalidInput(format!(
                    "Metric oracle update policy from {ORACLE_UPDATE_POLICY_ATTR} must be 0, 1, or 2, got {value}"
                )))
            }
        };

        let oracle_update = if matches!(
            oracle_update_policy,
            MetricOracleUpdatePolicy::Always | MetricOracleUpdatePolicy::RetryOnRevert
        ) {
            Some(self.request_oracle_update(swap, encoding_context)?)
        } else {
            None
        };

        let mut encoded = Vec::with_capacity(62);
        encoded.extend_from_slice(token_in.as_slice());
        encoded.extend_from_slice(token_out.as_slice());
        encoded.extend_from_slice(pool.as_slice());
        encoded.push(u8::from(zero_for_one));
        // Byte 61 is the oracle update policy consumed by MetricExecutor.
        encoded.push(oracle_update_policy as u8);

        if let Some(update) = oracle_update {
            let args_len = u32::try_from(update.args.len()).map_err(|_| {
                EncodingError::InvalidInput(
                    "Metric oracle update args are too large to encode".to_string(),
                )
            })?;
            encoded.extend_from_slice(&args_len.to_be_bytes());
            encoded.extend_from_slice(&update.args);
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
    args: Bytes,
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

        let args = signed_oracle_update
            .quote_attributes
            .get(ORACLE_UPDATE_ARGS_ATTR)
            .ok_or(EncodingError::FatalError(format!(
                "Metric quote must have an {ORACLE_UPDATE_ARGS_ATTR} attribute"
            )))?
            .clone();
        if args.is_empty() {
            return Err(EncodingError::InvalidInput(format!(
                "Metric {ORACLE_UPDATE_ARGS_ATTR} cannot be empty"
            )));
        }

        Ok(MetricOracleUpdate { args })
    }
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

    fn component_with_policy(
        token0: &Bytes,
        token1: &Bytes,
        policy: MetricOracleUpdatePolicy,
    ) -> ProtocolComponent {
        ProtocolComponent {
            id: "0x1111111111111111111111111111111111111111".to_string(),
            protocol_system: "rfq:metric".to_string(),
            tokens: vec![token0.clone(), token1.clone()],
            contract_addresses: Vec::new(),
            static_attributes: HashMap::from([(
                ORACLE_UPDATE_POLICY_ATTR.to_string(),
                vec![policy as u8].into(),
            )]),
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

    fn encoder() -> MetricSwapEncoder {
        MetricSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).unwrap()
    }

    #[test]
    fn test_encode_metric_without_oracle_update() {
        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token_out = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let swap = Swap::new(
            component_with_policy(&token_in, &token_out, MetricOracleUpdatePolicy::Never),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoder = encoder();

        let encoded_swap = encoder
            .encode_swap(&swap, &context())
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        let expected = String::from(concat!(
            "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            "1111111111111111111111111111111111111111",
            "01",
            "00",
        ));
        assert_eq!(hex_swap, expected);
    }

    #[test]
    fn test_encode_metric_one_for_zero_encodes_direction_without_price_limit() {
        let token0 = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token1 = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let swap = Swap::new(
            component_with_policy(&token0, &token1, MetricOracleUpdatePolicy::Never),
            default_token(token1.clone()),
            default_token(token0.clone()),
            BigUint::ZERO,
        );
        let encoder = encoder();

        let encoded_swap = encoder
            .encode_swap(&swap, &context())
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(&hex_swap[120..122], "00");
        assert_eq!(&hex_swap[122..124], "00");
        assert_eq!(encoded_swap.len(), 62);
    }

    #[test]
    fn test_encode_metric_with_oracle_update() {
        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token_out = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let first_oracle_args = Bytes::from_str("0xaabbccdd").unwrap();
        let second_oracle_args = Bytes::from_str("0x11223344").unwrap();
        let quote_state = MockRFQState {
            quote_amount_out: BigUint::from(1u64),
            quote_data: HashMap::from([
                (ORACLE_UPDATE_ARGS_ATTR.to_string(), first_oracle_args.clone()),
                ("oracle_update_1_args".to_string(), second_oracle_args.clone()),
            ]),
        };
        let swap = Swap::new(
            component_with_policy(&token_in, &token_out, MetricOracleUpdatePolicy::Always),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(3_000_000_000u64))
        .with_protocol_state(Arc::new(quote_state));
        let encoder = encoder();

        let encoded_swap = encoder
            .encode_swap(&swap, &context())
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        let expected_suffix = String::from(concat!("01", "00000004", "aabbccdd",));
        assert!(hex_swap.ends_with(&expected_suffix));
        assert!(!hex_swap.contains("11223344"));
    }

    #[test]
    fn test_encode_metric_with_retry_oracle_update() {
        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token_out = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let oracle_args = Bytes::from_str("0xaabbccdd").unwrap();
        let quote_state = MockRFQState {
            quote_amount_out: BigUint::from(1u64),
            quote_data: HashMap::from([(ORACLE_UPDATE_ARGS_ATTR.to_string(), oracle_args.clone())]),
        };
        let swap = Swap::new(
            component_with_policy(&token_in, &token_out, MetricOracleUpdatePolicy::RetryOnRevert),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(3_000_000_000u64))
        .with_protocol_state(Arc::new(quote_state));
        let encoder = encoder();

        let encoded_swap = encoder
            .encode_swap(&swap, &context())
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        let expected_suffix = String::from(concat!("02", "00000004", "aabbccdd",));
        assert!(hex_swap.ends_with(&expected_suffix));
    }

    #[test]
    fn test_metric_missing_oracle_update_policy_fails() {
        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token_out = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let swap = Swap::new(
            ProtocolComponent {
                id: "0x1111111111111111111111111111111111111111".to_string(),
                protocol_system: "rfq:metric".to_string(),
                tokens: vec![token_in.clone(), token_out.clone()],
                contract_addresses: Vec::new(),
                ..Default::default()
            },
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoder = encoder();

        let err = encoder
            .encode_swap(&swap, &context())
            .unwrap_err();

        assert!(matches!(err, EncodingError::FatalError(_)));
    }

    #[test]
    fn test_metric_oracle_update_requires_protocol_state_when_enabled() {
        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token_out = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let swap = Swap::new(
            component_with_policy(&token_in, &token_out, MetricOracleUpdatePolicy::Always),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoder = encoder();

        let err = encoder
            .encode_swap(&swap, &context())
            .unwrap_err();

        assert!(matches!(err, EncodingError::FatalError(_)));
    }
}
