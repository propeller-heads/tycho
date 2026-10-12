use std::{collections::HashMap, time::Duration};

use futures::Stream;
use miette::{miette, IntoDiagnostic, WrapErr};
use tycho_client::{feed::component_tracker::ComponentFilter, stream::RetryConfiguration};
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};
use tycho_simulation::{
    evm::{
        decoder::StreamDecodeError,
        protocol::{
            aerodrome_slipstreams::state::AerodromeSlipstreamsState,
            aerodrome_v1::state::AerodromeV1State, lunarbase::LunarBaseState,
            uniswap_v2::state::UniswapV2State, uniswap_v3::state::UniswapV3State,
            uniswap_v4::state::UniswapV4State,
        },
        stream::ProtocolStreamBuilder,
    },
    protocol::models::Update,
};

/// The native protocols a chain's stream carries, as the integration test lists them.
pub fn default_protocols(chain: Chain) -> Vec<String> {
    let protocols: &[&str] = match chain {
        Chain::Base => &[
            "uniswap_v2",
            "sushiswap_v2",
            "uniswap_v3",
            "pancakeswap_v3",
            "uniswap_v4",
            "aerodrome_v1",
            "aerodrome_slipstreams",
            "lunarbase",
        ],
        _ => &["uniswap_v2", "uniswap_v3", "uniswap_v4"],
    };
    protocols
        .iter()
        .map(|p| p.to_string())
        .collect()
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
        builder = match protocol.as_str() {
            "uniswap_v2" | "sushiswap_v2" => {
                builder.exchange::<UniswapV2State>(protocol, tvl.clone(), None)
            }
            "uniswap_v3" | "pancakeswap_v3" => {
                builder.exchange::<UniswapV3State>(protocol, tvl.clone(), None)
            }
            "uniswap_v4" => builder.exchange::<UniswapV4State>(protocol, tvl.clone(), None),
            "aerodrome_v1" => builder.exchange::<AerodromeV1State>(protocol, tvl.clone(), None),
            "aerodrome_slipstreams" => {
                builder.exchange::<AerodromeSlipstreamsState>(protocol, tvl.clone(), None)
            }
            "lunarbase" => builder.exchange::<LunarBaseState>(protocol, tvl.clone(), None),
            other => return Err(miette!("native-gate has no decoder registered for {other}")),
        };
    }
    let retries = RetryConfiguration::constant(u64::MAX, Duration::from_secs(3));
    builder
        .auth_key(Some(api_key.to_string()))
        .skip_state_decode_failures(true)
        .startup_timeout(Duration::from_secs(600))
        .websocket_retry_config(&RetryConfiguration::constant(u64::MAX, Duration::from_secs(1)))
        .state_synchronizer_retry_config(&retries)
        .set_tokens(tokens.clone())
        .await
        .build()
        .await
        .into_diagnostic()
        .wrap_err("Failed to build the Tycho stream")
}
