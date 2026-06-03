use std::{collections::HashMap, str::FromStr};

use alloy::{primitives::Address, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a Baseline swap through the Baseline executor.
///
/// Calldata layout:
/// - bToken/component id: 20 bytes
/// - token in: 20 bytes
/// - token out: 20 bytes
#[derive(Clone)]
pub struct BaselineSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for BaselineSwapEncoder {
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
        let b_token = Address::from_str(&swap.component().id)
            .map_err(|_| EncodingError::FatalError("Invalid Baseline component id".to_string()))?;
        let token_in = bytes_to_address(&swap.token_in().address)?;
        let token_out = bytes_to_address(&swap.token_out().address)?;

        Ok((b_token, token_in, token_out).abi_encode_packed())
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
    use crate::encoding::models::{default_token, Swap};

    const BTOKEN: &str = "0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63";
    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";

    fn baseline_component() -> ProtocolComponent {
        ProtocolComponent {
            id: BTOKEN.to_string(),
            protocol_system: "baseline".to_string(),
            ..Default::default()
        }
    }

    fn encoding_context(token_in: &Bytes, token_out: &Bytes) -> EncodingContext {
        EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        }
    }

    fn encoder() -> BaselineSwapEncoder {
        BaselineSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            None,
        )
        .unwrap()
    }

    #[test]
    fn test_encode_baseline_buy() {
        let token_in = Bytes::from(WETH);
        let token_out = Bytes::from(BTOKEN);
        let swap = Swap::new(
            baseline_component(),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );

        let encoded_swap = encoder()
            .encode_swap(&swap, &encoding_context(&token_in, &token_out))
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(
            hex_swap,
            String::from(concat!(
                // bToken/component id
                "9fDbDE76236998Dc2836FE67A9954eDE456A1D63",
                // token in
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                // token out
                "9fDbDE76236998Dc2836FE67A9954eDE456A1D63",
            ))
            .to_lowercase()
        );
    }

    #[test]
    fn test_encode_baseline_sell() {
        let token_in = Bytes::from(BTOKEN);
        let token_out = Bytes::from(WETH);
        let swap = Swap::new(
            baseline_component(),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );

        let encoded_swap = encoder()
            .encode_swap(&swap, &encoding_context(&token_in, &token_out))
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(
            hex_swap,
            String::from(concat!(
                // bToken/component id
                "9fDbDE76236998Dc2836FE67A9954eDE456A1D63",
                // token in
                "9fDbDE76236998Dc2836FE67A9954eDE456A1D63",
                // token out
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            ))
            .to_lowercase()
        );
    }
}
