use std::{collections::HashMap, str::FromStr};

use alloy::primitives::{aliases::U24, Address};
use tycho_common::{models::Chain, Bytes};

use super::uniswap_v3::encode_uniswap_v3_swap;
use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on a Nami V4 (Algebra Integral) pool for `UniswapV3Executor`.
///
/// An Algebra pool's `swap(recipient, zeroToOne, amountRequired, limitSqrtPrice, data)` has the
/// same selector as Uniswap V3's, and TychoRouterV3 answers its `algebraSwapCallback` through its
/// selector-agnostic fallback, so the Uniswap V3 executor runs these swaps unchanged. Its calldata
/// carries a Uniswap V3 fee tier the executor never reads; Algebra fees are set per swap by the
/// pool's plugin, so that field is zero.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct NamiV4SwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for NamiV4SwapEncoder {
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
        let token_in = bytes_to_address(&swap.token_in().address)?;
        let token_out = bytes_to_address(&swap.token_out().address)?;
        let zero_to_one = token_in < token_out;
        let pool = Address::from_str(&swap.component().id).map_err(|err| {
            EncodingError::FatalError(format!(
                "Invalid Nami V4 component id {}: expected the pool address: {err}",
                swap.component().id
            ))
        })?;

        Ok(encode_uniswap_v3_swap(token_in, token_out, U24::ZERO, pool, zero_to_one))
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

    /// Nami's WETH/USDC pool on Base.
    const POOL: &str = "0x0e8af367dba0c9cb4674d38539ef3465a25a7237";
    const WETH: &str = "0x4200000000000000000000000000000000000006";
    const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";

    fn encode_nami_swap(token_in: &str, token_out: &str) -> String {
        let pool = ProtocolComponent {
            id: String::from(POOL),
            protocol_system: String::from("nami_v4"),
            ..Default::default()
        };
        let token_in = Bytes::from(token_in);
        let token_out = Bytes::from(token_out);
        let swap = Swap::new(
            pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
            origin: None,
        };
        let encoder = NamiV4SwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Base,
            None,
        )
        .unwrap();
        encode(
            encoder
                .encode_swap(&swap, &encoding_context)
                .unwrap(),
        )
    }

    #[test]
    fn test_encode_nami_v4() {
        // WETH -> USDC: WETH is token0, so zero for one. `NamiV4.t.sol` checks the same bytes
        // against the executor's layout.
        let encoded = encode_nami_swap(WETH, USDC);

        assert_eq!(
            encoded,
            String::from(concat!(
                // in token
                "4200000000000000000000000000000000000006",
                // out token
                "833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                // unused fee
                "000000",
                // pool
                "0e8af367dba0c9cb4674d38539ef3465a25a7237",
                // zero for one
                "01",
            ))
        );
        write_calldata_to_file("test_encode_nami_v4", encoded.as_str());
    }

    #[test]
    fn test_encode_nami_v4_one_for_zero() {
        let encoded = encode_nami_swap(USDC, WETH);

        assert_eq!(
            encoded,
            String::from(concat!(
                // in token
                "833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                // out token
                "4200000000000000000000000000000000000006",
                // unused fee
                "000000",
                // pool
                "0e8af367dba0c9cb4674d38539ef3465a25a7237",
                // one for zero
                "00",
            ))
        );
    }
}
