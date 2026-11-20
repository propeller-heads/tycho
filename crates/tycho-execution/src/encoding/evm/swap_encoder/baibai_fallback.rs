use std::collections::HashMap;

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::swap_encoder::{baibai::base_and_direction, fallback::FallbackSwap},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a BaiBai swap for `BaibaiFallbackRouter` as `[tokenIn: 20][tokenOut: 20][fallback]`.
/// The router's entrypoint fixes the quote token, so the tokens name the pair.
#[derive(Clone)]
pub struct BaibaiFallbackSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
}

impl SwapEncoder for BaibaiFallbackSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Base {
            return Err(EncodingError::FatalError(
                "BaiBai swaps are only supported on Base".to_string(),
            ));
        }
        Ok(Self { executor_address, chain })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        base_and_direction(swap)?;
        let fallback = FallbackSwap::from_user_data(swap.user_data().as_ref())?;
        if !fallback
            .protocol
            .supported_on(self.chain)
        {
            return Err(EncodingError::InvalidInput(format!(
                "Fallback protocol {} is not supported on {}: the chain's BaibaiFallbackRouter \
                 would revert the swap",
                fallback.protocol.user_data_name(),
                self.chain
            )));
        }

        let mut data = swap.token_in().address.to_vec();
        data.extend_from_slice(&swap.token_out().address);
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
    use rstest::rstest;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::utils::write_calldata_to_file, models::default_token};

    const WETH: &str = "4200000000000000000000000000000000000006";
    const USDC: &str = "833589fcd6edb6e08f4c7c32d4f71b54bda02913";
    const OTHER: &str = "3333333333333333333333333333333333333333";
    const USDC_WETH_USV3: &str = "d0b53d9277642d899df5c87a3966a349a798f224";

    fn address(hex: &str) -> Bytes {
        Bytes::from(format!("0x{hex}").as_str())
    }

    fn encode_swap(
        chain: Chain,
        token_in: &str,
        token_out: &str,
        user_data: Option<&str>,
    ) -> Result<String, EncodingError> {
        let component = ProtocolComponent {
            id: "baibai-weth".to_string(),
            protocol_system: String::from("fallback:pamm:baibai"),
            tokens: vec![address(WETH), address(USDC)],
            static_attributes: HashMap::from([
                ("base".to_string(), address(WETH)),
                ("quote".to_string(), address(USDC)),
            ]),
            ..Default::default()
        };
        let mut swap = Swap::new(
            component,
            default_token(address(token_in)),
            default_token(address(token_out)),
            BigUint::ZERO,
        );
        if let Some(data) = user_data {
            swap = swap.with_user_data(Bytes::from(data.as_bytes()));
        }
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: address(token_in),
            group_token_out: address(token_out),
            origin: None,
        };

        BaibaiFallbackSwapEncoder::new(Bytes::default(), chain, None)?
            .encode_swap(&swap, &encoding_context)
            .map(|encoded| encode(&encoded))
    }

    fn uniswap_v3_fallback() -> String {
        format!(r#"{{"fallback_protocol":"uniswap_v3","pool":"0x{USDC_WETH_USV3}"}}"#)
    }

    #[rstest]
    #[case::sell_base(WETH, USDC, "test_encode_baibai_fallback_sell_base_for_solidity")]
    #[case::buy_base(USDC, WETH, "test_encode_baibai_fallback_buy_base_for_solidity")]
    fn test_encode_baibai_with_uniswap_v3_fallback(
        #[case] token_in: &str,
        #[case] token_out: &str,
        #[case] calldata_name: &str,
    ) {
        let hex_swap =
            encode_swap(Chain::Base, token_in, token_out, Some(&uniswap_v3_fallback())).unwrap();

        // The fallback is protocol byte 01 followed by the Uniswap V3 pool.
        assert_eq!(hex_swap, format!("{token_in}{token_out}01{USDC_WETH_USV3}"));
        // `BaibaiFallbackRouterTest` runs this swap data against BaiBai on a Base fork.
        write_calldata_to_file(calldata_name, &hex_swap);
    }

    #[test]
    fn test_rejects_token_outside_the_pair() {
        let err = encode_swap(Chain::Base, WETH, OTHER, Some(&uniswap_v3_fallback())).unwrap_err();

        assert!(matches!(err, EncodingError::InvalidInput(message) if message.contains("trades")));
    }

    #[test]
    fn test_rejects_missing_fallback() {
        assert!(encode_swap(Chain::Base, WETH, USDC, None).is_err());
    }

    #[test]
    fn test_rejects_fallback_the_chain_does_not_run() {
        let err = encode_swap(
            Chain::Base,
            WETH,
            USDC,
            Some(&format!(
                r#"{{"fallback_protocol":"fluid_v1","dex":"0x{USDC_WETH_USV3}","zero2one":true}}"#
            )),
        )
        .unwrap_err();

        assert!(matches!(err, EncodingError::InvalidInput(message) if message.contains("base")));
    }

    #[test]
    fn test_refuses_other_chains() {
        assert!(BaibaiFallbackSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).is_err());
    }
}
