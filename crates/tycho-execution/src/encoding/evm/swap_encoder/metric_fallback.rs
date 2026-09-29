use std::collections::HashMap;

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::swap_encoder::{fallback::FallbackSwap, metric::MetricSwapEncoder},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a Metric swap for `MetricFallbackRouter`, which quotes the Metric pool against the
/// fallback protocol named in the swap's `user_data` and runs whichever quotes more.
///
/// The swap data is the `rfq:metric` swap data followed by the fallback:
/// `[tokenIn: 20][tokenOut: 20][pool: 20][zeroForOne: 1][fallback]`.
///
/// # Fields
/// * `executor_address` - The `MetricFallbackExecutor` that performs the swap.
/// * `chain` - The chain whose router runs the swap. Fallback protocols it does not run are
///   rejected.
#[derive(Clone)]
pub struct MetricFallbackSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
    metric: MetricSwapEncoder,
}

impl SwapEncoder for MetricFallbackSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let metric = MetricSwapEncoder::new(executor_address.clone(), chain, config)?;
        Ok(Self { executor_address, chain, metric })
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
                "Fallback protocol {} is not supported on {}: the chain's MetricFallbackRouter \
                 would revert the swap",
                fallback.protocol.user_data_name(),
                self.chain
            )));
        }

        let mut data = self
            .metric
            .encode_swap(swap, encoding_context)?;
        data.extend(fallback.encode()?);
        Ok(data)
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
    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::models::default_token;

    const WETH: &str = "4200000000000000000000000000000000000006";
    const USDC: &str = "833589fcd6edb6e08f4c7c32d4f71b54bda02913";
    const METRIC_POOL: &str = "600668566fc5e9d471a1a235937221e39ac0ed04";
    const USDC_WETH_USV3: &str = "d0b53d9277642d899df5c87a3966a349a798f224";

    fn encode_weth_usdc(user_data: Option<&str>) -> Result<String, EncodingError> {
        let weth = Bytes::from(format!("0x{WETH}").as_str());
        let usdc = Bytes::from(format!("0x{USDC}").as_str());
        let component = ProtocolComponent {
            id: format!("0x{METRIC_POOL}"),
            protocol_system: String::from("fallback:rfq:metric"),
            tokens: vec![weth.clone(), usdc.clone()],
            ..Default::default()
        };
        let mut swap = Swap::new(
            component,
            default_token(weth.clone()),
            default_token(usdc.clone()),
            BigUint::ZERO,
        );
        if let Some(data) = user_data {
            swap = swap.with_user_data(Bytes::from(data.as_bytes()));
        }
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: weth,
            group_token_out: usdc,
        };

        MetricFallbackSwapEncoder::new(Bytes::default(), Chain::Base, None)
            .unwrap()
            .encode_swap(&swap, &encoding_context)
            .map(|encoded| encode(&encoded))
    }

    #[test]
    fn test_encode_metric_with_uniswap_v3_fallback() {
        let hex_swap = encode_weth_usdc(Some(&format!(
            r#"{{"fallback_protocol":"uniswap_v3","pool":"0x{USDC_WETH_USV3}"}}"#
        )))
        .unwrap();

        // WETH is the pool's token0, so zeroForOne is 01; the fallback is protocol byte 01.
        assert_eq!(hex_swap, format!("{WETH}{USDC}{METRIC_POOL}0101{USDC_WETH_USV3}"));
    }

    #[test]
    fn test_rejects_fallback_the_chain_does_not_run() {
        let err = encode_weth_usdc(Some(&format!(
            r#"{{"fallback_protocol":"fluid_v1","dex":"0x{USDC_WETH_USV3}","zero2one":true}}"#
        )))
        .unwrap_err();

        assert!(matches!(err, EncodingError::InvalidInput(message) if message.contains("base")));
    }

    #[test]
    fn test_rejects_missing_fallback() {
        let err = encode_weth_usdc(None).unwrap_err();

        assert!(matches!(err, EncodingError::InvalidInput(_)));
    }
}
