use std::collections::HashMap;

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on Tessera V (Wintermute's propAMM on Base).
///
/// The executor holds the `TesseraSwap` entrypoint address as an immutable, so the protocol
/// data is just the packed token pair; the traded book follows from it.
#[derive(Clone)]
pub struct TesseraSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for TesseraSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Base {
            return Err(EncodingError::FatalError(
                "Tessera swaps are only supported on Base".to_string(),
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

    const WETH: &str = "4200000000000000000000000000000000000006";
    const USDC: &str = "833589fcd6edb6e08f4c7c32d4f71b54bda02913";

    fn weth_usdc_component() -> ProtocolComponent {
        ProtocolComponent {
            // tesseraswap (20 bytes) ‖ WETH low 12 bytes, matching the substreams id.
            id: String::from("0x55555522005bcae1c2424d474bfd5ed477749e3e000000000000000000000006"),
            protocol_system: String::from("vm:tessera"),
            ..Default::default()
        }
    }

    fn encoder() -> TesseraSwapEncoder {
        TesseraSwapEncoder::new(
            Bytes::from("0x5c2F5a71f67c01775180adc06909288B4c329308"),
            Chain::Base,
            None,
        )
        .unwrap()
    }

    #[test]
    fn test_encode_tessera_weth_usdc() {
        let token_in = Bytes::from(format!("0x{WETH}").as_str());
        let token_out = Bytes::from(format!("0x{USDC}").as_str());
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

        assert_eq!(hex_swap, format!("{WETH}{USDC}"));
        write_calldata_to_file("test_encode_tessera_weth_usdc", hex_swap.as_str());
    }

    #[test]
    fn test_encode_tessera_usdc_weth() {
        let token_in = Bytes::from(format!("0x{USDC}").as_str());
        let token_out = Bytes::from(format!("0x{WETH}").as_str());
        let swap = Swap::new(
            weth_usdc_component(),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };

        let encoded_swap = encoder()
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        assert_eq!(hex_swap, format!("{USDC}{WETH}"));
        write_calldata_to_file("test_encode_tessera_usdc_weth", hex_swap.as_str());
    }

    #[test]
    fn test_encoder_rejects_non_base_chain() {
        let result = TesseraSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None);
        assert!(result.is_err());
    }
}
