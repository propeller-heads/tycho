use std::collections::HashMap;

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a native token wrap/unwrap swap.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
/// * `native_token_address` - The address of the native token.
#[derive(Clone)]
pub struct WrapSwapEncoder {
    executor_address: Bytes,
    native_token_address: Bytes,
}

impl SwapEncoder for WrapSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let native_token_address = chain.native_token().address;
        Ok(Self { executor_address, native_token_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let is_wrapping = *swap.token_in().address == self.native_token_address;
        Ok(is_wrapping.abi_encode_packed())
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
    use crate::encoding::{evm::utils::write_calldata_to_file, models::default_token};
    #[test]
    fn test_encode_wrap_wrapping() {
        // ETH -> (wrap) -> WETH
        let pool = ProtocolComponent {
            protocol_system: String::from("native_wrapper"),
            ..Default::default()
        };
        let token_in = Bytes::from("0x0000000000000000000000000000000000000000");
        let token_out = Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let swap = Swap::new(
            pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::default()),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = WrapSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            None,
        )
        .unwrap();

        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(hex_swap, String::from("01").to_lowercase());

        write_calldata_to_file("test_encode_wrap_wrapping", hex_swap.as_str());
    }

    #[test]
    fn test_encode_wrap_unwrapping() {
        // WETH -> (unwrap) -> ETH
        let pool = ProtocolComponent {
            id: String::from(""),
            protocol_system: String::from(""),
            ..Default::default()
        };
        let token_in = Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let token_out = Bytes::from("0x0000000000000000000000000000000000000000");
        let swap = Swap::new(
            pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::default()),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = WrapSwapEncoder::new(
            Bytes::from("0x13aa49bAc059d709dd0a18D6bb63290076a702D7"),
            Chain::Ethereum,
            None,
        )
        .unwrap();

        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(hex_swap, String::from("00").to_lowercase());

        write_calldata_to_file("test_encode_wrap_unwrapping", hex_swap.as_str());
    }
}
