use std::{collections::HashMap, str::FromStr};

use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::{
        constants::{DEFAULT_EXECUTORS_JSON, PROTOCOL_SPECIFIC_CONFIG},
        swap_encoder::{
            aerodrome_v1::AerodromeV1SwapEncoder, balancer_v2::BalancerV2SwapEncoder,
            balancer_v3::BalancerV3SwapEncoder, bebop::BebopSwapEncoder, curve::CurveSwapEncoder,
            ekubo::EkuboSwapEncoder, ekubo_v3::EkuboV3SwapEncoder, erc_4626::ERC4626SwapEncoder,
            etherfi::EtherfiSwapEncoder, fluid_v1::FluidV1SwapEncoder,
            hashflow::HashflowSwapEncoder, liquidity_party::LiquidityPartySwapEncoder,
            liquorice::LiquoriceSwapEncoder, maverick_v2::MaverickV2SwapEncoder,
            native_wrap::WrapSwapEncoder, rocketpool::RocketpoolSwapEncoder,
            slipstreams::SlipstreamsSwapEncoder, uniswap_v2::UniswapV2SwapEncoder,
            uniswap_v3::UniswapV3SwapEncoder, uniswap_v4::UniswapV4SwapEncoder,
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

    #[allow(clippy::borrowed_box)]
    pub fn get_encoder(&self, protocol_system: &str) -> Option<&Box<dyn SwapEncoder>> {
        self.encoders.get(protocol_system)
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
            "ekubo_v2" => {
                Ok(Box::new(EkuboSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "ekubo_v3" => {
                Ok(Box::new(EkuboV3SwapEncoder::new(executor_address, self.chain, config)?))
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
            "fluid_v1" => {
                Ok(Box::new(FluidV1SwapEncoder::new(executor_address, self.chain, config)?))
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
            "erc4626" => {
                Ok(Box::new(ERC4626SwapEncoder::new(executor_address, self.chain, config)?))
            }
            "velodrome_slipstreams" => {
                Ok(Box::new(SlipstreamsSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "native_wrapper" => {
                Ok(Box::new(WrapSwapEncoder::new(executor_address, self.chain, config)?))
            }
            "etherfi" => {
                Ok(Box::new(EtherfiSwapEncoder::new(executor_address, self.chain, config)?))
            }
            _ => Err(EncodingError::FatalError(format!(
                "Unknown protocol system: {}",
                protocol_system
            ))),
        }
    }
}
