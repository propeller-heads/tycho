use std::{collections::HashMap, str::FromStr};

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::swap_encoder::{fallback::FallbackSwap, liquorice::LiquoriceSwapEncoder},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Protocol config key of the chain's `LiquoriceFallbackRouter` address.
const FALLBACK_ROUTER_CONFIG_KEY: &str = "fallback_router";

/// `[tokenIn: 20][tokenOut: 20]`, the part of the Liquorice swap data before the executor's
/// length prefix.
const LIQUORICE_HEAD_LENGTH: usize = 40;

/// `[partialFillOffset: 4][originalBaseTokenAmount: 32][minBaseTokenAmount: 32]`, which the
/// Liquorice swap data carries between the tokens and the settlement calldata.
/// `LiquoriceFallbackRouter` does not partially fill, so it takes the calldata alone.
const LIQUORICE_FILL_FIELDS_LENGTH: usize = 68;

/// Encodes a Liquorice swap for `LiquoriceFallbackRouter`. The quote is requested with that router
/// as trader and receiver, and fills its signed amount.
#[derive(Clone)]
pub struct LiquoriceFallbackSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
    fallback_router: Bytes,
    liquorice: LiquoriceSwapEncoder,
}

impl LiquoriceFallbackSwapEncoder {
    fn quote_context(&self, encoding_context: &EncodingContext) -> EncodingContext {
        EncodingContext {
            router_address: Some(self.fallback_router.clone()),
            ..encoding_context.clone()
        }
    }
}

impl SwapEncoder for LiquoriceFallbackSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let fallback_router = config
            .as_ref()
            .and_then(|config| config.get(FALLBACK_ROUTER_CONFIG_KEY))
            .ok_or_else(|| {
                EncodingError::FatalError(format!(
                    "Missing {FALLBACK_ROUTER_CONFIG_KEY} in the fallback:rfq:liquorice config \
                     for {chain}: add the chain's LiquoriceFallbackRouter address to \
                     protocol_specific_addresses.json"
                ))
            })?;
        let fallback_router = Bytes::from_str(fallback_router).map_err(|_| {
            EncodingError::FatalError(format!(
                "Invalid {FALLBACK_ROUTER_CONFIG_KEY} address for {chain}: {fallback_router}"
            ))
        })?;
        let liquorice = LiquoriceSwapEncoder::new(executor_address.clone(), chain, None)?;
        Ok(Self { executor_address, chain, fallback_router, liquorice })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let fallback = FallbackSwap::from_user_data(swap.user_data())?;
        if !fallback
            .protocol
            .supported_on(self.chain)
        {
            return Err(EncodingError::InvalidInput(format!(
                "Fallback protocol {} is not supported on {}: the chain's \
                 LiquoriceFallbackRouter would revert the swap",
                fallback.protocol.user_data_name(),
                self.chain
            )));
        }

        let liquorice = self
            .liquorice
            .encode_swap(swap, &self.quote_context(encoding_context))?;
        let (head, rest) = liquorice.split_at(LIQUORICE_HEAD_LENGTH);
        let settlement_calldata = &rest[LIQUORICE_FILL_FIELDS_LENGTH..];
        let settlement_calldata_length =
            u32::try_from(settlement_calldata.len()).map_err(|_| {
                EncodingError::FatalError(format!(
                    "Liquorice calldata of {} bytes does not fit the uint32 length prefix",
                    settlement_calldata.len()
                ))
            })?;

        let mut data = head.to_vec();
        data.extend_from_slice(&settlement_calldata_length.to_be_bytes());
        data.extend_from_slice(settlement_calldata);
        data.extend(fallback.encode()?);
        Ok(data)
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn blocks_on_quote(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}
