use std::collections::HashMap;

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{swap_encoder::fallback::FallbackSwap, utils::bytes_to_address},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a Tessera swap for `TesseraFallbackRouter`, which quotes Tessera against the fallback
/// protocol named in the swap's `user_data` and runs whichever pays more.
///
/// The swap data is `token_in ++ token_out ++ fallback`: `TesseraSwap` finds the pair by its two
/// tokens, so the component carries nothing the router needs.
#[derive(Clone)]
pub struct TesseraFallbackSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
}

impl SwapEncoder for TesseraFallbackSwapEncoder {
    /// # Errors
    ///
    /// Returns [`EncodingError::FatalError`] for a chain other than Base, the only chain Tessera
    /// is on.
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Base {
            return Err(EncodingError::FatalError(format!(
                "Tessera swaps are only supported on Base, not {chain}"
            )));
        }
        Ok(Self { executor_address, chain })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let fallback = FallbackSwap::from_user_data(swap.user_data().as_ref())?;
        if !fallback
            .protocol
            .supported_on(self.chain)
        {
            return Err(EncodingError::InvalidInput(format!(
                "Fallback protocol {} is not supported on {}: the chain's TesseraFallbackRouter \
                 would revert the swap",
                fallback.protocol.user_data_name(),
                self.chain
            )));
        }
        let token_in = bytes_to_address(&swap.token_in().address)?;
        let token_out = bytes_to_address(&swap.token_out().address)?;

        let mut data = (token_in, token_out).abi_encode_packed();
        data.extend(fallback.encode()?);
        Ok(data)
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
        evm::{utils::write_calldata_to_file, TESSERA_FALLBACK_PROTOCOL_SYSTEM},
        models::default_token,
    };

    const WETH: &str = "4200000000000000000000000000000000000006";
    const USDC: &str = "833589fcd6edb6e08f4c7c32d4f71b54bda02913";
    const USDC_WETH_USV3: &str = "d0b53d9277642d899df5c87a3966a349a798f224";

    fn encode_weth_usdc(user_data: Option<&str>) -> Result<String, EncodingError> {
        let weth = Bytes::from(format!("0x{WETH}").as_str());
        let usdc = Bytes::from(format!("0x{USDC}").as_str());
        let component = ProtocolComponent {
            id: String::from("tessera-weth-usdc"),
            protocol_system: String::from(TESSERA_FALLBACK_PROTOCOL_SYSTEM),
            tokens: vec![weth.clone(), usdc.clone()],
            ..Default::default()
        };
        let mut swap = Swap::new(
            component,
            default_token(weth.clone()),
            default_token(usdc.clone()),
            BigUint::ZERO,
        );
        if let Some(data) = user_data {
            swap = swap.with_user_data(Bytes::from(data.as_bytes()));
        }
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: weth,
            group_token_out: usdc,
            origin: None,
        };

        TesseraFallbackSwapEncoder::new(Bytes::default(), Chain::Base, None)?
            .encode_swap(&swap, &encoding_context)
            .map(|encoded| encode(&encoded))
    }

    #[test]
    fn test_encode_tessera_with_uniswap_v3_fallback() {
        let hex_swap = encode_weth_usdc(Some(&format!(
            r#"{{"fallback_protocol":"uniswap_v3","pool":"0x{USDC_WETH_USV3}"}}"#
        )))
        .unwrap();

        // The tokens, then the fallback: protocol byte 01 and the pool.
        assert_eq!(hex_swap, format!("{WETH}{USDC}01{USDC_WETH_USV3}"));
        // `TesseraFallbackRouterTest` runs this swap data on a Base fork.
        write_calldata_to_file("test_encode_tessera_fallback_for_solidity", &hex_swap);
    }

    #[test]
    fn test_rejects_a_swap_without_a_fallback() {
        let err = encode_weth_usdc(None).unwrap_err();

        assert!(
            matches!(&err, EncodingError::InvalidInput(msg) if msg.contains("user_data")),
            "{err:?}"
        );
    }

    /// Base's fallback router has no Fluid liquidity layer, so a Fluid fallback would revert.
    #[test]
    fn test_rejects_a_fallback_the_chain_does_not_run() {
        let dex = "4444444444444444444444444444444444444444";

        let err = encode_weth_usdc(Some(&format!(
            r#"{{"fallback_protocol":"fluid_v1","dex":"0x{dex}","zero2one":true}}"#
        )))
        .unwrap_err();

        assert!(
            matches!(&err, EncodingError::InvalidInput(msg)
                if msg.contains("fluid_v1") && msg.contains("base")),
            "{err:?}"
        );
    }

    #[test]
    fn test_rejects_a_chain_other_than_base() {
        let err = TesseraFallbackSwapEncoder::new(Bytes::default(), Chain::Ethereum, None)
            .err()
            .unwrap();

        assert!(matches!(err, EncodingError::FatalError(_)), "{err:?}");
    }
}
