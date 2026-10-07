use std::{collections::HashMap, str::FromStr};

use alloy::{primitives::Bytes as AlloyBytes, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on a Balancer V2 pool through the given executor address.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct BalancerV2SwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for BalancerV2SwapEncoder {
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
        let component_id = AlloyBytes::from_str(&swap.component().id)
            .map_err(|_| EncodingError::FatalError("Invalid component ID".to_string()))?;

        let args = (
            bytes_to_address(&swap.token_in().address)?,
            bytes_to_address(&swap.token_out().address)?,
            component_id,
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
        evm::{swap_encoder::balancer_v2::BalancerV2SwapEncoder, utils::write_calldata_to_file},
        models::{default_token, Swap},
    };
    #[test]
    fn test_encode_balancer_v2() {
        let balancer_pool = ProtocolComponent {
            id: String::from("0x5c6ee304399dbdb9c8ef030ab642b10820db8f56000200000000000000000014"),
            protocol_system: String::from("vm:balancer_v2"),
            ..Default::default()
        };
        let token_in = Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let token_out = Bytes::from("0xba100000625a3754423978a60c9317c58a424e3D");
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
        let encoder = BalancerV2SwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            Some(HashMap::from([(
                "vault_address".to_string(),
                "0xba12222222228d8ba445958a75a0704d566bf2c8".to_string(),
            )])),
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
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                // token out
                "ba100000625a3754423978a60c9317c58a424e3d",
                // pool id
                "5c6ee304399dbdb9c8ef030ab642b10820db8f56000200000000000000000014",
            ))
        );
        write_calldata_to_file("test_encode_balancer_v2", hex_swap.as_str());
    }
}
