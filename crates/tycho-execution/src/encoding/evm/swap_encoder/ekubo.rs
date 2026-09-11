use core::cmp::Eq;
use std::collections::HashMap;

use alloy::{primitives::Address, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{bytes_to_address, convert_to_router_token, get_static_attribute},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on an Ekubo pool through the given executor address.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EkuboSwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for EkuboSwapEncoder {
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
        encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let fee = u64::from_be_bytes(
            get_static_attribute(swap, "fee")?
                .try_into()
                .map_err(|_| EncodingError::FatalError("fee should be an u64".to_string()))?,
        );

        let tick_spacing = u32::from_be_bytes(
            get_static_attribute(swap, "tick_spacing")?
                .try_into()
                .map_err(|_| {
                    EncodingError::FatalError("tick_spacing should be an u32".to_string())
                })?,
        );

        let extension: Address = get_static_attribute(swap, "extension")?
            .as_slice()
            .try_into()
            .map_err(|_| EncodingError::FatalError("extension should be an address".to_string()))?;

        let mut encoded = vec![];

        if encoding_context.group_token_in == *swap.token_in().address {
            let token_in = convert_to_router_token(bytes_to_address(&swap.token_in().address)?);
            encoded.extend(token_in);
        }

        let token_out = convert_to_router_token(bytes_to_address(&swap.token_out().address)?);
        encoded.extend(token_out);
        encoded.extend((extension, fee, tick_spacing).abi_encode_packed());

        Ok(encoded)
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
        evm::{swap_encoder::ekubo::EkuboSwapEncoder, utils::write_calldata_to_file},
        models::default_token,
    };

    #[test]
    fn test_encode_swap_simple() {
        let token_in = Bytes::from(Address::ZERO.as_slice());
        let token_out = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC

        let static_attributes = HashMap::from([
            ("fee".to_string(), Bytes::from(0_u64)),
            ("tick_spacing".to_string(), Bytes::from(0_u32)),
            ("extension".to_string(), Bytes::from("0x51d02a5948496a67827242eabc5725531342527c")), /* Oracle */
        ]);

        let component = ProtocolComponent { static_attributes, ..Default::default() };

        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );

        let encoding_context = EncodingContext {
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
            router_address: Some(Bytes::default()),
        };

        let encoder = EkuboSwapEncoder::new(Bytes::default(), Chain::Ethereum, None).unwrap();

        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();

        let hex_swap = encode(&encoded_swap);

        assert_eq!(
            hex_swap,
            concat!(
                // group token in (ETH_ADDRESS)
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                // token out 1st swap
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                // pool config 1st swap
                "51d02a5948496a67827242eabc5725531342527c000000000000000000000000",
            ),
        );
    }

    #[test]
    fn test_encode_swap_multi() {
        let group_token_in = Bytes::from(Address::ZERO.as_slice());
        let group_token_out = Bytes::from("0xdAC17F958D2ee523a2206206994597C13D831ec7"); // USDT
        let intermediary_token = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC

        let encoder = EkuboSwapEncoder::new(Bytes::default(), Chain::Ethereum, None).unwrap();

        let encoding_context = EncodingContext {
            group_token_in: group_token_in.clone(),
            group_token_out: group_token_out.clone(),
            router_address: Some(Bytes::default()),
        };

        let first_swap = Swap::new(
            ProtocolComponent {
                static_attributes: HashMap::from([
                    ("fee".to_string(), Bytes::from(0_u64)),
                    ("tick_spacing".to_string(), Bytes::from(0_u32)),
                    (
                        "extension".to_string(),
                        Bytes::from("0x51d02a5948496a67827242eabc5725531342527c"),
                    ), // Oracle
                ]),
                ..Default::default()
            },
            default_token(group_token_in.clone()),
            default_token(intermediary_token.clone()),
            BigUint::ZERO,
        );

        let second_swap = Swap::new(
            ProtocolComponent {
                // 0.0025% fee & 0.005% base pool
                static_attributes: HashMap::from([
                    ("fee".to_string(), Bytes::from(461168601842738_u64)),
                    ("tick_spacing".to_string(), Bytes::from(50_u32)),
                    ("extension".to_string(), Bytes::zero(20)),
                ]),
                ..Default::default()
            },
            default_token(intermediary_token.clone()),
            default_token(group_token_out.clone()),
            BigUint::ZERO,
        );

        let first_encoded_swap = encoder
            .encode_swap(&first_swap, &encoding_context)
            .unwrap();

        let second_encoded_swap = encoder
            .encode_swap(&second_swap, &encoding_context)
            .unwrap();

        let combined_hex = format!("{}{}", encode(first_encoded_swap), encode(second_encoded_swap));

        assert_eq!(
            combined_hex,
            concat!(
                // group token in (ETH_ADDRESS)
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                // token out 1st swap
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                // pool config 1st swap
                "51d02a5948496a67827242eabc5725531342527c000000000000000000000000",
                // token out 2nd swap
                "dac17f958d2ee523a2206206994597c13d831ec7",
                // pool config 2nd swap
                "00000000000000000000000000000000000000000001a36e2eb1c43200000032",
            ),
        );
        write_calldata_to_file("test_ekubo_encode_swap_multi", combined_hex.as_str());
    }
}
