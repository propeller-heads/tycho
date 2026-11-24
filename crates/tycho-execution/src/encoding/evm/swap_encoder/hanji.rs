use std::collections::HashMap;

use alloy::sol_types::SolValue;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::HANJI_ORDERBOOK_ATTRIBUTE,
        utils::{bytes_to_address, convert_to_router_token, get_static_attribute},
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a Hanji swap as the market's proxy orderbook, the token in and the token out (20 bytes
/// each, the router's ETH address for native ETH). The executor checks the pair against the
/// orderbook's config and reads the scaling from it.
///
/// The component's tokens are the base then the quote, as the book feed publishes them; the pair
/// must be the component's, in either direction.
#[derive(Clone)]
pub struct HanjiSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for HanjiSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Base {
            return Err(EncodingError::FatalError(
                "Hanji swaps are only supported on Base".to_string(),
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
        let orderbook = get_static_attribute(swap, HANJI_ORDERBOOK_ATTRIBUTE).map_err(|e| {
            EncodingError::FatalError(format!("Hanji component {}: {e}", component.id))
        })?;
        if orderbook.len() != 20 {
            return Err(EncodingError::FatalError(format!(
                "Hanji component {}: attribute {HANJI_ORDERBOOK_ATTRIBUTE} holds {} bytes, not an \
                 address",
                component.id,
                orderbook.len()
            )));
        }
        let [base, quote] = component.tokens.as_slice() else {
            return Err(EncodingError::FatalError(format!(
                "Hanji component {} lists {} tokens, not a base and a quote",
                component.id,
                component.tokens.len()
            )));
        };
        let token_in = &swap.token_in().address;
        let token_out = &swap.token_out().address;
        let in_market =
            (token_in == base && token_out == quote) || (token_in == quote && token_out == base);
        if !in_market {
            return Err(EncodingError::InvalidInput(format!(
                "Hanji component {} trades {base} against {quote}, not {token_in} for {token_out}",
                component.id
            )));
        }
        let orderbook = bytes_to_address(&Bytes::from(orderbook))?;
        let token_in = convert_to_router_token(bytes_to_address(token_in)?);
        let token_out = convert_to_router_token(bytes_to_address(token_out)?);
        Ok((orderbook, token_in, token_out).abi_encode_packed())
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
    use crate::encoding::{evm::constants::HANJI_PROTOCOL_SYSTEM, models::default_token};

    const ORDERBOOK: &str = "0x6a6951db3bb99f76a8ccb9f8a8399b613666298b";
    const USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
    const OTHER: &str = "0x3333333333333333333333333333333333333333";

    /// Native ETH as the book feed publishes it.
    fn eth() -> String {
        Chain::Base
            .native_token()
            .address
            .to_string()
    }

    fn component(orderbook: Option<&str>, tokens: &[&str]) -> ProtocolComponent {
        ProtocolComponent {
            id: "hanji-market".to_string(),
            protocol_system: HANJI_PROTOCOL_SYSTEM.to_string(),
            tokens: tokens
                .iter()
                .map(|token| Bytes::from(*token))
                .collect(),
            static_attributes: orderbook
                .map(|orderbook| (HANJI_ORDERBOOK_ATTRIBUTE.to_string(), Bytes::from(orderbook)))
                .into_iter()
                .collect(),
            ..Default::default()
        }
    }

    fn encode_with(
        component: ProtocolComponent,
        token_in: &str,
        token_out: &str,
    ) -> Result<Vec<u8>, EncodingError> {
        let swap = Swap::new(
            component,
            default_token(Bytes::from(token_in)),
            default_token(Bytes::from(token_out)),
            BigUint::ZERO,
        );
        let encoder = HanjiSwapEncoder::new(
            Bytes::from("0x1234567890123456789012345678901234567890"),
            Chain::Base,
            None,
        )?;
        let context = EncodingContext {
            router_address: None,
            group_token_in: Bytes::from(token_in),
            group_token_out: Bytes::from(token_out),
            origin: None,
        };
        encoder.encode_swap(&swap, &context)
    }

    /// The orderbook, the token in and the token out, with native ETH as the router's ETH
    /// address: selling the native base for USDC, and buying it back.
    #[rstest]
    #[case::sell_native_base(
        true,
        "6a6951db3bb99f76a8ccb9f8a8399b613666298b\
         eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\
         833589fcd6edb6e08f4c7c32d4f71b54bda02913"
    )]
    #[case::buy_native_base(
        false,
        "6a6951db3bb99f76a8ccb9f8a8399b613666298b\
         833589fcd6edb6e08f4c7c32d4f71b54bda02913\
         eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
    )]
    fn encodes_the_orderbook_and_the_pair(#[case] sell_base: bool, #[case] expected: &str) {
        let eth = eth();
        let (token_in, token_out) =
            if sell_base { (eth.as_str(), USDC) } else { (USDC, eth.as_str()) };

        let encoded =
            encode_with(component(Some(ORDERBOOK), &[&eth, USDC]), token_in, token_out).unwrap();

        assert_eq!(encode(encoded), expected);
    }

    #[test]
    fn a_token_outside_the_market_is_refused() {
        let encoded = encode_with(component(Some(ORDERBOOK), &[&eth(), USDC]), OTHER, USDC);

        assert!(matches!(encoded, Err(EncodingError::InvalidInput(_))));
    }

    #[rstest]
    #[case::no_orderbook(None, &[USDC, OTHER])]
    #[case::short_orderbook(Some("0x6a6951db"), &[USDC, OTHER])]
    #[case::long_orderbook(Some("0x6a6951db3bb99f76a8ccb9f8a8399b613666298b00"), &[USDC, OTHER])]
    #[case::one_token(Some(ORDERBOOK), &[USDC])]
    fn a_malformed_component_is_refused(#[case] orderbook: Option<&str>, #[case] tokens: &[&str]) {
        let encoded = encode_with(component(orderbook, tokens), USDC, OTHER);

        assert!(matches!(encoded, Err(EncodingError::FatalError(_))));
    }

    #[test]
    fn other_chains_are_refused() {
        let encoder = HanjiSwapEncoder::new(Bytes::from("0x12"), Chain::Ethereum, None);

        assert!(matches!(encoder, Err(EncodingError::FatalError(_))));
    }
}
