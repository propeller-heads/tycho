use std::collections::HashMap;

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encoder for Tempest, Flowdesk's propAMM.
///
/// Tempest resolves the pool from the token pair alone — the router derives the quote lane as
/// `keccak256(token0 ‖ token1)` internally — so the executor only needs the ordered token pair.
#[derive(Clone)]
pub struct TempestSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for TempestSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Ethereum {
            return Err(EncodingError::FatalError(
                "Tempest swaps are only supported on Ethereum".to_string(),
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

    const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";

    /// The live USDC/WETH pair: id is `keccak(router, USDC, WETH)`, as emitted by the
    /// substreams package.
    fn usdc_weth_component() -> ProtocolComponent {
        ProtocolComponent {
            id: String::from("0x6b7d03b47715d0315ccb75d25300c242e26b51f5202e1a32df20f239e1d9de35"),
            protocol_system: String::from("vm:tempest"),
            ..Default::default()
        }
    }

    fn encoder() -> TempestSwapEncoder {
        TempestSwapEncoder::new(
            Bytes::from("0x5615dEB798BB3E4dFa0139dFa1b3D433Cc23b72f"),
            Chain::Ethereum,
            None,
        )
        .unwrap()
    }

    fn encode_swap_hex(token_in: &str, token_out: &str) -> String {
        let token_in = Bytes::from(token_in);
        let token_out = Bytes::from(token_out);
        let swap = Swap::new(
            usdc_weth_component(),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };

        encode(
            encoder()
                .encode_swap(&swap, &encoding_context)
                .unwrap(),
        )
    }

    #[test]
    fn test_encode_tempest_weth_usdc() {
        let hex_swap = encode_swap_hex(WETH, USDC);

        assert_eq!(
            hex_swap,
            String::from(concat!(
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            ))
        );
        write_calldata_to_file("test_encode_tempest_weth_usdc", hex_swap.as_str());
    }

    /// The reverse direction must encode the tokens as given, not re-sorted: the executor passes
    /// them straight through as `tokenIn`/`tokenOut`, and only the lane derivation inside the
    /// router is order-independent.
    #[test]
    fn test_encode_tempest_usdc_weth() {
        let hex_swap = encode_swap_hex(USDC, WETH);

        assert_eq!(
            hex_swap,
            String::from(concat!(
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            ))
        );
        write_calldata_to_file("test_encode_tempest_usdc_weth", hex_swap.as_str());
    }

    #[test]
    fn test_new_rejects_non_ethereum_chains() {
        let result = TempestSwapEncoder::new(Bytes::zero(20), Chain::Base, None);

        assert!(matches!(result.err(), Some(EncodingError::FatalError(_))));
    }
}
