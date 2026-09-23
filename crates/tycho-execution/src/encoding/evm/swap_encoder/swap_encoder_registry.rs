use std::{collections::HashMap, str::FromStr};

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::{
            DEFAULT_EXECUTORS_JSON, FALLBACK_KEY, FALLBACK_PREFIX, PRICE_LEVEL_STREAM_KEY,
            PRICE_LEVEL_STREAM_PREFIX, PROTOCOL_SPECIFIC_CONFIG, SLIPSTREAMS_FORKS,
            UNISWAP_V2_FORKS, UNISWAP_V3_FORKS,
        },
        swap_encoder::{
            aerodrome_v1::AerodromeV1SwapEncoder, balancer_v2::BalancerV2SwapEncoder,
            balancer_v3::BalancerV3SwapEncoder, bebop::BebopSwapEncoder, bopamm::BopAMMSwapEncoder,
            curve::CurveSwapEncoder, ekubo::EkuboSwapEncoder, ekubo_v3::EkuboV3SwapEncoder,
            erc_4626::ERC4626SwapEncoder, etherfi::EtherfiSwapEncoder,
            fallback::FallbackSwapEncoder, fermiswap::FermiSwapEncoder,
            fluid_v1::FluidV1SwapEncoder, hashflow::HashflowSwapEncoder,
            lido_v4::LidoV4SwapEncoder, liquidity_party::LiquidityPartySwapEncoder,
            liquorice::LiquoriceSwapEncoder, lunarbase::LunarBaseSwapEncoder,
            maverick_v2::MaverickV2SwapEncoder, metric::MetricSwapEncoder,
            native::NativeSwapEncoder, native_wrap::WrapSwapEncoder, propamm::PropAMMSwapEncoder,
            ring_swap_v2::RingSwapV2SwapEncoder, rocketpool::RocketpoolSwapEncoder,
            sky::SkySwapEncoder, slipstreams::SlipstreamsSwapEncoder,
            uniswap_v2::UniswapV2SwapEncoder, uniswap_v3::UniswapV3SwapEncoder,
            uniswap_v4::UniswapV4SwapEncoder,
        },
    },
    swap_encoder::SwapEncoder,
};

/// Registry containing all supported `SwapEncoders`.
#[derive(Clone)]
pub struct SwapEncoderRegistry {
    chain: Chain,
    /// A hashmap containing the protocol system as a key and the `SwapEncoder` as a value.
    encoders: HashMap<String, Box<dyn SwapEncoder>>,
}

impl SwapEncoderRegistry {
    pub fn new(chain: Chain) -> Self {
        Self { chain, encoders: HashMap::new() }
    }

    /// Creates a new registry pre-populated with all default encoders for the given chain.
    pub fn new_with_defaults(chain: Chain) -> Result<Self, EncodingError> {
        Self::new(chain).add_default_encoders(None)
    }

    /// Populates the registry with the default `SwapEncoders` for the given blockchain by
    /// parsing the executors' addresses in the file at the given path.
    pub fn add_default_encoders(
        mut self,
        executors_addresses: Option<String>,
    ) -> Result<Self, EncodingError> {
        let config_str = if let Some(addresses) = executors_addresses {
            addresses
        } else {
            DEFAULT_EXECUTORS_JSON.to_string()
        };
        let config: HashMap<Chain, HashMap<String, String>> = serde_json::from_str(&config_str)?;
        let executors = config
            .get(&self.chain)
            .ok_or(EncodingError::FatalError("No executors found for chain".to_string()))?;

        let protocol_specific_config: HashMap<Chain, HashMap<String, HashMap<String, String>>> =
            serde_json::from_str(PROTOCOL_SPECIFIC_CONFIG)?;
        let protocol_specific_config = protocol_specific_config
            .get(&self.chain)
            .ok_or(EncodingError::FatalError(
                "No protocol specific config found for chain".to_string(),
            ))?;
        for (protocol, executor_address) in executors {
            let encoder = self.create_encoder(
                protocol,
                Bytes::from_str(executor_address).map_err(|_| {
                    EncodingError::FatalError(format!(
                        "Invalid executor address for protocol {}",
                        protocol
                    ))
                })?,
                protocol_specific_config
                    .get(protocol)
                    .cloned(),
            )?;
            self.encoders
                .insert(protocol.to_string(), encoder);
        }
        Ok(self)
    }

