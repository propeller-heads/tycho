use std::{collections::HashMap, str::FromStr};

use alloy::{primitives::Address, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on a Uniswap V2 pool through the given executor address.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct UniswapV2SwapEncoder {
    executor_address: Bytes,
}

impl UniswapV2SwapEncoder {}

impl SwapEncoder for UniswapV2SwapEncoder {
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
        let token_in_address = bytes_to_address(&swap.token_in().address)?;
        let token_out_address = bytes_to_address(&swap.token_out().address)?;
        let component_id = Address::from_str(&swap.component().id)
            .map_err(|_| EncodingError::FatalError("Invalid USV2 component id".to_string()))?;

        Ok((component_id, token_in_address, token_out_address).abi_encode_packed())
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
    use crate::encoding::{
        evm::{swap_encoder::uniswap_v2::UniswapV2SwapEncoder, utils::write_calldata_to_file},
        models::{default_token, Swap},
    };
    #[test]
    fn test_encode_uniswap_v2() {
        let usv2_pool = ProtocolComponent {
            id: String::from("0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11"),
            ..Default::default()
        };

        let token_in = Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let token_out = Bytes::from("0x6b175474e89094c44da98b954eedeac495271d0f");
        let swap = Swap::new(
            usv2_pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = UniswapV2SwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            None,
        )
        .unwrap();
        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);
        assert_eq!(
            hex_swap,
            String::from(concat!(
                // component id (pool address)
                "a478c2975ab1ea89e8196811f51a7b7ade33eb11",
                // tokenIn
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                // tokenOut
                "6b175474e89094c44da98b954eedeac495271d0f",
            ))
        );
        write_calldata_to_file("test_encode_uniswap_v2", hex_swap.as_str());
    }
}
