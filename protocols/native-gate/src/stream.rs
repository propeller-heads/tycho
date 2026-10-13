use std::{collections::HashMap, time::Duration};

use futures::Stream;
use miette::{IntoDiagnostic, WrapErr};
use tycho_client::{feed::component_tracker::ComponentFilter, stream::RetryConfiguration};
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};
use tycho_simulation::{
    evm::{
        decoder::StreamDecodeError,
        engine_db::tycho_db::PreCachedDB,
        protocol::{
            aerodrome_slipstreams::state::AerodromeSlipstreamsState,
            aerodrome_v1::state::AerodromeV1State,
            balancer_v3::BalancerV3State,
            cowamm::state::CowAMMState,
            curve::CurveState,
            ekubo::state::EkuboState,
            ekubo_v3::state::EkuboV3State,
            erc4626::state::ERC4626State,
            etherfi::state::EtherfiState,
            filters::{
                balancer_v2_pool_filter, curve_filter, ekubo_v3_extension_filter, erc4626_filter,
                fluid_v1_paused_pools_filter, liquidityparty_killed_pools_filter,
            },
            fluid::FluidV1,
            lido_v4::state::LidoV4State,
            lunarbase::LunarBaseState,
            pancakeswap_v2::state::PancakeswapV2State,
            ramses_v3::state::RamsesV3State,
            ring_swap_v2::state::RingSwapV2State,
            rocketpool::state::RocketpoolState,
            sky::state::SkyState,
            uniswap_v2::state::UniswapV2State,
            uniswap_v3::state::UniswapV3State,
            uniswap_v4::state::UniswapV4State,
            velodrome_slipstreams::state::VelodromeSlipstreamsState,
            vm::state::EVMPoolState,
        },
        stream::ProtocolStreamBuilder,
    },
    protocol::models::Update,
};

/// The protocols a chain's stream carries, as the integration test lists them.
pub fn default_protocols(chain: Chain) -> Vec<String> {
    match chain {
        Chain::Ethereum => vec![
            "uniswap_v2".to_string(),
            "sushiswap_v2".to_string(),
            "pancakeswap_v2".to_string(),
            "uniswap_v3".to_string(),
            "pancakeswap_v3".to_string(),
            "vm:balancer_v2".to_string(),
            "uniswap_v4".to_string(),
            "ekubo_v2".to_string(),
            "vm:curve".to_string(),
            "uniswap_v4_hooks".to_string(),
            "vm:maverick_v2".to_string(),
            "fluid_v1".to_string(),
            "erc4626".to_string(),
            "cowamm".to_string(),
            "ekubo_v3".to_string(),
            "rocketpool".to_string(),
            "sky".to_string(),
            "lido_v4".to_string(),
            "etherfi".to_string(),
            "vm:liquidityparty".to_string(),
            "vm:fermiswap".to_string(),
            "vm:bopamm".to_string(),
            "ring_swap_v2".to_string(),
            "vm:balancer_v3".to_string(),
        ],
        Chain::Base => vec![
            "uniswap_v2".to_string(),
            "sushiswap_v2".to_string(),
            "uniswap_v3".to_string(),
            "uniswap_v4".to_string(),
            "pancakeswap_v3".to_string(),
            "aerodrome_slipstreams".to_string(),
            "aerodrome_v1".to_string(),
            "lunarbase".to_string(),
        ],
        Chain::Bsc => vec![
            "uniswap_v2".to_string(),
            "uniswap_v3".to_string(),
            "uniswap_v4".to_string(),
            "pancakeswap_v2".to_string(),
            "pancakeswap_v3".to_string(),
            "ring_swap_v2".to_string(),
        ],
        Chain::Unichain => {
            vec![
                "uniswap_v2".to_string(),
                "uniswap_v3".to_string(),
                "uniswap_v4".to_string(),
                "uniswap_v4_hooks".to_string(),
                "velodrome_slipstreams".to_string(),
                // "vm:curve".to_string(), // Temporarily disabled due to indexing issues
            ]
        }
        Chain::Polygon => {
            vec![
                "uniswap_v2".to_string(),
                "uniswap_v3".to_string(),
                "uniswap_v4".to_string(),
                "quickswap_v2".to_string(),
                "ramses_v3".to_string(),
            ]
        }
        Chain::Robinhood => {
            vec![
                "uniswap_v2".to_string(),
                "uniswap_v3".to_string(),
                "uniswap_v4".to_string(),
                "uniswap_v4_hooks".to_string(),
                "sushiswap_v3".to_string(),
                "robinswap_v3".to_string(),
                "gigadex_v3".to_string(),
                "ramses_v3".to_string(),
                "ekubo_v3".to_string(),
                "up_v3".to_string(),
            ]
        }
        Chain::Arbitrum => {
            vec![
                "uniswap_v2".to_string(),
                "uniswap_v3".to_string(),
                "uniswap_v4".to_string(),
                "pancakeswap_v3".to_string(),
            ]
        }
        _ => vec![],
    }
}

