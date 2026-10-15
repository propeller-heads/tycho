use std::{collections::HashMap, str::FromStr};

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::swap_encoder::{fallback::FallbackSwap, native::NativeSwapEncoder},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Protocol config key of the chain's `NativeFallbackRouter` address.
const FALLBACK_ROUTER_CONFIG_KEY: &str = "fallback_router";

/// `[tokenIn: 20][tokenOut: 20][target: 20]`, the part of the Native swap data before the
/// executor's length prefix.
const NATIVE_HEAD_LENGTH: usize = 60;

/// Encodes a Native swap for `NativeFallbackRouter`. The quote is requested with that router as
/// recipient.
#[derive(Clone)]
pub struct NativeFallbackSwapEncoder {
    executor_address: Bytes,
    chain: Chain,
    fallback_router: Bytes,
    native: NativeSwapEncoder,
}

impl NativeFallbackSwapEncoder {
    fn quote_context(&self, encoding_context: &EncodingContext) -> EncodingContext {
        EncodingContext {
            router_address: Some(self.fallback_router.clone()),
            ..encoding_context.clone()
        }
    }
}

impl SwapEncoder for NativeFallbackSwapEncoder {
    /// Reads `fallback_router` and Native's `router_v6` from `config`.
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
                    "Missing {FALLBACK_ROUTER_CONFIG_KEY} in the fallback:rfq:native config for \
                     {chain}: add the chain's NativeFallbackRouter address to \
                     protocol_specific_addresses.json"
                ))
            })?;
        let fallback_router = Bytes::from_str(fallback_router).map_err(|_| {
            EncodingError::FatalError(format!(
                "Invalid {FALLBACK_ROUTER_CONFIG_KEY} address for {chain}: {fallback_router}"
            ))
        })?;
        let native = NativeSwapEncoder::new(executor_address.clone(), chain, config)?;
        Ok(Self { executor_address, chain, fallback_router, native })
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
                "Fallback protocol {} is not supported on {}: the chain's NativeFallbackRouter \
                 would revert the swap",
                fallback.protocol.user_data_name(),
                self.chain
            )));
        }

        let native = self
            .native
            .encode_swap(swap, &self.quote_context(encoding_context))?;
        let (head, native_data) = native.split_at(NATIVE_HEAD_LENGTH);
        let native_data_length = u32::try_from(native_data.len()).map_err(|_| {
            EncodingError::FatalError(format!(
                "Native swap data of {} bytes does not fit the uint32 length prefix",
                native_data.len()
            ))
        })?;

        let mut data = head.to_vec();
        data.extend_from_slice(&native_data_length.to_be_bytes());
        data.extend_from_slice(native_data);
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
