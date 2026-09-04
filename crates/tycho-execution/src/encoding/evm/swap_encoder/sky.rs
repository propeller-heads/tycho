use std::collections::HashMap;

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap through a Sky mint/redeem venue (LitePSM, UsdsPsmWrapper or the
/// DaiUsds converter) for the SkyExecutor.
///
/// The executor holds the three venue addresses as immutables, so the calldata is
/// just 2 bytes: target (1) ++ is_gem_to_stable (1). The target is selected by the
/// component's `component_type` static attribute and the direction by whether the
/// input token is the `gem` static attribute (USDC on the PSM legs, USDS on the
/// converter) — the same role assignment tycho-simulation's SkyState uses.
#[derive(Clone)]
pub struct SkySwapEncoder {
    executor_address: Bytes,
}

#[repr(u8)]
enum SkyTarget {
    Psm = 0,
    Wrapper = 1,
    Converter = 2,
}

impl SwapEncoder for SkySwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Ethereum {
            return Err(EncodingError::FatalError(
                "Sky swaps are only supported on Ethereum".to_string(),
            ));
        }
        Ok(Self { executor_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let component = swap.component();
        let get_attribute = |name: &str| -> Result<&Bytes, EncodingError> {
            component
                .static_attributes
                .get(name)
                .ok_or_else(|| {
                    EncodingError::FatalError(format!(
                        "Missing static attribute {name} on sky component {}",
                        component.id
                    ))
                })
        };

        let target = match get_attribute("component_type")?.as_ref() {
            b"psm" => SkyTarget::Psm,
            b"psm_wrapper" => SkyTarget::Wrapper,
            b"converter" => SkyTarget::Converter,
            other => {
                return Err(EncodingError::FatalError(format!(
                    "Unknown sky component_type: {}",
                    String::from_utf8_lossy(other)
                )))
            }
        };
        let gem_to_stable = swap.token_in().address == *get_attribute("gem")?;

        Ok(vec![target as u8, u8::from(gem_to_stable)])
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
    use rstest::rstest;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::utils::write_calldata_to_file, models::default_token};

    const PSM: &str = "0xf6e72db5454dd049d0788e411b06cfaf16853042";
    const WRAPPER: &str = "0xa188eec8f81263234da3622a406892f3d630f98c";
    const CONVERTER: &str = "0x3225737a9bbb6473cb4a45b7244aca2befdb276a";
    const DAI: &str = "0x6b175474e89094c44da98b954eedeac495271d0f";
    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const USDS: &str = "0xdc035d45d973e3ec169d2276ddab16f1e407384f";

    fn component(id: &str, component_type: &str, gem: &str) -> ProtocolComponent {
        ProtocolComponent {
            id: id.to_string(),
            protocol_system: "sky".to_string(),
            static_attributes: HashMap::from([
                ("component_type".to_string(), Bytes::from(component_type.as_bytes().to_vec())),
                ("gem".to_string(), Bytes::from(gem)),
            ]),
            ..Default::default()
        }
    }

    fn encoder() -> SkySwapEncoder {
        SkySwapEncoder::new(
            Bytes::from("0x5c2f5a71f67c01775180adc06909288b4c329308"),
            Chain::Ethereum,
            None,
        )
        .unwrap()
    }

    fn encode_test_swap(
        component_id: &str,
        component_type: &str,
        gem: &str,
        token_in: &str,
        token_out: &str,
    ) -> Vec<u8> {
        let swap = Swap::new(
            component(component_id, component_type, gem),
            default_token(Bytes::from(token_in)),
            default_token(Bytes::from(token_out)),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: Bytes::from(token_in),
            group_token_out: Bytes::from(token_out),
        };
        encoder()
            .encode_swap(&swap, &encoding_context)
            .unwrap()
    }

    #[rstest]
    #[case::psm_sell_gem(PSM, "psm", USDC, USDC, DAI, [0u8, 1u8], "sky_psm_sell_gem")]
    #[case::psm_buy_gem(PSM, "psm", USDC, DAI, USDC, [0u8, 0u8], "sky_psm_buy_gem")]
    #[case::wrapper_sell_gem(
        WRAPPER,
        "psm_wrapper",
        USDC,
        USDC,
        USDS,
        [1u8, 1u8],
        "sky_wrapper_sell_gem"
    )]
    #[case::wrapper_buy_gem(
        WRAPPER,
        "psm_wrapper",
        USDC,
        USDS,
        USDC,
        [1u8, 0u8],
        "sky_wrapper_buy_gem"
    )]
    #[case::dai_to_usds(CONVERTER, "converter", USDS, DAI, USDS, [2u8, 0u8], "sky_dai_to_usds")]
    #[case::usds_to_dai(CONVERTER, "converter", USDS, USDS, DAI, [2u8, 1u8], "sky_usds_to_dai")]
    fn test_encode_sky_swap(
        #[case] component_id: &str,
        #[case] component_type: &str,
        #[case] gem: &str,
        #[case] token_in: &str,
        #[case] token_out: &str,
        #[case] expected: [u8; 2],
        #[case] test_name: &str,
    ) {
        let encoded = encode_test_swap(component_id, component_type, gem, token_in, token_out);
        assert_eq!(encoded, expected);
        write_calldata_to_file(test_name, encode(&encoded).as_str());
    }

    #[test]
    fn test_chain_restriction() {
        let result = SkySwapEncoder::new(
            Bytes::from("0x5c2f5a71f67c01775180adc06909288b4c329308"),
            Chain::Base,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_gem_attribute_errors() {
        let mut comp = component(PSM, "psm", USDC);
        comp.static_attributes.remove("gem");
        let swap = Swap::new(
            comp,
            default_token(Bytes::from(USDC)),
            default_token(Bytes::from(DAI)),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: Bytes::from(USDC),
            group_token_out: Bytes::from(DAI),
        };
        assert!(encoder()
            .encode_swap(&swap, &encoding_context)
            .is_err());
    }
}
