use std::{collections::HashMap, str::FromStr};

use alloy::{primitives::Bytes as AlloyBytes, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on a Maverick V2 pool through the given executor address.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct MaverickV2SwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for MaverickV2SwapEncoder {
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
            component_id,
            bytes_to_address(&swap.token_in().address)?,
            bytes_to_address(&swap.token_out().address)?,
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
        evm::{swap_encoder::maverick_v2::MaverickV2SwapEncoder, utils::write_calldata_to_file},
        models::default_token,
    };
    #[test]
    fn test_encode_maverick_v2() {
        // GHO -> (maverick) -> USDC
        let maverick_pool = ProtocolComponent {
            id: String::from("0x14Cf6D2Fe3E1B326114b07d22A6F6bb59e346c67"),
            protocol_system: String::from("vm:maverick_v2"),
            ..Default::default()
        };
        let token_in = Bytes::from("0x40D16FC0246aD3160Ccc09B8D0D3A2cD28aE6C2f");
        let token_out = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let swap = Swap::new(
            maverick_pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::default()),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = MaverickV2SwapEncoder::new(
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
                // pool
                "14Cf6D2Fe3E1B326114b07d22A6F6bb59e346c67",
                // token in
                "40D16FC0246aD3160Ccc09B8D0D3A2cD28aE6C2f",
                // token out
                "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            ))
            .to_lowercase()
        );

        write_calldata_to_file("test_encode_maverick_v2", hex_swap.as_str());
    }
}
