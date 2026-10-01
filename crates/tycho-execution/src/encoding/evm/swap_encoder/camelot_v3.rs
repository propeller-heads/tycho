use std::{collections::HashMap, str::FromStr};

use alloy::{primitives::Address, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on a Camelot V3 pool for the Uniswap V3 executor.
///
/// Camelot V3 pools (Algebra V1.9 on Arbitrum One) expose the same
/// `swap(address,bool,int256,uint160,bytes)` entry point as Uniswap V3 and settle through
/// `algebraSwapCallback`, which the router's selector-agnostic fallback routes to the executor
/// like any other callback. The executor reads only the pool address (bytes `43..63`) and the
/// direction flag (byte `63`) and never the 3-byte slot at bytes `40..43` where the Uniswap V3
/// encoder packs the pool fee. That slot is zero here: a Camelot pool has no static fee, its fee
/// is adaptive and differs per direction.
///
/// The executor funds the callback with the encoded input amount. Should the pool run out of
/// liquidity before absorbing all of it, the surplus stays in the pool and only the router's
/// minimum-output check bounds the loss, exactly as for Uniswap V3. Simulation refuses such
/// trades up front: the adapter reverts with `LimitExceeded` instead of reporting a partial
/// fill.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct CamelotV3SwapEncoder {
    executor_address: Bytes,
}

impl CamelotV3SwapEncoder {
    /// Bytes `40..43` of the payload, where the Uniswap V3 executor format carries a fee.
    const NO_FEE: [u8; 3] = [0; 3];

    fn get_zero_to_one(sell_token_address: Address, buy_token_address: Address) -> bool {
        sell_token_address < buy_token_address
    }
}

impl SwapEncoder for CamelotV3SwapEncoder {
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
            EncodingError::FatalError("Invalid Camelot V3 component id".to_string())
        })?;

        let args = (token_in_address, token_out_address, Self::NO_FEE, component_id, zero_to_one);

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
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::models::{default_token, Swap};

    /// The payload is the 64-byte Uniswap V3 executor format with a zeroed fee slot.
    #[test]
    fn test_encode_camelot_v3() {
        // Camelot V3 WETH/USDC pool on Arbitrum One: token0 = WETH, token1 = USDC.
        let pool = ProtocolComponent {
            id: String::from("0xB1026b8e7276e7AC75410F1fcbbe21796e8f7526"),
            protocol_system: String::from("vm:camelot_v3"),
            ..Default::default()
        };
        let weth = Bytes::from("0x82af49447d8a07e3bd95bd0d56f35241523fbab1");
        let usdc = Bytes::from("0xaf88d065e77c8cc2239327c5edb3a432268e5831");
        let encoder = CamelotV3SwapEncoder::new(
            Bytes::from("0xCaAac0C6193E3e2e3E8E94bAf6367F75BaE591C9"),
            Chain::Arbitrum,
            None,
        )
        .unwrap();
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: weth.clone(),
            group_token_out: usdc.clone(),
        };

        let sell_weth = Swap::new(
            pool.clone(),
            default_token(weth.clone()),
            default_token(usdc.clone()),
            BigUint::ZERO,
        );
        let encoded = encoder
            .encode_swap(&sell_weth, &encoding_context)
            .unwrap();
        assert_eq!(encoded.len(), 64);
        assert_eq!(
            encode(&encoded),
            concat!(
                // in token
                "82af49447d8a07e3bd95bd0d56f35241523fbab1",
                // out token
                "af88d065e77c8cc2239327c5edb3a432268e5831",
                // unused fee slot
                "000000",
                // pool
                "b1026b8e7276e7ac75410f1fcbbe21796e8f7526",
                // zero to one
                "01",
            )
        );

        let sell_usdc = Swap::new(pool, default_token(usdc), default_token(weth), BigUint::ZERO);
        let encoded = encoder
            .encode_swap(&sell_usdc, &encoding_context)
            .unwrap();
        assert_eq!(
            encode(&encoded),
            concat!(
                "af88d065e77c8cc2239327c5edb3a432268e5831",
                "82af49447d8a07e3bd95bd0d56f35241523fbab1",
                "000000",
                "b1026b8e7276e7ac75410f1fcbbe21796e8f7526",
                "00",
            )
        );
    }
}
