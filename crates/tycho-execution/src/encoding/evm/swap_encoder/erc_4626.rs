use std::{collections::HashMap, str::FromStr};

use alloy::{primitives::Bytes as AlloyBytes, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

#[derive(Clone)]
pub struct ERC4626SwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for ERC4626SwapEncoder {
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

        let args = (bytes_to_address(&swap.token_in().address)?, component_id);
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
    use crate::encoding::models::default_token;
    #[test]
    fn test_encode_erc4626_deposit() {
        // WETH -> (spETH) -> spETH
        let sp_eth_pool = ProtocolComponent {
            id: String::from("0xfE6eb3b609a7C8352A241f7F3A21CEA4e9209B8f"),
            protocol_system: String::from("erc4626"),
            ..Default::default()
        };
        let token_in = Bytes::from("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let token_out = Bytes::from("0xfE6eb3b609a7C8352A241f7F3A21CEA4e9209B8f");
        let swap = Swap::new(
            sp_eth_pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = ERC4626SwapEncoder::new(
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
                "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
                // target
                "fE6eb3b609a7C8352A241f7F3A21CEA4e9209B8f",
            ))
            .to_lowercase()
        );
    }

    #[test]
    fn test_encode_erc4626_redeem() {
        // spETH -> (spETH) -> WETH
        let sp_eth_pool = ProtocolComponent {
            id: String::from("0xfE6eb3b609a7C8352A241f7F3A21CEA4e9209B8f"),
            protocol_system: String::from("erc4626"),
            ..Default::default()
        };
        let token_in = Bytes::from("0xfE6eb3b609a7C8352A241f7F3A21CEA4e9209B8f");
        let token_out = Bytes::from("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let swap = Swap::new(
            sp_eth_pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = ERC4626SwapEncoder::new(
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
                "fE6eb3b609a7C8352A241f7F3A21CEA4e9209B8f",
                // target
                "fE6eb3b609a7C8352A241f7F3A21CEA4e9209B8f",
            ))
            .to_lowercase()
        );
    }
}
