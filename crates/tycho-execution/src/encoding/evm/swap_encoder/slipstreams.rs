use std::{collections::HashMap, str::FromStr};

use alloy::{primitives::Address, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{bytes_to_address, get_static_attribute, pad_or_truncate_to_size},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on a Aerodrome Slipstreams pool through the given executor address.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct SlipstreamsSwapEncoder {
    executor_address: Bytes,
}

impl SlipstreamsSwapEncoder {
    fn get_zero_to_one(sell_token_address: Address, buy_token_address: Address) -> bool {
        sell_token_address < buy_token_address
    }
}

impl SwapEncoder for SlipstreamsSwapEncoder {
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
        let token_in_address = bytes_to_address(&swap.token_in().address)?;
        let token_out_address = bytes_to_address(&swap.token_out().address)?;

        let zero_to_one = Self::get_zero_to_one(token_in_address, token_out_address);
        let component_id = Address::from_str(&swap.component().id).map_err(|_| {
            EncodingError::FatalError("Invalid Slipstreams component id".to_string())
        })?;
        let tick_spacing_bytes = get_static_attribute(swap, "tick_spacing")?;

        let tick_spacing_bytes_u24 =
            pad_or_truncate_to_size::<3>(&tick_spacing_bytes).map_err(|_| {
                EncodingError::FatalError("Failed to extract tick_spacing bytes".to_string())
            })?;

        let args = (
            token_in_address,
            token_out_address,
            tick_spacing_bytes_u24,
            component_id,
            zero_to_one,
        );

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
    use num_bigint::{BigInt, BigUint};
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::models::{default_token, Swap};

    /// Covers the encoder shared by the slipstream forks and Ramses V3: it packs the immutable
    /// `tick_spacing` static attribute into the 3-byte slot, which the Uniswap V3 executor ignores.
    #[test]
    fn test_encode_slipstreams() {
        let tick_spacing = BigInt::from(50);

        let pool = ProtocolComponent {
            id: String::from("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            static_attributes: [(
                "tick_spacing".into(),
                Bytes::from(tick_spacing.to_signed_bytes_be()),
            )]
            .into(),
            ..Default::default()
        };
        let token_in = Bytes::from("0x1111111111111111111111111111111111111111");
        let token_out = Bytes::from("0x2222222222222222222222222222222222222222");
        let swap = Swap::new(
            pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = SlipstreamsSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            None,
        )
        .unwrap();
        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);
        assert_eq!(
            hex_swap,
            String::from(concat!(
                // in token
                "1111111111111111111111111111111111111111",
                // out token
                "2222222222222222222222222222222222222222",
                // tick spacing
                "000032",
                // pool id
                "88e6a0c2ddd26feeb64f039a2c41296fcb3f5640",
                // zero for one
                "01",
            ))
        );
    }
}
