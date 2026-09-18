use std::collections::HashMap;

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// The executor fixes the entrypoint and quote token; each swap selects a base and direction.
#[derive(Clone)]
pub struct BaibaiSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for BaibaiSwapEncoder {
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
        _context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let attr = |name: &str| {
            swap.component()
                .static_attributes
                .get(name)
                .ok_or_else(|| {
                    EncodingError::FatalError(format!("Missing BaiBai {name} attribute"))
                })
        };
        let base = attr("base")?;
        let quote = attr("quote")?;
        let sell_base = swap.token_in().address == *base && swap.token_out().address == *quote;
        let buy_base = swap.token_in().address == *quote && swap.token_out().address == *base;
        if base.len() != 20 ||
            quote.len() != 20 ||
            base == quote ||
            base == &Bytes::zero(20) ||
            !(sell_base || buy_base)
        {
            return Err(EncodingError::FatalError("Invalid BaiBai token pair".into()));
        }
        let mut data = base.to_vec();
        data.push(u8::from(sell_base));
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
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::models::default_token;

    #[test]
    fn encodes_both_directions_and_rejects_unrelated_tokens() {
        let base = Bytes::from([1; 20]);
        let quote = Bytes::from([2; 20]);
        let component = ProtocolComponent {
            static_attributes: HashMap::from([
                ("base".into(), base.clone()),
                ("quote".into(), quote.clone()),
            ]),
            ..Default::default()
        };
        let encoder = BaibaiSwapEncoder::new(Bytes::zero(20), Chain::Base, None).unwrap();
        for (input, output, expected) in [
            (base.clone(), quote.clone(), Some(1)),
            (quote.clone(), base.clone(), Some(0)),
            (base.clone(), Bytes::zero(20), None),
        ] {
            let context = EncodingContext {
                router_address: None,
                group_token_in: input.clone(),
                group_token_out: output.clone(),
            };
            let swap = Swap::new(
                component.clone(),
                default_token(input),
                default_token(output),
                BigUint::ZERO,
            );
            let encoded = encoder.encode_swap(&swap, &context);
            if let Some(direction) = expected {
                let mut expected = base.to_vec();
                expected.push(direction);
                assert_eq!(encoded.unwrap(), expected);
            } else {
                assert!(encoded.is_err());
            }
        }
    }
}
