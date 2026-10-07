use std::{collections::HashMap, str::FromStr};

use alloy::{primitives::Address, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on an Aerodrome V1 pool through the given executor address.
#[derive(Clone)]
pub struct AerodromeV1SwapEncoder {
    executor_address: Bytes,
}

impl AerodromeV1SwapEncoder {}

impl SwapEncoder for AerodromeV1SwapEncoder {
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
        let zero_for_one = token_in_address < token_out_address;
        let component_id = Address::from_str(&swap.component().id).map_err(|_| {
            EncodingError::FatalError("Invalid aerodrome_v1 component id".to_string())
        })?;

        Ok((component_id, token_in_address, token_out_address, zero_for_one).abi_encode_packed())
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
        evm::{swap_encoder::aerodrome_v1::AerodromeV1SwapEncoder, utils::write_calldata_to_file},
        models::{default_token, Swap},
    };

    #[test]
    fn test_encode_aerodrome_v1() {
        let pool = ProtocolComponent {
            id: String::from("0x723aef6543aece026a15662be4d3fb3424d502a9"),
            ..Default::default()
        };

        let token_in = Bytes::from("0x236aa50979d5f3de3bd1eeb40e81137f22ab794b");
        let token_out = Bytes::from("0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca");
        let swap = Swap::new(
            pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };

        let encoder = AerodromeV1SwapEncoder::new(
            Bytes::from("0x1111111111111111111111111111111111111111"),
            Chain::Base,
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
                "723aef6543aece026a15662be4d3fb3424d502a9",
                "236aa50979d5f3de3bd1eeb40e81137f22ab794b",
                "d9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca",
                "01",
            ))
        );

        write_calldata_to_file("test_encode_aerodrome_v1", hex_swap.as_str());
    }
}
