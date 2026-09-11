use std::{collections::HashMap, str::FromStr};

use alloy::{primitives::Address, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::ROUTER_ETH_ADDRESS,
        utils::{bytes_to_address, convert_to_router_token},
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

#[derive(Clone)]
pub struct FluidV1SwapEncoder {
    executor_address: Bytes,
    chain: Chain,
}

impl SwapEncoder for FluidV1SwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        Ok(Self { executor_address, chain })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let dex_address = Address::from_str(&swap.component().id).map_err(|_| {
            EncodingError::FatalError(format!(
                "Failed parsing FluidV1 component id as ethereum address: {}",
                swap.component().id
            ))
        })?;

        let args = (
            dex_address,
            self.coerce_native_address(&swap.token_in().address) <
                self.coerce_native_address(&swap.token_out().address),
            convert_to_router_token(bytes_to_address(&swap.token_in().address)?),
            convert_to_router_token(bytes_to_address(&swap.token_out().address)?),
            swap.token_in().address == self.chain.native_token().address,
        );
        Ok(args.abi_encode_packed())
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

impl FluidV1SwapEncoder {
    fn coerce_native_address<'a>(&'a self, address: &'a Bytes) -> &'a Bytes {
        if address == &self.chain.native_token().address {
            &ROUTER_ETH_ADDRESS
        } else {
            address
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::swap_encoder::fluid_v1::FluidV1SwapEncoder, models::default_token};

    #[test]
    fn test_encode_fluid_v1() {
        // sUSDe -> (fluid_v1) -> USDT
        let fluid_dex = ProtocolComponent {
            id: String::from("0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b"),
            protocol_system: String::from("fluid_v1"),
            ..Default::default()
        };
        let token_in = Bytes::from("0x9d39a5de30e57443bff2a8307a4256c8797a3497");
        let token_out = Bytes::from("0xdac17f958d2ee523a2206206994597c13d831ec7");
        let swap = Swap::new(
            fluid_dex,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::default()),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = FluidV1SwapEncoder::new(
            Bytes::from("0x212224D2F2d262cd093eE13240ca4873fcCBbA3C"),
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
                // dex
                "1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b",
                // zero2one
                "01",
                // tokenIn (sUSDe)
                "9d39a5de30e57443bff2a8307a4256c8797a3497",
                // tokenOut (USDT)
                "dac17f958d2ee523a2206206994597c13d831ec7",
                // isNativeSell
                "00",
            ))
            .to_lowercase()
        );
    }
}
