use std::{collections::HashMap, str::FromStr};

use alloy::{primitives::Address, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{bytes_to_address, convert_to_router_token},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

#[derive(Clone)]
pub struct LunarBaseSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for LunarBaseSwapEncoder {
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
        let pool = Address::from_str(&swap.component().id)
            .map_err(|_| EncodingError::FatalError("Invalid LunarBase component id".to_owned()))?;
        let token_in = convert_to_router_token(bytes_to_address(&swap.token_in().address)?);
        let token_out = convert_to_router_token(bytes_to_address(&swap.token_out().address)?);

        Ok((pool, token_in, token_out).abi_encode_packed())
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

    #[test]
    fn encodes_pool_token_in_token_out_packed() {
        let component = ProtocolComponent {
            id: "0x0000efc4ec03a7c47d3a38a9be7ff1d52dd01b99".to_owned(),
            protocol_system: "lunarbase".to_owned(),
            ..Default::default()
        };
        let token_in = Bytes::from("0x0000000000000000000000000000000000000000");
        let token_out = Bytes::from("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913");
        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };
        let encoder = LunarBaseSwapEncoder::new(Bytes::zero(20), Chain::Base, None).unwrap();

        assert_eq!(
            encode(
                encoder
                    .encode_swap(&swap, &context)
                    .unwrap()
            ),
            concat!(
                "0000efc4ec03a7c47d3a38a9be7ff1d52dd01b99",
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                "833589fcd6edb6e08f4c7c32d4f71b54bda02913",
            )
        );
    }
}
