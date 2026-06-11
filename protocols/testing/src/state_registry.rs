use tycho_simulation::{
    evm::{
        engine_db::tycho_db::PreCachedDB,
        protocol::{
            ekubo::state::EkuboState, fluid::FluidV1, lunarbase::LunarBaseTychoState,
            pancakeswap_v2::state::PancakeswapV2State, rocketpool::state::RocketpoolState,
            uniswap_v2::state::UniswapV2State, uniswap_v3::state::UniswapV3State,
            uniswap_v4::state::UniswapV4State, vm::state::EVMPoolState,
        },
        stream::ProtocolStreamBuilder,
    },
    protocol::models::DecoderContext,
    tycho_client::feed::component_tracker::ComponentFilter,
    tycho_common::{dto::TvlThresholdTier, models::Chain},
};

/// Register decoder based on protocol system. Defaults to EVMPoolState.
/// To add a new protocol, just add a case to the match statement.
pub fn register_protocol(
    stream_builder: ProtocolStreamBuilder,
    protocol_system: &str,
    chain: Chain,
    decoder_context: DecoderContext,
) -> miette::Result<ProtocolStreamBuilder> {
    let tvl = chain.default_tvl_threshold(TvlThresholdTier::Medium);
    let tvl_filter = ComponentFilter::with_tvl_range(tvl, tvl);
    let stream_builder = match protocol_system {
        "uniswap_v2" | "sushiswap_v2" => stream_builder
            .exchange_with_decoder_context::<UniswapV2State>(
                protocol_system,
                tvl_filter,
                None,
                decoder_context,
            ),
        "pancakeswap_v2" => stream_builder.exchange_with_decoder_context::<PancakeswapV2State>(
            protocol_system,
            tvl_filter,
            None,
            decoder_context,
        ),
        "uniswap_v3" | "pancakeswap_v3" => stream_builder
            .exchange_with_decoder_context::<UniswapV3State>(
                protocol_system,
                tvl_filter,
                None,
                decoder_context,
            ),
        "ekubo_v2" => stream_builder.exchange_with_decoder_context::<EkuboState>(
            protocol_system,
            tvl_filter,
            None,
            decoder_context,
        ),
        "uniswap_v4" | "uniswap_v4_hooks" => stream_builder
            .exchange_with_decoder_context::<UniswapV4State>(
                protocol_system,
                tvl_filter,
                None,
                decoder_context,
            ),
        "fluid_v1" => stream_builder.exchange_with_decoder_context::<FluidV1>(
            protocol_system,
            tvl_filter,
            None,
            decoder_context,
        ),
        "rocketpool" => stream_builder.exchange_with_decoder_context::<RocketpoolState>(
            protocol_system,
            tvl_filter,
            None,
            decoder_context,
        ),
        "lunarbase" => stream_builder.exchange_with_decoder_context::<LunarBaseTychoState>(
            protocol_system,
            tvl_filter,
            None,
            decoder_context,
        ),
        // Default to EVMPoolState for all other protocols
        _ => stream_builder.exchange_with_decoder_context::<EVMPoolState<PreCachedDB>>(
            protocol_system,
            tvl_filter,
            None,
            decoder_context,
        ),
    };

    Ok(stream_builder)
}
