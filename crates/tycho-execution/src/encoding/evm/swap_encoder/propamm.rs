use std::collections::HashMap;

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Static attribute under which price-level-stream components carry their pAMM venue address.
const PAMM_ADDRESS_ATTRIBUTE: &str = "pamm_address";

/// Encodes a swap on any pAMM implementing the standard `IPropAMM` interface pushed by Titan.
///
/// A single generic executor serves all such venues, so the pAMM address travels in the swap data
/// (packed `pamm ++ token_in ++ token_out`), taken from the component's `pamm_address` static
/// attribute, which every price-level-stream component carries.
#[derive(Clone)]
pub struct PropAMMSwapEncoder {
    executor_address: Bytes,
}

impl PropAMMSwapEncoder {
    fn pamm_address(swap: &Swap) -> Result<Bytes, EncodingError> {
        let component = swap.component();
        component
            .static_attributes
            .get(PAMM_ADDRESS_ATTRIBUTE)
            .cloned()
            .ok_or_else(|| {
                EncodingError::FatalError(format!(
                    "Price level stream component {} is missing the {PAMM_ADDRESS_ATTRIBUTE} \
                     static attribute",
                    component.id
                ))
            })
    }
}

impl SwapEncoder for PropAMMSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Ethereum {
            return Err(EncodingError::FatalError(
                "Price level stream swaps are only supported on Ethereum".to_string(),
            ));
        }

        Ok(Self { executor_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let pamm = bytes_to_address(&Self::pamm_address(swap)?)?;
        let token_in = bytes_to_address(&swap.token_in().address)?;
        let token_out = bytes_to_address(&swap.token_out().address)?;

        let args = (pamm, token_in, token_out);
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

    // Must match what the Solidity integration test decodes the generated calldata against
    // (`contracts/test/protocols/PropAMM.t.sol`: `MOCK_PAMM`, `WETH_ADDR`, `USDC_ADDR`).
    const PAMM: &str = "1111111111111111111111111111111111111111";
    const WETH: &str = "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const USDC: &str = "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

    fn weth_usdc_component() -> ProtocolComponent {
        ProtocolComponent {
            // The id the price level stream produces: pamm ++ token0 ++ token1.
            id: format!("0x{PAMM}{WETH}{USDC}"),
            protocol_system: String::from("pricelevelstream:kipseli"),
            static_attributes: HashMap::from([(
                PAMM_ADDRESS_ATTRIBUTE.to_string(),
                Bytes::from(format!("0x{PAMM}").as_str()),
            )]),
            ..Default::default()
        }
    }

    fn encoder() -> PropAMMSwapEncoder {
        PropAMMSwapEncoder::new(Bytes::default(), Chain::Ethereum, None).unwrap()
    }

    fn encode_weth_usdc(component: ProtocolComponent) -> String {
        let token_in = Bytes::from(format!("0x{WETH}").as_str());
        let token_out = Bytes::from(format!("0x{USDC}").as_str());
        let swap = Swap::new(
            component,
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
        encode(&encoded_swap)
    }

    #[test]
    fn test_encode_propamm_weth_usdc() {
        let hex_swap = encode_weth_usdc(weth_usdc_component());

        assert_eq!(hex_swap, format!("{PAMM}{WETH}{USDC}"));
        write_calldata_to_file("test_encode_propamm_weth_usdc", hex_swap.as_str());
    }

    #[test]
    fn test_rejects_component_without_pamm_address() {
        let mut component = weth_usdc_component();
        component.static_attributes.clear();
        let swap = Swap::new(
            component,
            default_token(Bytes::from(format!("0x{WETH}").as_str())),
            default_token(Bytes::from(format!("0x{USDC}").as_str())),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: Bytes::from(format!("0x{WETH}").as_str()),
            group_token_out: Bytes::from(format!("0x{USDC}").as_str()),
        };

        let result = encoder().encode_swap(&swap, &encoding_context);
        assert!(result.is_err());
    }

    #[test]
    fn test_encoder_rejects_non_ethereum_chain() {
        let result = PropAMMSwapEncoder::new(Bytes::zero(20), Chain::Base, None);
        assert!(result.is_err());
    }
}