/// Builds the decoded Tycho stream for `protocols`, keeping components above `tvl_threshold`.
///
/// # Errors
/// Fails for a protocol this gate cannot decode, or if the stream cannot be built.
pub async fn build(
    tycho_url: &str,
    api_key: &str,
    chain: Chain,
    protocols: &[String],
    tvl_threshold: f64,
    tokens: &HashMap<Bytes, Token>,
) -> miette::Result<impl Stream<Item = Result<Update, StreamDecodeError>>> {
    let mut builder = ProtocolStreamBuilder::new(tycho_url, chain);
    let tvl = ComponentFilter::with_tvl_range(tvl_threshold / 2.0, tvl_threshold);
    for protocol in protocols {
        builder = add_protocol(builder, protocol, &tvl)?;
    }
    let retries = RetryConfiguration::constant(u64::MAX, Duration::from_secs(3));
    builder
        .auth_key(Some(api_key.to_string()))
        .skip_state_decode_failures(true)
        .startup_timeout(Duration::from_secs(1000))
        .websocket_retry_config(&RetryConfiguration::constant(u64::MAX, Duration::from_secs(1)))
        .state_synchronizer_retry_config(&retries)
        .set_tokens(tokens.clone())
        .await
        .build()
        .await
        .into_diagnostic()
        .wrap_err("Failed to build the Tycho stream")
}

