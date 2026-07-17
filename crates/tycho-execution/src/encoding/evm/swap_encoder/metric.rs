use std::collections::HashMap;

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

// Oracle update mode byte consumed by MetricExecutor. Under the v1 heartbeat model Tycho never
// relays an oracle update, so the encoder always emits `Never`. Kept as a named constant to stay
// lined up with MetricExecutor's OracleUpdateMode enum.
const ORACLE_UPDATE_MODE_NEVER: u8 = 0;

#[derive(Clone)]
pub struct MetricSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for MetricSwapEncoder {
    fn new(
        executor_address: Bytes,
        _chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        Ok(Self { executor_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
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

        let mut encoded = Vec::with_capacity(62);
        encoded.extend_from_slice(token_in.as_slice());
        encoded.extend_from_slice(token_out.as_slice());
        encoded.extend_from_slice(pool.as_slice());
        encoded.push(u8::from(zero_for_one));
        // Byte 61 is the oracle update mode consumed by MetricExecutor.
        encoded.push(ORACLE_UPDATE_MODE_NEVER);

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
mod tests {
    use std::str::FromStr;

    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::swap_encoder::metric::MetricSwapEncoder, models::default_token};

    fn component(token0: &Bytes, token1: &Bytes) -> ProtocolComponent {
        ProtocolComponent {
            id: "0x1111111111111111111111111111111111111111".to_string(),
            protocol_system: "rfq:metric".to_string(),
            tokens: vec![token0.clone(), token1.clone()],
            contract_addresses: Vec::new(),
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
    fn test_encode_metric_zero_for_one() {
        let token_in = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token_out = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let swap = Swap::new(
            component(&token_in, &token_out),
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
        assert_eq!(encoded_swap.len(), 62);
    }

    #[test]
    fn test_encode_metric_one_for_zero_encodes_direction() {
        let token0 = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token1 = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let swap = Swap::new(
            component(&token0, &token1),
            default_token(token1.clone()),
            default_token(token0.clone()),
            BigUint::ZERO,
        );
        let encoder = encoder();

        let encoded_swap = encoder
            .encode_swap(&swap, &context())
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        // Byte 60 (direction) is 0 for one-for-zero, byte 61 (oracle mode) is always Never.
        assert_eq!(&hex_swap[120..122], "00");
        assert_eq!(&hex_swap[122..124], "00");
        assert_eq!(encoded_swap.len(), 62);
    }

    #[test]
    fn test_encode_metric_token_pair_mismatch_fails() {
        let token0 = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let token1 = Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let other = Bytes::from_str("0x0000000000000000000000000000000000000009").unwrap();
        let swap = Swap::new(
            component(&token0, &token1),
            default_token(token0.clone()),
            default_token(other.clone()),
            BigUint::ZERO,
        );
        let encoder = encoder();

        let err = encoder
            .encode_swap(&swap, &context())
            .unwrap_err();

        assert!(matches!(err, EncodingError::InvalidInput(_)));
    }
}
