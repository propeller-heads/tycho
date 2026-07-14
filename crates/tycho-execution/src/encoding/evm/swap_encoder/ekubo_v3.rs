use std::collections::HashMap;

use alloy::{
    primitives::{aliases::B32, Address},
    sol_types::SolValue as _,
};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{bytes_to_address, convert_to_router_token, get_static_attribute},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on an Ekubo V3 pool through the given executor address.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EkuboV3SwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for EkuboV3SwapEncoder {
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

        let pool_type_config = B32::try_from(&get_static_attribute(swap, "pool_type_config")?[..])
            .map_err(|_| {
                EncodingError::FatalError("pool_type_config should be 4 bytes long".to_string())
            })?;

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
        encoded.extend((extension, fee, pool_type_config).abi_encode_packed());

        // A signed (SignedExclusiveSwap, forward-only) hop carries a self-describing tail so the
        // executor's length-aware walk can skip past it to any following hop. When `user_data` is
        // absent the hop is byte-identical to a normal hop, preserving existing behavior.
        if let Some(user_data) = swap.user_data() {
            // user_data layout: meta(32) | minBalanceUpdate(32) | signature(N)
            // Wire format:      meta(32) | minBalanceUpdate(32) | sigLen(u16 be) | signature(N)
            const SIGNED_FIXED_PREFIX: usize = 64;
            if user_data.len() < SIGNED_FIXED_PREFIX {
                return Err(EncodingError::FatalError(
                    "signed swap user_data must be at least 64 bytes (meta + minBalanceUpdate)"
                        .to_string(),
                ));
            }
            let signature = &user_data[SIGNED_FIXED_PREFIX..];
            let sig_len = u16::try_from(signature.len()).map_err(|_| {
                EncodingError::FatalError("signature length exceeds u16::MAX".to_string())
            })?;

            encoded.extend_from_slice(&user_data[..SIGNED_FIXED_PREFIX]);
            encoded.extend_from_slice(&sig_len.to_be_bytes());
            encoded.extend_from_slice(signature);
        }

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
    use std::str::FromStr as _;

    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::utils::write_calldata_to_file, models::default_token};

    #[test]
    fn test_encode_swap_simple() {
        let token_in = Bytes::from(Address::ZERO.as_slice());
        let token_out = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC

        let static_attributes = HashMap::from([
            ("extension".to_string(), Bytes::from("0x517e506700271aea091b02f42756f5e174af5230")), /* Oracle */
            ("fee".to_string(), Bytes::from(0_u64)),
            ("pool_type_config".to_string(), Bytes::from(0_u32)),
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

        let encoder = EkuboV3SwapEncoder::new(Bytes::default(), Chain::Ethereum, None).unwrap();

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
                "517e506700271aea091b02f42756f5e174af5230000000000000000000000000",
            ),
        );
    }

    #[test]
    fn test_encode_signed_swap() {
        let token_in = Bytes::from(Address::ZERO.as_slice());
        let token_out = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC

        let static_attributes = HashMap::from([
            // SignedExclusiveSwap extension placeholder used by signed pools.
            ("extension".to_string(), Bytes::from("0x5519ed5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e")),
            ("fee".to_string(), Bytes::from(0_u64)),
            ("pool_type_config".to_string(), Bytes::from(0_u32)),
        ]);

        let component = ProtocolComponent { static_attributes, ..Default::default() };

        // `user_data` is the packed concatenation meta(32) | minBalanceUpdate(32) | signature(N).
        // The encoder splits it and inserts the 2-byte big-endian signature length.
        let meta = "1111111111111111111111111111111111111111111111111111111111111111";
        let min_balance_update = "2222222222222222222222222222222222222222222222222222222222222222";
        let signature = "abcdef0123456789"; // 8-byte signature
        let user_data =
            Bytes::from_str(&format!("0x{meta}{min_balance_update}{signature}")).unwrap();

        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_user_data(user_data);

        let encoding_context = EncodingContext {
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
            router_address: Some(Bytes::default()),
        };

        let encoder = EkuboV3SwapEncoder::new(Bytes::default(), Chain::Ethereum, None).unwrap();

        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();

        let hex_swap = encode(&encoded_swap);

        assert_eq!(
            hex_swap,
            concat!(
                // group token in (ETH_ADDRESS)
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                // token out
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                // pool config (extension = SIGNED_EXCLUSIVE_SWAP placeholder)
                "5519ed5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e000000000000000000000000",
                // meta(32)
                "1111111111111111111111111111111111111111111111111111111111111111",
                // minBalanceUpdate(32)
                "2222222222222222222222222222222222222222222222222222222222222222",
                // sigLen(2, u16 be) = 8
                "0008",
                // signature(8)
                "abcdef0123456789",
            ),
        );
    }

    #[test]
    fn test_encode_swap_multi() {
        let group_token_in = Bytes::from(Address::ZERO.as_slice());
        let group_token_out = Bytes::from("0xdAC17F958D2ee523a2206206994597C13D831ec7"); // USDT
        let intermediary_token = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"); // USDC

        let encoder = EkuboV3SwapEncoder::new(Bytes::default(), Chain::Ethereum, None).unwrap();

        let encoding_context = EncodingContext {
            group_token_in: group_token_in.clone(),
            group_token_out: group_token_out.clone(),
            router_address: Some(Bytes::default()),
        };

        let first_swap = Swap::new(
            ProtocolComponent {
                static_attributes: HashMap::from([
                    (
                        "extension".to_string(),
                        Bytes::from("517e506700271aea091b02f42756f5e174af5230"),
                    ), // Oracle
                    ("fee".to_string(), Bytes::from(0_u64)),
                    ("pool_type_config".to_string(), Bytes::zero(4)),
                ]),
                ..Default::default()
            },
            default_token(group_token_in.clone()),
            default_token(intermediary_token.clone()),
            BigUint::ZERO,
        );

        let second_swap = Swap::new(
            ProtocolComponent {
                static_attributes: HashMap::from([
                    ("extension".to_string(), Bytes::zero(20)),
                    ("fee".to_string(), Bytes::from(184467440737096_u64)),
                    ("pool_type_config".to_string(), Bytes::from_str("0x80000032").unwrap()), /* tick spacing = 50 */
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
                "517e506700271aea091b02f42756f5e174af5230000000000000000000000000",
                // token out 2nd swap
                "dac17f958d2ee523a2206206994597c13d831ec7",
                // pool config 2nd swap
                "00000000000000000000000000000000000000000000a7c5ac471b4880000032",
            ),
        );
        write_calldata_to_file("test_ekubo_v3_encode_swap_multi", combined_hex.as_str());
    }
}
