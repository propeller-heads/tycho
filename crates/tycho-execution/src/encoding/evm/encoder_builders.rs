use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::get_router_address,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
        tycho_encoders::{TychoExecutorEncoder, TychoRouterEncoder},
    },
    tycho_encoder::TychoEncoder,
};

/// Builder pattern for constructing a `TychoRouterEncoder` with customizable options.
///
/// This struct allows setting a chain and strategy encoder before building the final encoder.
pub struct TychoRouterEncoderBuilder {
    chain: Option<Chain>,
    swap_encoder_registry: Option<SwapEncoderRegistry>,
    router_address: Option<Bytes>,
}

impl Default for TychoRouterEncoderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TychoRouterEncoderBuilder {
    pub fn new() -> Self {
        TychoRouterEncoderBuilder { chain: None, swap_encoder_registry: None, router_address: None }
    }
    pub fn chain(mut self, chain: Chain) -> Self {
        self.chain = Some(chain);
        self
    }

    pub fn swap_encoder_registry(mut self, swap_encoder_registry: SwapEncoderRegistry) -> Self {
        self.swap_encoder_registry = Some(swap_encoder_registry);
        self
    }

    /// Sets the `router_address` manually.
    /// If it's not set, the default router address will be used (config/router_addresses.json)
    pub fn router_address(mut self, router_address: Bytes) -> Self {
        self.router_address = Some(router_address);
        self
    }

    /// Builds the `TychoRouterEncoder` instance using the configured chain.
    /// Returns an error if either the chain has not been set.
    pub fn build(self) -> Result<Box<dyn TychoEncoder>, EncodingError> {
        if let (Some(chain), Some(swap_encoder_registry)) = (self.chain, self.swap_encoder_registry)
        {
            let tycho_router_address = if let Some(address) = self.router_address {
                address
            } else {
                get_router_address(&chain)?.clone()
            };

            Ok(Box::new(TychoRouterEncoder::new(swap_encoder_registry, tycho_router_address)?))
        } else {
            Err(EncodingError::FatalError(
                "Please set the chain and swap encoder registry before building the encoder"
                    .to_string(),
            ))
        }
    }
}

/// Builder pattern for constructing a `TychoExecutorEncoder` with customizable options.
pub struct TychoExecutorEncoderBuilder {
    swap_encoder_registry: Option<SwapEncoderRegistry>,
}

impl Default for TychoExecutorEncoderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TychoExecutorEncoderBuilder {
    pub fn new() -> Self {
        TychoExecutorEncoderBuilder { swap_encoder_registry: None }
    }

    pub fn swap_encoder_registry(mut self, swap_encoder_registry: SwapEncoderRegistry) -> Self {
        self.swap_encoder_registry = Some(swap_encoder_registry);
        self
    }

    /// Builds the `TychoExecutorEncoder` instance using the configured chain and strategy.
    /// Returns an error if either the chain or strategy has not been set.
    pub fn build(self) -> Result<Box<dyn TychoEncoder>, EncodingError> {
        if let Some(swap_encoder_registry) = self.swap_encoder_registry {
            Ok(Box::new(TychoExecutorEncoder::new(swap_encoder_registry)?))
        } else {
            Err(EncodingError::FatalError(
                "Please set the swap encoder registry before building the encoder".to_string(),
            ))
        }
    }
}
