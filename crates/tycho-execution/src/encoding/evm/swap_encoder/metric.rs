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

const ORACLE_ADDRESS_CONFIG: &str = "oracle_address";
const ORACLE_UPDATE_POLICY_ATTR: &str = "oracle_update_policy";

#[derive(Clone)]
pub struct MetricSwapEncoder {
    executor_address: Bytes,
    oracle_address: Bytes,
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

impl MetricOracleUpdatePolicy {
    fn requires_oracle_update(self) -> bool {
        // RetryOnRevert still needs signed oracle calldata encoded up front; the executor decides
        // on-chain whether to call it after the first swap attempt.
        matches!(self, Self::Always | Self::RetryOnRevert)
    }

    fn from_byte(value: u8, source: &str) -> Result<Self, EncodingError> {
        match value {
            0 => Ok(Self::Never),
            1 => Ok(Self::Always),
            2 => Ok(Self::RetryOnRevert),
            _ => Err(EncodingError::InvalidInput(format!(
                "Metric oracle update policy from {source} must be 0, 1, or 2, got {value}"
            ))),
        }
    }

    fn from_attribute(value: &Bytes) -> Result<Self, EncodingError> {
        if value.len() != 1 {
            return Err(EncodingError::InvalidInput(format!(
                "Metric {ORACLE_UPDATE_POLICY_ATTR} attribute must be exactly one byte"
            )));
        }
        Self::from_byte(value[0], ORACLE_UPDATE_POLICY_ATTR)
    }
}

impl SwapEncoder for MetricSwapEncoder {
    fn new(
        executor_address: Bytes,
        _chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let oracle_address = configured_oracle_address(config)?;
        let (runtime_handle, runtime) = create_encoding_runtime()?;
        Ok(Self { executor_address, oracle_address, runtime_handle, runtime })
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

        let oracle_update_policy = oracle_update_policy(swap)?;
        let oracle_update = if oracle_update_policy.requires_oracle_update() {
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
        // Byte 113 is the oracle update policy consumed by MetricExecutor.
        encoded.push(oracle_update_policy as u8);

        if let Some(update) = oracle_update {
            let calldata_len = u32::try_from(update.calldata.len()).map_err(|_| {
                EncodingError::InvalidInput(
                    "Metric oracle update calldata is too large to encode".to_string(),
                )
            })?;
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
        if target != self.oracle_address {
            return Err(EncodingError::InvalidInput(format!(
                "Metric oracle update target {} does not match configured oracle {}",
                target, self.oracle_address
            )));
        }

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

        Ok(MetricOracleUpdate { calldata })
    }
}

fn configured_oracle_address(
    config: Option<HashMap<String, String>>,
) -> Result<Bytes, EncodingError> {
    let config = config.ok_or_else(|| {
        EncodingError::FatalError(format!("Metric config missing {ORACLE_ADDRESS_CONFIG}"))
    })?;
    let oracle_address = config
        .get(ORACLE_ADDRESS_CONFIG)
        .ok_or_else(|| {
            EncodingError::FatalError(format!("Metric config missing {ORACLE_ADDRESS_CONFIG}"))
        })
        .and_then(|oracle_address| {
            oracle_address
                .parse::<Bytes>()
                .map_err(|_| {
                    EncodingError::FatalError(format!(
                        "Invalid Metric {ORACLE_ADDRESS_CONFIG}: expected an EVM address"
                    ))
                })
        })?;
    bytes_to_address(&oracle_address)?;
    Ok(oracle_address)
}

fn oracle_update_policy(swap: &Swap) -> Result<MetricOracleUpdatePolicy, EncodingError> {
    // No config fallback: Metric components must carry the policy explicitly.
    swap.component()
        .static_attributes
        .get(ORACLE_UPDATE_POLICY_ATTR)
        .ok_or_else(|| {
            EncodingError::FatalError(format!(
                "Metric component missing {ORACLE_UPDATE_POLICY_ATTR} static attribute"
            ))
        })
        .and_then(MetricOracleUpdatePolicy::from_attribute)
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

    fn component_with_policy(
        token0: &Bytes,
        token1: &Bytes,
        policy: MetricOracleUpdatePolicy,
    ) -> ProtocolComponent {
        ProtocolComponent {
            id: "metric-rfq".to_string(),
            protocol_system: "rfq:metric".to_string(),
            tokens: vec![token0.clone(), token1.clone()],
            contract_addresses: vec![
                Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap(),
                Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap(),
                Bytes::from_str("0x3333333333333333333333333333333333333333").unwrap(),
            ],
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

    fn metric_config(oracle_address: &Bytes) -> HashMap<String, String> {
        HashMap::from([(ORACLE_ADDRESS_CONFIG.to_string(), oracle_address.to_string())])
    }

    fn encoder(oracle_address: &Bytes) -> MetricSwapEncoder {
        MetricSwapEncoder::new(
            Bytes::zero(20),
            Chain::Ethereum,
            Some(metric_config(oracle_address)),
        )
        .unwrap()
    }

    #[test]
    fn test_encode_metric_without_oracle_update() {
        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token_out = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let oracle_target = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let swap = Swap::new(
            component_with_policy(&token_in, &token_out, MetricOracleUpdatePolicy::Never),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoder = encoder(&oracle_target);

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
        let oracle_target = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let swap = Swap::new(
            component_with_policy(&token0, &token1, MetricOracleUpdatePolicy::Never),
            default_token(token1.clone()),
            default_token(token0.clone()),
            BigUint::ZERO,
        );
        let encoder = encoder(&oracle_target);

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
            component_with_policy(&token_in, &token_out, MetricOracleUpdatePolicy::Always),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(3_000_000_000u64))
        .with_protocol_state(Arc::new(quote_state));
        let encoder = encoder(&oracle_target);

        let encoded_swap = encoder
            .encode_swap(&swap, &context())
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        let expected_suffix = String::from(concat!("01", "00000008", "78ce3ae1aabbccdd",));
        assert!(hex_swap.ends_with(&expected_suffix));
        assert!(!hex_swap.contains("78ce3ae111223344"));
    }

    #[test]
    fn test_encode_metric_with_retry_oracle_update() {
        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token_out = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let oracle_target = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let oracle_calldata = Bytes::from_str("0x78ce3ae1aabbccdd").unwrap();
        let quote_state = MockRFQState {
            quote_amount_out: BigUint::from(1u64),
            quote_data: HashMap::from([
                ("oracle_update_target".to_string(), oracle_target.clone()),
                ("oracle_update_0_calldata".to_string(), oracle_calldata.clone()),
            ]),
        };
        let swap = Swap::new(
            component_with_policy(&token_in, &token_out, MetricOracleUpdatePolicy::RetryOnRevert),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from(3_000_000_000u64))
        .with_protocol_state(Arc::new(quote_state));
        let encoder = encoder(&oracle_target);

        let encoded_swap = encoder
            .encode_swap(&swap, &context())
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        let expected_suffix = String::from(concat!("02", "00000008", "78ce3ae1aabbccdd",));
        assert!(hex_swap.ends_with(&expected_suffix));
    }

    #[test]
    fn test_metric_missing_oracle_update_policy_fails() {
        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token_out = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let swap = Swap::new(
            ProtocolComponent {
                id: "metric-rfq".to_string(),
                protocol_system: "rfq:metric".to_string(),
                tokens: vec![token_in.clone(), token_out.clone()],
                contract_addresses: vec![
                    Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap(),
                    Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap(),
                    Bytes::from_str("0x3333333333333333333333333333333333333333").unwrap(),
                ],
                ..Default::default()
            },
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let oracle_target = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let encoder = encoder(&oracle_target);

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
        let oracle_target = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let encoder = encoder(&oracle_target);

        let err = encoder
            .encode_swap(&swap, &context())
            .unwrap_err();

        assert!(matches!(err, EncodingError::FatalError(_)));
    }

    #[test]
    fn test_metric_oracle_target_must_match_config() {
        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token_out = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let quote_target = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();
        let configured_oracle =
            Bytes::from_str("0x9999999999999999999999999999999999999999").unwrap();
        let quote_state = MockRFQState {
            quote_amount_out: BigUint::from(1u64),
            quote_data: HashMap::from([
                ("oracle_update_target".to_string(), quote_target),
                (
                    "oracle_update_0_calldata".to_string(),
                    Bytes::from_str("0x78ce3ae1aabbccdd").unwrap(),
                ),
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
        let encoder = encoder(&configured_oracle);

        let err = encoder
            .encode_swap(&swap, &context())
            .unwrap_err();

        assert!(matches!(err, EncodingError::InvalidInput(_)));
    }
}
