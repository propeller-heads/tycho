// Copyright (c) 2026 Everlong Labs Limited
use std::collections::HashMap;

use alloy::primitives::Address;
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// The swap venue: `pool.swap(tokenIn, tokenOut, ...)`, either direction.
const VENUE_SWAP: u8 = 0;
/// The leverage venue's lever-up leg: `pool.leverUp(poolAssetIn, ...)`.
const VENUE_LEVER_UP: u8 = 1;
/// The lever-up component's discriminator, the trailing `uint64` of its id.
const LEVER_UP_DISCRIMINATOR: u64 = 1;

/// Encodes a fill on an Everlong FLAMM pool for `FLAMMExecutor`.
///
/// One pool carries two components: the swap venue, whose id is the pool address
/// (20 bytes), and the lever-up venue, whose id is `pool (20) || 0x00000000 ||
/// uint64(1)` (32 bytes). The component id therefore selects the venue byte the
/// executor dispatches on.
///
/// The executor takes the pool from calldata and looks it up in the factory's
/// `isPool` registry, so the data is `pool (20) || tokenIn (20) || tokenOut (20)
/// || venue (1)`. FLAMM is deployed on Base only.
#[derive(Clone)]
pub struct FLAMMSwapEncoder {
    executor_address: Bytes,
}

impl FLAMMSwapEncoder {
    /// Splits a component id into the pool address and the venue it names.
    fn decode_component_id(id: &str) -> Result<(Address, u8), EncodingError> {
        let raw = hex::decode(id.trim_start_matches("0x"))
            .map_err(|_| EncodingError::FatalError(format!("Invalid FLAMM component id {id}")))?;
        match raw.len() {
            20 => Ok((Address::from_slice(&raw), VENUE_SWAP)),
            32 => {
                let discriminator = u64::from_be_bytes(
                    raw[24..32]
                        .try_into()
                        .expect("eight bytes"),
                );
                if raw[20..24] != [0u8; 4] || discriminator != LEVER_UP_DISCRIMINATOR {
                    return Err(EncodingError::FatalError(format!(
                        "Unknown FLAMM component discriminator in id {id}"
                    )));
                }
                Ok((Address::from_slice(&raw[..20]), VENUE_LEVER_UP))
            }
            _ => Err(EncodingError::FatalError(format!("Invalid FLAMM component id {id}"))),
        }
    }
}

impl SwapEncoder for FLAMMSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        if chain != Chain::Base {
            return Err(EncodingError::FatalError(
                "FLAMM swaps are only supported on Base".to_string(),
            ));
        }
        Ok(Self { executor_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let (pool, venue) = Self::decode_component_id(&swap.component().id)?;
        // FLAMM pairs are ERC-20 only (cbBTC/USDC), so no native-token translation.
        let token_in = bytes_to_address(&swap.token_in().address)?;
        let token_out = bytes_to_address(&swap.token_out().address)?;

        let mut encoded = Vec::with_capacity(61);
        encoded.extend_from_slice(pool.as_slice());
        encoded.extend_from_slice(token_in.as_slice());
        encoded.extend_from_slice(token_out.as_slice());
        encoded.push(venue);
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
    use crate::encoding::{evm::utils::write_calldata_to_file, models::default_token};

    const POOL: &str = "0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572";
    const CBBTC: &str = "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf";
    const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";

    fn component(id: &str) -> ProtocolComponent {
        ProtocolComponent {
            id: id.to_owned(),
            protocol_system: "flamm".to_owned(),
            ..Default::default()
        }
    }

    fn encoder() -> FLAMMSwapEncoder {
        FLAMMSwapEncoder::new(Bytes::zero(20), Chain::Base, None).unwrap()
    }

    fn pack(
        component: ProtocolComponent,
        token_in: &str,
        token_out: &str,
    ) -> Result<String, EncodingError> {
        let token_in = Bytes::from(token_in);
        let token_out = Bytes::from(token_out);
        let swap = Swap::new(
            component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };
        encoder()
            .encode_swap(&swap, &context)
            .map(encode)
    }

    #[test]
    fn test_encode_flamm_sell() {
        let hex_swap = pack(component(POOL), CBBTC, USDC).unwrap();
        assert_eq!(
            hex_swap,
            concat!(
                "c0fdcb1799ccc2cebaa1fe247157b0df33d57572",
                "cbb7c0000ab88b473b1f5afd9ef808440eed33bf",
                "833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                "00",
            )
        );
        write_calldata_to_file("test_encode_flamm_sell", hex_swap.as_str());
    }

    #[test]
    fn test_encode_flamm_buy() {
        let hex_swap = pack(component(POOL), USDC, CBBTC).unwrap();
        assert_eq!(
            hex_swap,
            concat!(
                "c0fdcb1799ccc2cebaa1fe247157b0df33d57572",
                "833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                "cbb7c0000ab88b473b1f5afd9ef808440eed33bf",
                "00",
            )
        );
        write_calldata_to_file("test_encode_flamm_buy", hex_swap.as_str());
    }

    #[test]
    fn test_encode_flamm_lever_up() {
        let id = format!("{POOL}000000000000000000000001");
        let hex_swap = pack(component(&id), CBBTC, USDC).unwrap();
        assert_eq!(
            hex_swap,
            concat!(
                "c0fdcb1799ccc2cebaa1fe247157b0df33d57572",
                "cbb7c0000ab88b473b1f5afd9ef808440eed33bf",
                "833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                "01",
            )
        );
        write_calldata_to_file("test_encode_flamm_lever_up", hex_swap.as_str());
    }

    #[test]
    fn test_encoder_rejects_unknown_discriminator() {
        // Discriminator 2 is not a venue.
        let id = format!("{POOL}000000000000000000000002");
        assert!(pack(component(&id), CBBTC, USDC).is_err());
        // The four bytes between the pool and the discriminator must be zero.
        let id = format!("{POOL}000000010000000000000001");
        assert!(pack(component(&id), CBBTC, USDC).is_err());
    }

    #[test]
    fn test_encoder_rejects_other_id_lengths() {
        assert!(pack(component("0xc0fdcb1799ccc2cebaa1fe247157b0df33d575"), CBBTC, USDC).is_err());
        assert!(pack(component(&format!("{POOL}01")), CBBTC, USDC).is_err());
        assert!(pack(component("not hex"), CBBTC, USDC).is_err());
    }

    #[test]
    fn test_encoder_rejects_non_base_chain() {
        assert!(FLAMMSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).is_err());
    }
}