/// Registers one protocol's decoder, with the pool filters the integration test uses.
fn add_protocol(
    mut stream: ProtocolStreamBuilder,
    protocol: &str,
    tvl_filter: &ComponentFilter,
) -> miette::Result<ProtocolStreamBuilder> {
    match protocol {
        "uniswap_v2" => {
            stream = stream.exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None);
        }
        "sushiswap_v2" => {
            stream = stream.exchange::<UniswapV2State>("sushiswap_v2", tvl_filter.clone(), None);
        }
        "pancakeswap_v2" => {
            stream =
                stream.exchange::<PancakeswapV2State>("pancakeswap_v2", tvl_filter.clone(), None);
        }
        "uniswap_v3" => {
            stream = stream.exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None);
        }
        "sushiswap_v3" => {
            stream = stream.exchange::<UniswapV3State>("sushiswap_v3", tvl_filter.clone(), None);
        }
        "robinswap_v3" => {
            stream = stream.exchange::<UniswapV3State>("robinswap_v3", tvl_filter.clone(), None);
        }
        "gigadex_v3" => {
            stream = stream.exchange::<UniswapV3State>("gigadex_v3", tvl_filter.clone(), None);
        }
        "pancakeswap_v3" => {
            stream = stream.exchange::<UniswapV3State>("pancakeswap_v3", tvl_filter.clone(), None);
        }
        "vm:balancer_v2" => {
            stream = stream.exchange::<EVMPoolState<PreCachedDB>>(
                "vm:balancer_v2",
                tvl_filter.clone(),
                Some(balancer_v2_pool_filter),
            );
        }
        "uniswap_v4" => {
            stream = stream.exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None);
        }
        "ekubo_v2" => {
            stream = stream.exchange::<EkuboState>("ekubo_v2", tvl_filter.clone(), None);
        }
        "ekubo_v3" => {
            stream = stream.exchange::<EkuboV3State>(
                "ekubo_v3",
                tvl_filter.clone(),
                Some(ekubo_v3_extension_filter),
            );
        }
        "vm:curve" => {
            stream =
                stream.exchange::<CurveState>("vm:curve", tvl_filter.clone(), Some(curve_filter));
        }
        "uniswap_v4_hooks" => {
            stream =
                stream.exchange::<UniswapV4State>("uniswap_v4_hooks", tvl_filter.clone(), None);
        }
        "vm:maverick_v2" => {
            stream = stream.exchange::<EVMPoolState<PreCachedDB>>(
                "vm:maverick_v2",
                tvl_filter.clone(),
                None,
            );
        }
        "fluid_v1" => {
            stream = stream.exchange::<FluidV1>(
                "fluid_v1",
                tvl_filter.clone(),
                Some(fluid_v1_paused_pools_filter),
            );
        }
        "aerodrome_slipstreams" => {
            stream = stream.exchange::<AerodromeSlipstreamsState>(
                "aerodrome_slipstreams",
                tvl_filter.clone(),
                None,
            );
        }
        "erc4626" => {
            stream = stream.exchange::<ERC4626State>(
                "erc4626",
                tvl_filter.clone(),
                Some(erc4626_filter),
            );
        }
        "velodrome_slipstreams" => {
            stream = stream.exchange::<VelodromeSlipstreamsState>(
                "velodrome_slipstreams",
                tvl_filter.clone(),
                None,
            );
        }
        // UP on Robinhood Chain deploys the Slipstream contracts verbatim, so it decodes
        // with the same state as Aerodrome.
        "up_v3" => {
            stream =
                stream.exchange::<AerodromeSlipstreamsState>("up_v3", tvl_filter.clone(), None);
        }
        "ramses_v3" => {
            stream = stream.exchange::<RamsesV3State>("ramses_v3", tvl_filter.clone(), None);
        }
        "rocketpool" => {
            stream = stream.exchange::<RocketpoolState>("rocketpool", tvl_filter.clone(), None);
        }
        "sky" => {
            stream = stream.exchange::<SkyState>("sky", tvl_filter.clone(), None);
        }
        "lido_v4" => {
            stream = stream.exchange::<LidoV4State>("lido_v4", tvl_filter.clone(), None);
        }
        "etherfi" => {
            stream = stream.exchange::<EtherfiState>("etherfi", tvl_filter.clone(), None);
        }
        "cowamm" => {
            stream = stream.exchange::<CowAMMState>("cowamm", tvl_filter.clone(), None);
        }
        "vm:liquidityparty" => {
            stream = stream.exchange::<EVMPoolState<PreCachedDB>>(
                "vm:liquidityparty",
                tvl_filter.clone(),
                Some(liquidityparty_killed_pools_filter),
            );
        }
        "quickswap_v2" => {
            stream = stream.exchange::<UniswapV2State>("quickswap_v2", tvl_filter.clone(), None);
        }
        "aerodrome_v1" => {
            stream = stream.exchange::<AerodromeV1State>("aerodrome_v1", tvl_filter.clone(), None);
        }
        "vm:fermiswap" => {
            stream = stream.exchange::<EVMPoolState<PreCachedDB>>(
                "vm:fermiswap",
                tvl_filter.clone(),
                None,
            );
        }
        "vm:bopamm" => {
            stream =
                stream.exchange::<EVMPoolState<PreCachedDB>>("vm:bopamm", tvl_filter.clone(), None);
        }
        "lunarbase" => {
            stream = stream.exchange::<LunarBaseState>("lunarbase", tvl_filter.clone(), None);
        }
        "ring_swap_v2" => {
            stream = stream.exchange::<RingSwapV2State>("ring_swap_v2", tvl_filter.clone(), None);
        }
        "vm:balancer_v3" => {
            stream = stream.exchange::<BalancerV3State>("vm:balancer_v3", tvl_filter.clone(), None);
        }
        _ => {
            return Err(miette::miette!("Unknown protocol: {}", protocol));
        }
    }
    Ok(stream)
}
