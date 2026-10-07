use std::{collections::HashMap, str::FromStr};

use alloy::sol_types::SolValue;
use tycho_common::{
    models::{Address, Chain},
    Bytes,
};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::bytes_to_address,
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on a Liquidity Party pool through the given executor address.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct LiquidityPartySwapEncoder {
    executor_address: Bytes,
}

impl LiquidityPartySwapEncoder {
    fn get_token_indexes(&self, swap: &Swap) -> Result<(Bytes, Bytes, u8, u8), EncodingError> {
        let token_addresses: &Vec<Address> = &swap.component().tokens;
        let token_in = &swap.token_in().address;
        let token_out = &swap.token_out().address;

        let token_in_idx = token_addresses
            .iter()
            .position(|addr| addr == token_in)
            .ok_or(EncodingError::FatalError("Token in not found in pool tokens".to_string()))?;

        let token_out_idx = token_addresses
            .iter()
            .position(|addr| addr == token_out)
            .ok_or(EncodingError::FatalError("Token out not found in pool tokens".to_string()))?;

        Ok((token_in.clone(), token_out.clone(), token_in_idx as u8, token_out_idx as u8))
    }
}

impl SwapEncoder for LiquidityPartySwapEncoder {
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
        let pool_addr = Address::from_str(&swap.component().id).map_err(|_| {
            EncodingError::FatalError("LiqP swap encoder: invalid component id".to_string())
        })?;

        let (token_in, token_out, token_in_idx, token_out_idx) = self.get_token_indexes(swap)?;

        let args = (
            bytes_to_address(&pool_addr)?,
            bytes_to_address(&token_in)?,
            bytes_to_address(&token_out)?,
            token_in_idx.to_be_bytes(),
            token_out_idx.to_be_bytes(),
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
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::utils::write_calldata_to_file, models::default_token};

    #[test]
    fn test_encode_liquidityparty() {
        let liqp_pool = ProtocolComponent {
            // mainnet test pool — 3-token USDC/WETH/AAVE pool
            id: String::from("0x1270Da05Cf1d047763CEEfDe25a4a5438b26fdA6"),
            tokens: [
                Address::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), // USDC [0]
                Address::from("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), // WETH [1]
                Address::from("0x7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9"), // AAVE [2]
            ]
            .to_vec(),
            ..Default::default()
        };

        let token_in = Bytes::from("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"); // WETH (index 1)
        let token_out = Bytes::from("0x7Fc66500c84A76Ad7e9c93437bFc5Ac33E2DDaE9"); // AAVE (index 2)
        let swap = Swap::new(
            liqp_pool,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        );
        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };
        let encoder = LiquidityPartySwapEncoder::new(
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
                // pool address
                "1270da05cf1d047763ceefde25a4a5438b26fda6",
                // in token address (WETH)
                "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                // out token address (AAVE)
                "7fc66500c84a76ad7e9c93437bfc5ac33e2ddae9",
                // in token index (WETH = 1)
                "01",
                // out token index (AAVE = 2)
                "02",
            ))
        );
        write_calldata_to_file("test_encode_liquidityparty", hex_swap.as_str());
    }
}
