use std::collections::HashMap;

use alloy::{primitives::Address, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Static attribute holding the venue contract the swap is executed on.
const PAMM_ADDRESS_ATTRIBUTE: &str = "pamm_address";

/// Encodes swaps on the Biconomy PropAMM venue, a push-payment `IPropAMM` contract serving many
/// pairs from one address. The router transfers the input to the venue, then the
/// `BiconomyExecutor` calls `swap`. The venue handles ERC20 tokens only.
#[derive(Clone)]
pub struct BiconomySwapEncoder {
    executor_address: Bytes,
}

impl BiconomySwapEncoder {
    fn venue_address(swap: &Swap) -> Result<Address, EncodingError> {
        let component = swap.component();
        let venue = component
            .static_attributes
            .get(PAMM_ADDRESS_ATTRIBUTE)
            .ok_or_else(|| {
                EncodingError::FatalError(format!(
                    "Biconomy component {} is missing the {PAMM_ADDRESS_ATTRIBUTE} static attribute",
                    component.id
                ))
            })?;
        bytes_to_address(venue)
    }

    fn erc20(token: &Bytes) -> Result<Address, EncodingError> {
        let address = bytes_to_address(token)?;
        if address == Address::ZERO {
            return Err(EncodingError::FatalError(
                "Biconomy venue does not support native ETH, wrap it first".to_owned(),
            ));
        }
        Ok(address)
    }
}

impl SwapEncoder for BiconomySwapEncoder {
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
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let venue = Self::venue_address(swap)?;
        let token_in = Self::erc20(&swap.token_in().address)?;
        let token_out = Self::erc20(&swap.token_out().address)?;

        Ok((venue, token_in, token_out).abi_encode_packed())
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
    use crate::encoding::models::default_token;

    const VENUE: &str = "0x000000da21a0f02b2626874870b6447db220c1ef";
    const WETH: &str = "0x4200000000000000000000000000000000000006";
    const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";

    fn swap(static_attributes: HashMap<String, Bytes>, token_in: &str, token_out: &str) -> Swap {
        let component = ProtocolComponent {
            id: VENUE.to_owned(),
            protocol_system: "biconomy".to_owned(),
            static_attributes,
            ..Default::default()
        };
        Swap::new(
            component,
            default_token(Bytes::from(token_in)),
            default_token(Bytes::from(token_out)),
            BigUint::ZERO,
        )
    }

    fn venue_attribute() -> HashMap<String, Bytes> {
        HashMap::from([(PAMM_ADDRESS_ATTRIBUTE.to_owned(), Bytes::from(VENUE))])
    }

    fn context() -> EncodingContext {
        EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: Bytes::from(WETH),
            group_token_out: Bytes::from(USDC),
        }
    }

    fn encoder() -> BiconomySwapEncoder {
        BiconomySwapEncoder::new(Bytes::zero(20), Chain::Base, None).unwrap()
    }

    #[test]
    fn encodes_venue_token_in_token_out_packed() {
        let encoded = encoder()
            .encode_swap(&swap(venue_attribute(), WETH, USDC), &context())
            .unwrap();

        assert_eq!(
            encode(encoded),
            concat!(
                "000000da21a0f02b2626874870b6447db220c1ef",
                "4200000000000000000000000000000000000006",
                "833589fcd6edb6e08f4c7c32d4f71b54bda02913",
            )
        );
    }

    #[test]
    fn fails_without_venue_attribute() {
        let result = encoder().encode_swap(&swap(HashMap::new(), WETH, USDC), &context());

        assert!(
            matches!(result, Err(EncodingError::FatalError(msg)) if msg.contains(PAMM_ADDRESS_ATTRIBUTE))
        );
    }

    #[test]
    fn fails_on_native_token() {
        let native = "0x0000000000000000000000000000000000000000";

        assert!(encoder()
            .encode_swap(&swap(venue_attribute(), native, USDC), &context())
            .is_err());
        assert!(encoder()
            .encode_swap(&swap(venue_attribute(), USDC, native), &context())
            .is_err());
    }
}