    /// Adds an encoder to the registry, replacing any existing encoder for the same protocol.
    pub fn register_encoder(mut self, protocol: &str, encoder: Box<dyn SwapEncoder>) -> Self {
        self.encoders
            .insert(protocol.to_string(), encoder);
        self
    }

    /// Returns the encoder registered for `protocol_system`.
    ///
    /// Price-level-stream protocols (`pricelevelstream:{protocol}`) without an exact entry fall
    /// back to the family entry registered under `pricelevelstream`, so a single configured
    /// executor address serves every pAMM — including auto-detected, address-named ones.
    /// `fallback:{protocol}` resolves the same way against `fallback`.
    #[allow(clippy::borrowed_box)]
    pub fn get_encoder(&self, protocol_system: &str) -> Option<&Box<dyn SwapEncoder>> {
        if let Some(encoder) = self.encoders.get(protocol_system) {
            return Some(encoder);
        }
        if protocol_system.starts_with(PRICE_LEVEL_STREAM_PREFIX) {
            return self
                .encoders
                .get(PRICE_LEVEL_STREAM_KEY);
        }
        if protocol_system.starts_with(FALLBACK_PREFIX) {
            return self.encoders.get(FALLBACK_KEY);
        }
        None
    }

    /// The executor address of every encoder in this registry, keyed by protocol system.
    ///
    /// Several protocol systems may share one executor address, so the returned addresses are not
    /// necessarily distinct.
    pub fn executor_addresses(&self) -> HashMap<String, Bytes> {
        self.encoders
            .iter()
            .map(|(protocol, encoder)| (protocol.clone(), encoder.executor_address().clone()))
            .collect()
    }

