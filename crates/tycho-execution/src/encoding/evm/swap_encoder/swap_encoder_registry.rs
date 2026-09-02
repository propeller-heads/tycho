use std::{collections::HashMap, str::FromStr};

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::{
            DEFAULT_EXECUTORS_JSON, PRICE_LEVEL_STREAM_KEY, PRICE_LEVEL_STREAM_PREFIX,
            PROPAMM_FALLBACK_KEY, PROPAMM_FALLBACK_PREFIX, PROTOCOL_SPECIFIC_CONFIG,
        },
        swap_encoder::{
            aerodrome_v1::AerodromeV1SwapEncoder, aqua0::Aqua0SwapEncoder,
            balancer_v2::BalancerV2SwapEncoder,
            balancer_v3::BalancerV3SwapEncoder, bebop::BebopSwapEncoder, bopamm::BopAMMSwapEncoder,
            curve::CurveSwapEncoder, ekubo::EkuboSwapEncoder, ekubo_v3::EkuboV3SwapEncoder,
            erc_4626::ERC4626SwapEncoder, etherfi::EtherfiSwapEncoder, fermiswap::FermiSwapEncoder,
            fluid_v1::FluidV1SwapEncoder, hashflow::HashflowSwapEncoder,
            liquidity_party::LiquidityPartySwapEncoder, liquorice::LiquoriceSwapEncoder,
            lunarbase::LunarBaseSwapEncoder, maverick_v2::MaverickV2SwapEncoder,
            metric::MetricSwapEncoder, native_wrap::WrapSwapEncoder, propamm::PropAMMSwapEncoder,
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
    /// Price-level-stream protocols (`pricelevelstream:{venue}`) without an exact entry fall
    /// back to the family entry registered under `pricelevelstream`, so a single configured
    /// executor address serves every pAMM — including auto-detected, address-named ones.
    /// `propammfallback:{venue}` resolves the same way against `propammfallback`.
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
        if protocol_system.starts_with(PROPAMM_FALLBACK_PREFIX) {
            return self.encoders.get(PROPAMM_FALLBACK_KEY);
        }
        None
    }

    fn create_encoder(
        &self,
        protocol_system: &str,
        executor_address: Bytes,
        config: Option<HashMap<String, String>>,
    ) -> Result<Box<dyn SwapEncoder>, EncodingError> {
        match protocol_system {
            "uniswap_v2" | "sushiswap_v2" | "pancakeswap_v2" | "quickswap_v2" => {
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
            "uniswap_v3" | "pancakeswap_v3" => {
                Ok(Box::new(UniswapV3SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "uniswap_v4" => {
                Ok(Box::new(UniswapV4SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "rfq:aqua0" => {
                Ok(Box::new(Aqua0SwapEncoder::new(executor_address, self.chain, config)?))
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
            "fluid_v1" => {
                Ok(Box::new(FluidV1SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "vm:fermiswap" => {
                Ok(Box::new(FermiSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "vm:liquidityparty" => {
                Ok(Box::new(LiquidityPartySwapEncoder::new(executor_address, self.chain, config)?))
            }
            "aerodrome_slipstreams" => {
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
            "velodrome_slipstreams" => {
                Ok(Box::new(SlipstreamsSwapEncoder::new(executor_address, self.chain, config)?))
            }
            // Ramses V3 reuses the standard Uniswap V3 executor unchanged, encoded via the
            // Slipstreams encoder. Three things make this sound:
            //   1. ABI match: the Ramses pool exposes the identical
            //      `swap(address,bool,int256,uint160,bytes)` and calls `uniswapV3SwapCallback`,
            //      which the router's selector-agnostic fallback routes back to the executor.
            //   2. The executor's `_decodeData` reads only the pool address (bytes 43..63) and the
            //      zero-for-one flag (byte 63): it calls `pool.swap` on that address without
            //      recomputing it, and never touches the 3-byte slot at bytes 40..43. So it is
            //      irrelevant both that Ramses keys pools by tick spacing rather than fee, and that
            //      the Slipstreams encoder packs `tick_spacing` into that slot (where Uniswap V3
            //      packs the fee).
            //   3. The SlipstreamsExecutor contract is byte-for-byte identical to the
            //      UniswapV3Executor, so the encoder choice does not imply a different on-chain
            //      executor.
            "ramses_v3" => {
                Ok(Box::new(SlipstreamsSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "native_wrapper" => {
                Ok(Box::new(WrapSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "etherfi" => {
                Ok(Box::new(EtherfiSwapEncoder::new(executor_address, self.chain, config)?))
            }
            // All pAMMs following the standard IPropAMM interface share one generic encoder /
            // executor; the concrete venue is identified by the component, not the encoder. The
            // bare family key serves every venue via the `get_encoder` fallback; venue-specific
            // `pricelevelstream:{venue}` entries override it per venue.
            // The PropAMMRouter path takes the same calldata, so it reuses the same encoder and
            // differs only in the executor address configured for the family.
            pls if pls == PRICE_LEVEL_STREAM_KEY ||
                pls.starts_with(PRICE_LEVEL_STREAM_PREFIX) ||
                pls == PROPAMM_FALLBACK_KEY ||
                pls.starts_with(PROPAMM_FALLBACK_PREFIX) =>
            {
                Ok(Box::new(PropAMMSwapEncoder::new(executor_address, self.chain, config)?))
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
    /// family key resolves as an exact entry, and every `pricelevelstream:{venue}` protocol —
    /// including auto-detected, address-named venues no config could enumerate — resolves to it
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

    /// The PropAMMRouter family resolves the same way, and to a different executor than the direct
    /// path — same calldata, different call target.
    #[test]
    fn test_propamm_fallback_protocol_resolution() {
        let executors = std::fs::read_to_string("config/test_executor_addresses.json").unwrap();
        let registry = SwapEncoderRegistry::new(Chain::Ethereum)
            .add_default_encoders(Some(executors))
            .unwrap();

        for protocol in [
            PROPAMM_FALLBACK_KEY,
            "propammfallback:fermiswap",
            "propammfallback:0x5979458912f80b96d30d4220af8e2e4925a33320",
        ] {
            assert!(registry.get_encoder(protocol).is_some(), "no encoder resolved for {protocol}");
        }

        let direct = registry
            .get_encoder("pricelevelstream:fermiswap")
            .unwrap()
            .executor_address()
            .clone();
        let via_router = registry
            .get_encoder("propammfallback:fermiswap")
            .unwrap()
            .executor_address()
            .clone();
        assert_ne!(direct, via_router);
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
}
