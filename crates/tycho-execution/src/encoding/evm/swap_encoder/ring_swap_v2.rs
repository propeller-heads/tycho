use std::{collections::HashMap, str::FromStr};

use alloy::{primitives::Address, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{bytes_to_address, get_static_attribute},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

#[derive(Clone)]
pub struct RingSwapV2SwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for RingSwapV2SwapEncoder {
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
        let component_id = Address::from_str(&swap.component().id).map_err(|_| {
            EncodingError::FatalError("Invalid ring_swap_v2 component id".to_string())
        })?;

        let fw_token0 = static_attribute_address(swap, "fw_token0")?;
        let fw_token1 = static_attribute_address(swap, "fw_token1")?;
        let underlying_token0 = static_attribute_address(swap, "underlying_token0")?;
        let underlying_token1 = static_attribute_address(swap, "underlying_token1")?;

        let (fw_token_in, fw_token_out) = if token_in_address == underlying_token0 &&
            token_out_address == underlying_token1
        {
            (fw_token0, fw_token1)
        } else if token_in_address == underlying_token1 && token_out_address == underlying_token0 {
            (fw_token1, fw_token0)
        } else {
            return Err(EncodingError::InvalidInput(format!(
                "Tokens {} -> {} do not match Ring Swap V2 component {}",
                swap.token_in().address,
                swap.token_out().address,
                swap.component().id
            )));
        };

        Ok((component_id, token_in_address, token_out_address, fw_token_in, fw_token_out)
            .abi_encode_packed())
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

fn static_attribute_address(swap: &Swap, attribute_name: &str) -> Result<Address, EncodingError> {
    let value = Bytes::from(get_static_attribute(swap, attribute_name)?);
    bytes_to_address(&value)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, str::FromStr};

    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::{swap_encoder::ring_swap_v2::RingSwapV2SwapEncoder, utils::write_calldata_to_file},
        models::{default_token, Swap},
    };

    fn ring_component() -> ProtocolComponent {
        let mut static_attributes = HashMap::new();
        static_attributes.insert(
            "fw_token0".to_string(),
            Bytes::from_str("0x8A6fe57C08C84e0f4eE97aAe68a62e820a37d259").unwrap(),
        );
        static_attributes.insert(
            "fw_token1".to_string(),
            Bytes::from_str("0xa250CC729Bb3323e7933022a67B52200fE354767").unwrap(),
        );
        static_attributes.insert(
            "underlying_token0".to_string(),
            Bytes::from_str("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap(),
        );
        static_attributes.insert(
            "underlying_token1".to_string(),
            Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
        );

        ProtocolComponent {
            id: String::from("0x68C498Df05982d635914ee0Ae6501C749A78B473"),
            protocol_system: "ring_swap_v2".to_string(),
            static_attributes,
            ..Default::default()
        }
    }

    fn encoder() -> RingSwapV2SwapEncoder {
        RingSwapV2SwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            None,
        )
        .unwrap()
    }

    #[test]
    fn test_encode_ring_swap_v2_forward() {
        let dai = Bytes::from_str("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap();
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let swap = Swap::new(
            ring_component(),
            default_token(dai.clone()),
            default_token(weth.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: dai,
            group_token_out: weth,
        };

        let hex_swap = encode(
            encoder()
                .encode_swap(&swap, &encoding_context)
                .unwrap(),
        );

        assert_eq!(
            hex_swap,
            String::from(concat!(
                "68c498df05982d635914ee0ae6501c749a78b473",
                "6b175474e89094c44da98b954eedeac495271d0f",
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "8a6fe57c08c84e0f4ee97aae68a62e820a37d259",
                "a250cc729bb3323e7933022a67b52200fe354767",
            ))
        );
        write_calldata_to_file("test_encode_ring_swap_v2_forward", hex_swap.as_str());
    }

    #[test]
    fn test_encode_ring_swap_v2_reverse() {
        let dai = Bytes::from_str("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap();
        let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let swap = Swap::new(
            ring_component(),
            default_token(weth.clone()),
            default_token(dai.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: weth,
            group_token_out: dai,
        };

        let hex_swap = encode(
            encoder()
                .encode_swap(&swap, &encoding_context)
                .unwrap(),
        );

        assert_eq!(
            hex_swap,
            String::from(concat!(
                "68c498df05982d635914ee0ae6501c749a78b473",
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "6b175474e89094c44da98b954eedeac495271d0f",
                "a250cc729bb3323e7933022a67b52200fe354767",
                "8a6fe57c08c84e0f4ee97aae68a62e820a37d259",
            ))
        );
        write_calldata_to_file("test_encode_ring_swap_v2_reverse", hex_swap.as_str());
    }
}
