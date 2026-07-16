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

const SIGNED_USER_DATA_MIN_LEN: usize = 8 + 32 + 32; // fee + meta + minBalanceUpdate

#[derive(Debug)]
struct SignedSwapTail {
    fee: u64,
    meta: [u8; 32],
    min_balance_update: [u8; 32],
    signature: Vec<u8>,
}

/// Parses `user_data` into a signed swap tail when present.
///
/// Expected byte layout: `fee(8) | meta(32) | minBalanceUpdate(32) | signature(N)`.
fn parse_signed_user_data(
    user_data: &Option<Bytes>,
) -> Result<Option<SignedSwapTail>, EncodingError> {
    let Some(data) = user_data.as_ref() else {
        return Ok(None);
    };
    if data.len() <= SIGNED_USER_DATA_MIN_LEN {
        return Err(EncodingError::InvalidInput(format!(
            "signed user_data too short: {} bytes, need more than \
             {SIGNED_USER_DATA_MIN_LEN} (signature must be non-empty)",
            data.len()
        )));
    }
    let mut fee_bytes = [0u8; 8];
    fee_bytes.copy_from_slice(&data[..8]);
    let fee = u64::from_be_bytes(fee_bytes);
    let mut meta = [0u8; 32];
    meta.copy_from_slice(&data[8..40]);
    let mut min_balance_update = [0u8; 32];
    min_balance_update.copy_from_slice(&data[40..72]);
    let signature = data[72..].to_vec();
    Ok(Some(SignedSwapTail { fee, meta, min_balance_update, signature }))
}

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
        let signed_tail = parse_signed_user_data(swap.user_data())?;

        let fee = if let Some(ref tail) = signed_tail {
            tail.fee
        } else {
            u64::from_be_bytes(
                get_static_attribute(swap, "fee")?
                    .try_into()
                    .map_err(|_| EncodingError::FatalError("fee should be an u64".to_string()))?,
            )
        };

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
        if let Some(tail) = signed_tail {
            // Wire format: meta(32) | minBalanceUpdate(32) | sigLen(u16 be) | signature(N)
            let sig_len = u16::try_from(tail.signature.len()).map_err(|_| {
                EncodingError::FatalError("signature length exceeds u16::MAX".to_string())
            })?;

            encoded.extend_from_slice(&tail.meta);
            encoded.extend_from_slice(&tail.min_balance_update);
            encoded.extend_from_slice(&sig_len.to_be_bytes());
            encoded.extend_from_slice(&tail.signature);
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

    use alloy::{hex::encode, primitives::keccak256};
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::utils::write_calldata_to_file, models::default_token};

    #[test]
    fn test_parse_signed_user_data_too_short() {
        let data = Some(Bytes::from(vec![0u8; 71]));
        let err = parse_signed_user_data(&data).unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(_)));
    }

    #[test]
    fn test_parse_signed_user_data_empty_signature() {
        let mut buf = vec![0u8; 72];
        buf[..8].copy_from_slice(&42u64.to_be_bytes());
        buf[8..40].fill(0x11);
        buf[40..72].fill(0x22);

        let err = parse_signed_user_data(&Some(buf.into())).unwrap_err();
        assert!(matches!(err, EncodingError::InvalidInput(_)));
    }

    #[test]
    fn test_parse_signed_user_data_with_signature() {
        let sig = vec![0xAB, 0xCD, 0xEF];
        let mut buf = vec![0u8; 72 + sig.len()];
        buf[..8].copy_from_slice(&100u64.to_be_bytes());
        buf[8..40].fill(0xAA);
        buf[40..72].fill(0xBB);
        buf[72..].copy_from_slice(&sig);

        let tail = parse_signed_user_data(&Some(buf.into()))
            .unwrap()
            .expect("should parse");
        assert_eq!(tail.fee, 100);
        assert_eq!(tail.meta, [0xAA; 32]);
        assert_eq!(tail.min_balance_update, [0xBB; 32]);
        assert_eq!(tail.signature, sig);
    }

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

        // `user_data` layout: fee(8) | meta(32) | minBalanceUpdate(32) | signature(N).
        // The encoder extracts the fee for the pool config and inserts the 2-byte
        // big-endian signature length before the signature on the wire.
        let fee_hex = "00000000deadbeef"; // target fee override
        let meta = "1111111111111111111111111111111111111111111111111111111111111111";
        let min_balance_update = "2222222222222222222222222222222222222222222222222222222222222222";
        let signature = "abcdef0123456789"; // 8-byte signature
        let user_data =
            Bytes::from_str(&format!("0x{fee_hex}{meta}{min_balance_update}{signature}")).unwrap();

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
                // pool config: extension(20) | fee(8, from user_data) | pool_type_config(4)
                "5519ed5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e00000000deadbeef00000000",
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

    /// Builds a real EIP-712 signed swap and writes the encoded calldata to file
    /// for the corresponding Solidity integration test in `EkuboV3.t.sol`.
    ///
    /// Uses the same controller key (`0xBEEF`) and pool parameters as the
    /// Solidity `testSignedSwapIntegration` test, so the on-chain extension
    /// validates the signature.
    #[test]
    fn test_encode_signed_swap_integration() {
        use alloy::signers::{local::PrivateKeySigner, SignerSync as _};

        let usdc = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let usdt = Bytes::from("0xdAC17F958D2ee523a2206206994597C13D831ec7");

        // Must match SIGNED_EXCLUSIVE_SWAP_ADDRESS in EkuboV3Executor.sol.
        let extension_hex = "0x5519ed5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e";
        let extension_addr =
            Address::from_str("0x5519ed5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e").unwrap();

        // Concentrated pool, tick_spacing = 100 (0x80000064).
        let pool_type_config = 0x8000_0064_u32;

        let static_attributes = HashMap::from([
            ("extension".to_string(), Bytes::from(extension_hex)),
            ("fee".to_string(), Bytes::from(0_u64)),
            ("pool_type_config".to_string(), Bytes::from_str("0x80000064").unwrap()),
        ]);
        let component = ProtocolComponent { static_attributes, ..Default::default() };

        // --- EIP-712 signing with controller key 0xBEEF ---
        let signer = PrivateKeySigner::from_bytes(&alloy::primitives::FixedBytes::from(
            alloy::primitives::U256::from(0xBEEF_u64).to_be_bytes::<32>(),
        ))
        .unwrap();

        // PoolConfig = extension(20) | fee(8) | pool_type_config(4)
        let mut pool_config = [0u8; 32];
        pool_config[0..20].copy_from_slice(extension_addr.as_slice());
        // fee bytes [20..28] remain zero
        pool_config[28..32].copy_from_slice(&pool_type_config.to_be_bytes());

        // poolId = keccak256(abi.encode(token0, token1, config))
        // USDC < USDT, so token0 = USDC, token1 = USDT.
        let usdc_addr = Address::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();
        let usdt_addr = Address::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap();

        let mut pool_key_encoded = [0u8; 96];
        pool_key_encoded[12..32].copy_from_slice(usdc_addr.as_slice());
        pool_key_encoded[44..64].copy_from_slice(usdt_addr.as_slice());
        pool_key_encoded[64..96].copy_from_slice(&pool_config);
        let pool_id = keccak256(pool_key_encoded);

        // EIP-712 domain separator for SignedExclusiveSwap.
        let domain_typehash = keccak256(
            "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
        );
        let name_hash = keccak256("Ekubo SignedExclusiveSwap");
        let version_hash = keccak256("1");

        let mut domain_data = [0u8; 160];
        domain_data[0..32].copy_from_slice(domain_typehash.as_slice());
        domain_data[32..64].copy_from_slice(name_hash.as_slice());
        domain_data[64..96].copy_from_slice(version_hash.as_slice());
        // chainId = 1 (Ethereum mainnet)
        domain_data[127] = 1;
        // verifyingContract = extension address, left-padded to 32 bytes
        domain_data[140..160].copy_from_slice(extension_addr.as_slice());
        let domain_separator = keccak256(domain_data);

        // SignedSwapMeta: deadline | fee | nonce | authorizedLocker
        // deadline = 1_752_000_000 (0x686B7B00); the Solidity test
        // vm.warps to 1 hour before this value.
        let deadline = 1_752_000_000_u32;
        let mut meta = [0u8; 32];
        meta[0..4].copy_from_slice(&deadline.to_be_bytes());

        // minBalanceUpdate: accept any output (both deltas at int128 min).
        let mut min_balance_update = [0u8; 32];
        min_balance_update[0] = 0x80;
        min_balance_update[16] = 0x80;

        // structHash = keccak256(abi.encode(typehash, poolId, meta, minBU))
        let signed_swap_typehash =
            keccak256("SignedSwap(bytes32 poolId,uint256 meta,bytes32 minBalanceUpdate)");
        let mut struct_data = [0u8; 128];
        struct_data[0..32].copy_from_slice(signed_swap_typehash.as_slice());
        struct_data[32..64].copy_from_slice(pool_id.as_slice());
        struct_data[64..96].copy_from_slice(&meta);
        struct_data[96..128].copy_from_slice(&min_balance_update);
        let struct_hash = keccak256(struct_data);

        // digest = keccak256("\x19\x01" || domainSeparator || structHash)
        let mut digest_input = [0u8; 66];
        digest_input[0] = 0x19;
        digest_input[1] = 0x01;
        digest_input[2..34].copy_from_slice(domain_separator.as_slice());
        digest_input[34..66].copy_from_slice(struct_hash.as_slice());
        let digest = keccak256(digest_input);

        let sig = signer
            .sign_hash_sync(&digest)
            .expect("signing should succeed");

        // Encode signature as r(32) | s(32) | v(1) with Ethereum recovery id (27/28).
        let mut sig_bytes = Vec::with_capacity(65);
        sig_bytes.extend_from_slice(&sig.r().to_be_bytes::<32>());
        sig_bytes.extend_from_slice(&sig.s().to_be_bytes::<32>());
        sig_bytes.push(if sig.v() { 28 } else { 27 });

        // user_data layout: fee(8) | meta(32) | minBalanceUpdate(32) | signature(65)
        let mut user_data_bytes = Vec::with_capacity(8 + 32 + 32 + 65);
        user_data_bytes.extend_from_slice(&0_u64.to_be_bytes());
        user_data_bytes.extend_from_slice(&meta);
        user_data_bytes.extend_from_slice(&min_balance_update);
        user_data_bytes.extend_from_slice(&sig_bytes);

        let swap = Swap::new(
            component,
            default_token(usdc.clone()),
            default_token(usdt.clone()),
            BigUint::ZERO,
        )
        .with_user_data(Bytes::from(user_data_bytes));

        let encoding_context = EncodingContext {
            group_token_in: usdc.clone(),
            group_token_out: usdt.clone(),
            router_address: Some(Bytes::default()),
        };

        let encoder = EkuboV3SwapEncoder::new(Bytes::default(), Chain::Ethereum, None).unwrap();
        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        write_calldata_to_file("test_ekubo_v3_signed_swap_integration", hex_swap.as_str());
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
