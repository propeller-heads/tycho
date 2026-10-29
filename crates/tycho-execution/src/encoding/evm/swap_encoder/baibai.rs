use std::collections::HashMap;

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::{BAIBAI_BASE_ATTRIBUTE, BAIBAI_QUOTE_ATTRIBUTE},
        utils::get_static_attribute,
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a BaiBai swap as the base token (20 bytes) followed by the direction (one byte, 1 to
/// sell base for the quote token, 0 to buy it). The executor's entrypoint fixes the quote token.
#[derive(Clone)]
pub struct BaibaiSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for BaibaiSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Base {
            return Err(EncodingError::FatalError(
                "BaiBai swaps are only supported on Base".to_string(),
            ));
        }
        Ok(Self { executor_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let component = swap.component();
        let address = |name: &str| -> Result<Vec<u8>, EncodingError> {
            let value = get_static_attribute(swap, name).map_err(|e| {
                EncodingError::FatalError(format!("BaiBai component {}: {e}", component.id))
            })?;
            if value.len() != 20 {
                return Err(EncodingError::FatalError(format!(
                    "BaiBai component {}: attribute {name} holds {} bytes, not an address",
                    component.id,
                    value.len()
                )));
            }
            Ok(value)
        };
        let base = address(BAIBAI_BASE_ATTRIBUTE)?;
        let quote = address(BAIBAI_QUOTE_ATTRIBUTE)?;
        let token_in = swap.token_in().address.as_ref();
        let token_out = swap.token_out().address.as_ref();
        let sell_base = if token_in == base && token_out == quote {
            1
        } else if token_in == quote && token_out == base {
            0
        } else {
            return Err(EncodingError::InvalidInput(format!(
                "BaiBai component {} trades {} against {}, not {} for {}",
                component.id,
                Bytes::from(base),
                Bytes::from(quote),
                swap.token_in().address,
                swap.token_out().address
            )));
        };
        let mut data = base;
        data.push(sell_base);
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
    use num_bigint::BigUint;
    use rstest::rstest;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::models::default_token;

    const BASE: &str = "0x1111111111111111111111111111111111111111";
    const QUOTE: &str = "0x2222222222222222222222222222222222222222";
    const OTHER: &str = "0x3333333333333333333333333333333333333333";

    fn swap(attributes: &[(&str, &str)], token_in: &str, token_out: &str) -> Swap {
        let component = ProtocolComponent {
            id: "baibai-pair".to_string(),
            static_attributes: attributes
                .iter()
                .map(|(name, value)| (name.to_string(), Bytes::from(*value)))
                .collect(),
            ..Default::default()
        };
        Swap::new(
            component,
            default_token(Bytes::from(token_in)),
            default_token(Bytes::from(token_out)),
            BigUint::ZERO,
        )
    }

    fn encode(swap: &Swap) -> Result<Vec<u8>, EncodingError> {
        let context = EncodingContext {
            router_address: None,
            group_token_in: swap.token_in().address.clone(),
            group_token_out: swap.token_out().address.clone(),
            origin: None,
        };
        BaibaiSwapEncoder::new(Bytes::zero(20), Chain::Base, None)
            .unwrap()
            .encode_swap(swap, &context)
    }

    #[rstest]
    #[case::sell_base(BASE, QUOTE, "111111111111111111111111111111111111111101")]
    #[case::buy_base(QUOTE, BASE, "111111111111111111111111111111111111111100")]
    fn test_encode_baibai_swap(
        #[case] token_in: &str,
        #[case] token_out: &str,
        #[case] expected: &str,
    ) {
        let swap = swap(&[("base", BASE), ("quote", QUOTE)], token_in, token_out);

        assert_eq!(hex::encode(encode(&swap).unwrap()), expected);
    }

    #[rstest]
    #[case::token_outside_the_pair(&[("base", BASE), ("quote", QUOTE)], BASE, OTHER, "trades")]
    #[case::no_base_attribute(&[("quote", QUOTE)], QUOTE, BASE, "Attribute base not found")]
    #[case::no_quote_attribute(&[("base", BASE)], BASE, QUOTE, "Attribute quote not found")]
    #[case::short_base(&[("base", "0x1111"), ("quote", QUOTE)], BASE, QUOTE, "holds 2 bytes")]
    fn test_encode_baibai_swap_refuses(
        #[case] attributes: &[(&str, &str)],
        #[case] token_in: &str,
        #[case] token_out: &str,
        #[case] reason: &str,
    ) {
        let error = encode(&swap(attributes, token_in, token_out)).unwrap_err();

        assert!(error.to_string().contains(reason), "{error}");
        assert!(
            error
                .to_string()
                .contains("baibai-pair"),
            "{error}"
        );
    }

    #[test]
    fn test_baibai_encoder_refuses_other_chains() {
        assert!(BaibaiSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).is_err());
    }
}
