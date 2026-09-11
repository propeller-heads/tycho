use std::{collections::HashMap, str::FromStr};

use alloy::{primitives::Address, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on a Balancer V3 pool through the given executor address.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct BalancerV3SwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for BalancerV3SwapEncoder {
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
        let pool = Address::from_str(&swap.component().id).map_err(|_| {
            EncodingError::FatalError("Invalid pool address for Balancer v3".to_string())
        })?;

        let args = (
            bytes_to_address(&swap.token_in().address)?,
            bytes_to_address(&swap.token_out().address)?,
            pool,
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

#[cfg(test)]
mod tests {
    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::{swap_encoder::balancer_v3::BalancerV3SwapEncoder, utils::write_calldata_to_file},
        models::default_token,
    };

    #[test]
    fn test_encode_balancer_v3() {
        let balancer_pool = ProtocolComponent {
            id: String::from("0x85b2b559bc2d21104c4defdd6efca8a20343361d"),
            protocol_system: String::from("vm:balancer_v3"),
            ..Default::default()
        };
        let token_in = Bytes::from("0x7bc3485026ac48b6cf9baf0a377477fff5703af8");
        let token_out = Bytes::from("0xc71ea051a5f82c67adcf634c36ffe6334793d24c");
        let swap = Swap::new(
            balancer_pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = BalancerV3SwapEncoder::new(
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
                // token in
                "7bc3485026ac48b6cf9baf0a377477fff5703af8",
                // token out
                "c71ea051a5f82c67adcf634c36ffe6334793d24c",
                // pool id
                "85b2b559bc2d21104c4defdd6efca8a20343361d",
            ))
        );
        write_calldata_to_file("test_encode_balancer_v3", hex_swap.as_str());
    }
}