    fn create_encoder(
        &self,
        protocol_system: &str,
        executor_address: Bytes,
        config: Option<HashMap<String, String>>,
    ) -> Result<Box<dyn SwapEncoder>, EncodingError> {
        match protocol_system {
            p if UNISWAP_V2_FORKS.contains(&p) => {
                Ok(Box::new(UniswapV2SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "ring_swap_v2" => {
                Ok(Box::new(RingSwapV2SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "aerodrome_v1" => {
                Ok(Box::new(AerodromeV1SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "vm:balancer_v2" => {
                Ok(Box::new(BalancerV2SwapEncoder::new(executor_address, self.chain, config)?))
            }
            p if UNISWAP_V3_FORKS.contains(&p) => {
                Ok(Box::new(UniswapV3SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "uniswap_v4" => {
                Ok(Box::new(UniswapV4SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "ekubo_v2" => {
                Ok(Box::new(EkuboSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "ekubo_v3" => {
                Ok(Box::new(EkuboV3SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "vm:bopamm" => {
                Ok(Box::new(BopAMMSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "vm:curve" => {
                Ok(Box::new(CurveSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "vm:maverick_v2" => {
                Ok(Box::new(MaverickV2SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "vm:balancer_v3" => {
                Ok(Box::new(BalancerV3SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "rfq:bebop" => {
                Ok(Box::new(BebopSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "rfq:hashflow" => {
                Ok(Box::new(HashflowSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "rfq:liquorice" => {
                Ok(Box::new(LiquoriceSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "rfq:metric" => {
                Ok(Box::new(MetricSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "rfq:native" => {
                Ok(Box::new(NativeSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "fluid_v1" => {
                Ok(Box::new(FluidV1SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "vm:fermiswap" => {
                Ok(Box::new(FermiSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "vm:liquidityparty" => {
                Ok(Box::new(LiquidityPartySwapEncoder::new(executor_address, self.chain, config)?))
            }
            p if SLIPSTREAMS_FORKS.contains(&p) => {
                Ok(Box::new(SlipstreamsSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "rocketpool" => {
                Ok(Box::new(RocketpoolSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "sky" => Ok(Box::new(SkySwapEncoder::new(executor_address, self.chain, config)?)),
            "erc4626" => {
                Ok(Box::new(ERC4626SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "lunarbase" => {
                Ok(Box::new(LunarBaseSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "native_wrapper" => {
                Ok(Box::new(WrapSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "etherfi" => {
                Ok(Box::new(EtherfiSwapEncoder::new(executor_address, self.chain, config)?))
            }
            // All pAMMs following the standard IPropAMM interface share one generic encoder /
            // executor; the concrete protocol is identified by the component, not the encoder. The
            // bare family key serves every protocol via the `get_encoder` fallback;
            // protocol-specific `pricelevelstream:{protocol}` entries override it per
            // protocol.
            pls if pls == PRICE_LEVEL_STREAM_KEY || pls.starts_with(PRICE_LEVEL_STREAM_PREFIX) => {
                Ok(Box::new(PropAMMSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "lido_v4" => {
                Ok(Box::new(LidoV4SwapEncoder::new(executor_address, self.chain, config)?))
            }
            // The TychoFallbackRouter path carries the fallback protocol in the swap data, so it
            // needs its own encoder; the family resolves like the price-level-stream one.
            f if f == FALLBACK_KEY || f.starts_with(FALLBACK_PREFIX) => {
                Ok(Box::new(FallbackSwapEncoder::new(executor_address, self.chain, config)?))
            }
            _ => Err(EncodingError::FatalError(format!(
                "Unknown protocol system: {}",
                protocol_system
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single `pricelevelstream` config entry serves the whole protocol family: the bare
    /// family key resolves as an exact entry, and every `pricelevelstream:{protocol}` protocol —
    /// including auto-detected, address-named protocols no config could enumerate — resolves to it
    /// through the fallback.
    #[test]
    fn test_price_level_stream_protocols_route_to_generic_encoder() {
        let executors = std::fs::read_to_string("config/test_executor_addresses.json").unwrap();
        let registry = SwapEncoderRegistry::new(Chain::Ethereum)
            .add_default_encoders(Some(executors))
            .unwrap();

        for protocol in [
            PRICE_LEVEL_STREAM_KEY,
            "pricelevelstream:fermiswap",
            "pricelevelstream:kipseli",
            "pricelevelstream:0x2222222222222222222222222222222222222222",
        ] {
            assert!(registry.get_encoder(protocol).is_some(), "no encoder resolved for {protocol}");
        }
        // The fallback is scoped to the price-level-stream prefix.
        assert!(registry
            .get_encoder("unknown_protocol")
            .is_none());
    }

    /// The TychoFallbackRouter family resolves like the price-level-stream family: the single
    /// `fallback` config entry serves the bare key and every `fallback:{protocol}` protocol,
    /// against the `FallbackExecutor` address.
    #[test]
    fn test_fallback_protocol_resolution() {
        let executors = std::fs::read_to_string("config/test_executor_addresses.json").unwrap();
        let registry = SwapEncoderRegistry::new(Chain::Ethereum)
            .add_default_encoders(Some(executors))
            .unwrap();
        let executor_address =
            Bytes::from_str("0x89CA9F4f77B267778EB2eA0Ba1bEAdEe8523af36").unwrap();

        for protocol in [
            FALLBACK_KEY,
            "fallback:fermiswap",
            "fallback:0x5979458912f80b96d30d4220af8e2e4925a33320",
        ] {
            let resolved = registry
                .get_encoder(protocol)
                .unwrap_or_else(|| panic!("no encoder resolved for {protocol}"));
            assert_eq!(resolved.executor_address(), &executor_address);
        }
        // The family fallback is scoped to the prefix.
        assert!(registry
            .get_encoder("fallbackless_protocol")
            .is_none());
    }

    #[test]
    fn test_default_encoders_build_for_every_configured_chain() {
        let chains = [
            Chain::Ethereum,
            Chain::Base,
            Chain::Unichain,
            Chain::Arbitrum,
            Chain::Bsc,
            Chain::Polygon,
            Chain::Plasma,
            Chain::Robinhood,
        ];
        for chain in chains {
            let registry = SwapEncoderRegistry::new_with_defaults(chain).unwrap_or_else(|e| {
                panic!("default encoders failed to build for chain {chain}: {e}")
            });
            assert!(
                registry
                    .get_encoder("uniswap_v3")
                    .is_some(),
                "chain {chain} is missing the uniswap_v3 encoder"
            );
        }
    }

    #[test]
    fn test_executor_addresses_match_registered_encoders() {
        let registry = SwapEncoderRegistry::new_with_defaults(Chain::Ethereum).unwrap();

        let executor_addresses = registry.executor_addresses();

        assert!(!executor_addresses.is_empty());
        for (protocol, executor_address) in executor_addresses {
            let encoder = registry
                .get_encoder(&protocol)
                .unwrap_or_else(|| panic!("no encoder registered for {protocol}"));
            assert_eq!(encoder.executor_address(), &executor_address);
        }
    }
}
