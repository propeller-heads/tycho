use std::collections::HashMap;

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on DollarStore.
///
/// The executor holds the store address as an immutable and the store routes by
/// token pair, so the protocol data is just the packed token pair.
#[derive(Clone)]
pub struct DollarStoreSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for DollarStoreSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Ethereum {
            return Err(EncodingError::FatalError(
                "DollarStore swaps are only supported on Ethereum".to_string(),
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

        Ok((token_in, token_out).abi_encode_packed())
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

    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const USDG: &str = "0xe343167631d89B6Ffc58B88d6b7fB0228795491D";

    fn usdc_usdg_component() -> ProtocolComponent {
        ProtocolComponent {
            id: String::from("0x097cc630397efe83d759e75de6f9f996bb350abdd015a099f837a2a69c42d5e4"),
            protocol_system: String::from("vm:dollarstore"),
            ..Default::default()
        }
    }

    fn encoder() -> DollarStoreSwapEncoder {
        DollarStoreSwapEncoder::new(
            Bytes::from("0xe916cadb12C49389E487eB1e8194B1459b29B0eC"),
            Chain::Ethereum,
            None,
        )
        .unwrap()
    }

    fn encode_pair(token_in: &str, token_out: &str) -> String {
        let token_in = Bytes::from(token_in);
        let token_out = Bytes::from(token_out);
        let swap = Swap::new(
            usdc_usdg_component(),
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
            origin: None,
        };
        encode(
            encoder()
                .encode_swap(&swap, &encoding_context)
                .unwrap(),
        )
    }

    #[test]
    fn test_encode_dollarstore_usdc_usdg() {
        let hex_swap = encode_pair(USDC, USDG);
        assert_eq!(
            hex_swap,
            String::from(concat!(
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "e343167631d89b6ffc58b88d6b7fb0228795491d",
            ))
        );
        write_calldata_to_file("test_encode_dollarstore_usdc_usdg", hex_swap.as_str());
    }

    #[test]
    fn test_encode_dollarstore_usdg_usdc() {
        assert_eq!(
            encode_pair(USDG, USDC),
            String::from(concat!(
                "e343167631d89b6ffc58b88d6b7fb0228795491d",
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            ))
        );
    }

    #[test]
    fn test_encoder_rejects_non_ethereum_chain() {
        let result = DollarStoreSwapEncoder::new(Bytes::zero(20), Chain::Base, None);
        assert!(matches!(
            result,
            Err(EncodingError::FatalError(msg)) if msg == "DollarStore swaps are only supported on Ethereum"
        ));
    }
}
