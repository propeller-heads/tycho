use std::collections::HashMap;

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

#[derive(Clone)]
pub struct FermiSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for FermiSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Ethereum {
            return Err(EncodingError::FatalError(
                "FermiSwap swaps are only supported on Ethereum".to_string(),
            ));
        }

        Ok(Self { executor_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let token_in = bytes_to_address(&swap.token_in().address)?;
        let token_out = bytes_to_address(&swap.token_out().address)?;

        let args = (token_in, token_out);
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
    use crate::encoding::{evm::utils::write_calldata_to_file, models::default_token};

    fn weth_usdc_component() -> ProtocolComponent {
        ProtocolComponent {
            id: String::from("0x7c85004568584fbf3665f41ebe85146ee0483587d65d9ea5a56c79816bb720d0"),
            protocol_system: String::from("vm:fermiswap"),
            ..Default::default()
        }
    }

    fn encoder() -> FermiSwapEncoder {
        FermiSwapEncoder::new(
            Bytes::from("0xe8dc788818033232EF9772CB2e6622F1Ec8bc840"),
            Chain::Ethereum,
            None,
        )
        .unwrap()
    }

    #[test]
    fn test_encode_fermiswap_weth_usdc() {
        let token_in = Bytes::from("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let token_out = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let swap = Swap::new(
            weth_usdc_component(),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };

        let encoded_swap = encoder()
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(
            hex_swap,
            String::from(concat!(
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            ))
        );
        write_calldata_to_file("test_encode_fermiswap_weth_usdc", hex_swap.as_str());
    }

    #[test]
    fn test_encode_fermiswap_usdc_weth() {
        let token_in = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let token_out = Bytes::from("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
        let swap = Swap::new(
            weth_usdc_component(),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };

        let encoded_swap = encoder()
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(
            hex_swap,
            String::from(concat!(
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            ))
        );
        write_calldata_to_file("test_encode_fermiswap_usdc_weth", hex_swap.as_str());
    }
}
