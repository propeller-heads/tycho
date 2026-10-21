use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};

use futures::{Stream, StreamExt};
use miette::{miette, IntoDiagnostic, WrapErr};
use tokio::{sync::mpsc::Sender, task::JoinHandle};
use tracing::{info, warn};
use tycho_client::{feed::component_tracker::ComponentFilter, stream::RetryConfiguration};
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};
use tycho_simulation::{
    evm::{
        decoder::StreamDecodeError,
        engine_db::tycho_db::PreCachedDB,
        override_stream::StateOverrideProvider,
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
            pancakeswap_infinity_bin::state::PancakeswapInfinityBinState,
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

use crate::stream_processor::{StreamUpdate, StreamUpdatePayload};

/// A TVL threshold, in native token units, for one protocol system. Parsed from
/// `protocol=threshold`, e.g. `lunarbase=10`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProtocolTvlThreshold {
    pub protocol: String,
    pub threshold: f64,
}

impl FromStr for ProtocolTvlThreshold {
    type Err = String;

    /// Parses `protocol=threshold`. Fails if the `=` or the protocol name is missing, or if the
    /// threshold is not a finite, non-negative number.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let Some((protocol, threshold)) = s.split_once('=') else {
            return Err(format!("expected `protocol=threshold`, got `{s}`"));
        };
        let protocol = protocol.trim();
        if protocol.is_empty() {
            return Err(format!("missing protocol name in `{s}`"));
        }
        let threshold: f64 = threshold
            .trim()
            .parse()
            .map_err(|e| format!("invalid threshold in `{s}`: {e}"))?;
        if !threshold.is_finite() || threshold < 0.0 {
            return Err(format!("threshold in `{s}` must be a finite, non-negative number"));
        }
        Ok(Self { protocol: protocol.to_string(), threshold })
    }
}

/// Returns the overridden TVL threshold of each protocol in `overrides`, keyed by protocol.
///
/// # Errors
/// Returns an error if `overrides` names a protocol twice, or names a protocol that is not in
/// `enabled`. An override for a protocol that is not streamed would otherwise have no effect.
fn validated_tvl_overrides<'a>(
    overrides: &'a [ProtocolTvlThreshold],
    enabled: &[String],
) -> miette::Result<HashMap<&'a str, f64>> {
    let mut thresholds = HashMap::new();
    for entry in overrides {
        if !enabled.contains(&entry.protocol) {
            return Err(miette!(
                "TVL threshold set for protocol `{}`, which is not enabled. Enabled protocols: {:?}",
                entry.protocol,
                enabled
            ));
        }
        if thresholds
            .insert(entry.protocol.as_str(), entry.threshold)
            .is_some()
        {
            return Err(miette!("TVL threshold set twice for protocol `{}`", entry.protocol));
        }
    }
    Ok(thresholds)
}

pub struct ProtocolStreamProcessor {
    chain: Chain,
    tycho_url: String,
    tycho_api_key: String,
    tvl_threshold: f64,
    tvl_buffer_ratio: f64,
    /// Per-protocol thresholds that replace `tvl_threshold` for the named protocols.
    protocol_tvl_thresholds: Vec<ProtocolTvlThreshold>,
    protocols: Option<Vec<String>>,
    partial_blocks: bool,
    no_tls: bool,
    /// State override providers registered on the stream, keyed by protocol system. Empty leaves
    /// the builder to install its own defaults.
    override_providers: HashMap<String, Arc<dyn StateOverrideProvider>>,
}

impl ProtocolStreamProcessor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain: Chain,
        tycho_url: String,
        tycho_api_key: String,
        tvl_threshold: f64,
        tvl_buffer_ratio: f64,
        protocols: Option<Vec<String>>,
        partial_blocks: bool,
        no_tls: bool,
    ) -> miette::Result<Self> {
        Ok(Self {
            chain,
            tycho_url,
            tycho_api_key,
            tvl_threshold,
            tvl_buffer_ratio,
            protocol_tvl_thresholds: Vec::new(),
            protocols,
            partial_blocks,
            no_tls,
            override_providers: HashMap::new(),
        })
    }

    /// Sets per-protocol TVL thresholds. Each one replaces the global threshold for its protocol
    /// and keeps the same buffer ratio. Building the stream fails if an entry names a protocol
    /// that is not enabled, or names a protocol twice.
    pub fn with_protocol_tvl_thresholds(mut self, thresholds: Vec<ProtocolTvlThreshold>) -> Self {
        self.protocol_tvl_thresholds = thresholds;
        self
    }

    /// Registers `providers` as the pools' state override sources, taking precedence over the
    /// builder's own defaults so the process opens one Titan connection rather than one per
    /// consumer.
    pub fn with_override_providers(
        mut self,
        providers: HashMap<String, Arc<dyn StateOverrideProvider>>,
    ) -> Self {
        self.override_providers = providers;
        self
    }

    pub async fn run_stream(
        &self,
        all_tokens: &HashMap<Bytes, Token>,
        tx: Sender<miette::Result<StreamUpdate>>,
    ) -> miette::Result<JoinHandle<()>> {
        info!("Starting protocol stream processor for chain {:?}", self.chain);
        let stream = self.build_stream(all_tokens).await?;
        let handle = tokio::spawn(async move {
            info!("Protocol stream processor started");
            tokio::pin!(stream);
            let mut is_first_update = true;
            while let Some(res) = stream.next().await {
                let update = match res {
                    Ok(msg) => msg,
                    Err(e) => {
                        if tx
                            .send(Err(
                                miette!(e).wrap_err("Error receiving message from protocol stream")
                            ))
                            .await
                            .is_err()
                        {
                            warn!("Receiver dropped, stopping stream processor");
                            break;
                        }
                        continue;
                    }
                };
                let received_at =
                    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                        Ok(duration) => duration,
                        Err(e) => {
                            if tx
                                .send(Err(miette!(e).wrap_err("Error getting current timestamp")))
                                .await
                                .is_err()
                            {
                                warn!("Receiver dropped, stopping stream processor");
                                break;
                            }
                            continue;
                        }
                    };
                let update = StreamUpdate {
                    payload: StreamUpdatePayload::Protocol(update),
                    is_first_update,
                    received_at,
                };
                if is_first_update {
                    is_first_update = false;
                }
                if tx.send(Ok(update)).await.is_err() {
                    warn!("Receiver dropped, stopping stream processor");
                    break;
                }
            }
        });
        Ok(handle)
    }

    async fn build_stream(
        &self,
        all_tokens: &HashMap<Bytes, Token>,
    ) -> miette::Result<impl Stream<Item = Result<Update, StreamDecodeError>>> {
        let mut protocol_stream = ProtocolStreamBuilder::new(&self.tycho_url, self.chain);

        let protocols_to_enable = match &self.protocols {
            Some(protocols) => protocols.clone(),
            None => self.get_default_protocols_for_chain(),
        };
        info!("Building protocol stream for protocols: {:?}", protocols_to_enable);
        let tvl_overrides =
            validated_tvl_overrides(&self.protocol_tvl_thresholds, &protocols_to_enable)?;
        if !tvl_overrides.is_empty() {
            info!("TVL threshold overrides: {:?}", tvl_overrides);
        }

        for protocol in &protocols_to_enable {
            let threshold = tvl_overrides
                .get(protocol.as_str())
                .copied()
                .unwrap_or(self.tvl_threshold);
            let tvl_filter =
                ComponentFilter::with_tvl_range(threshold / self.tvl_buffer_ratio, threshold);
            protocol_stream =
                self.add_protocol_to_stream(protocol_stream, protocol, &tvl_filter)?;
        }
        for (protocol_system, provider) in &self.override_providers {
            protocol_stream =
                protocol_stream.with_override_provider(protocol_system, provider.clone());
        }
        if self.partial_blocks {
            protocol_stream = protocol_stream.enable_partial_blocks();
        }
        // 1 s cooldown: must stay below the state-sync cooldown (≥ 2 s on every chain)
        // so synchronizers don't exhaust their retries while the WS is reconnecting.
        let infinite_ws_retries = RetryConfiguration::constant(u64::MAX, Duration::from_secs(1));
        let infinite_sync_retries = RetryConfiguration::constant(u64::MAX, Duration::from_secs(3));
        protocol_stream
            .auth_key(Some(self.tycho_api_key.clone()))
            .no_tls(self.no_tls)
            .skip_state_decode_failures(true)
            .startup_timeout(Duration::from_secs(1000))
            .websocket_retry_config(&infinite_ws_retries)
            .state_synchronizer_retry_config(&infinite_sync_retries)
            .set_tokens(all_tokens.clone())
            .await
            .build()
            .await
            .into_diagnostic()
            .wrap_err("Failed building protocol stream")
    }

    fn get_default_protocols_for_chain(&self) -> Vec<String> {
        match self.chain {
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
                "pancakeswap_infinity_cl".to_string(),
                "pancakeswap_infinity_bin".to_string(),
            ],
            Chain::Bsc => vec![
                "uniswap_v2".to_string(),
                "uniswap_v3".to_string(),
                "uniswap_v4".to_string(),
                "pancakeswap_v2".to_string(),
                "pancakeswap_v3".to_string(),
                "ring_swap_v2".to_string(),
                "pancakeswap_infinity_cl".to_string(),
                "lunarbase".to_string(),
                "pancakeswap_infinity_bin".to_string(),
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
                    "pancakeswap_infinity_cl".to_string(),
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

    /// Add a specific protocol to the stream builder
    fn add_protocol_to_stream(
        &self,
        mut stream: ProtocolStreamBuilder,
        protocol: &str,
        tvl_filter: &ComponentFilter,
    ) -> miette::Result<ProtocolStreamBuilder> {
        match protocol {
            "uniswap_v2" => {
                stream = stream.exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None);
            }
            "sushiswap_v2" => {
                stream =
                    stream.exchange::<UniswapV2State>("sushiswap_v2", tvl_filter.clone(), None);
            }
            "pancakeswap_v2" => {
                stream = stream.exchange::<PancakeswapV2State>(
                    "pancakeswap_v2",
                    tvl_filter.clone(),
                    None,
                );
            }
            "uniswap_v3" => {
                stream = stream.exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None);
            }
            "sushiswap_v3" => {
                stream =
                    stream.exchange::<UniswapV3State>("sushiswap_v3", tvl_filter.clone(), None);
            }
            "robinswap_v3" => {
                stream =
                    stream.exchange::<UniswapV3State>("robinswap_v3", tvl_filter.clone(), None);
            }
            "gigadex_v3" => {
                stream = stream.exchange::<UniswapV3State>("gigadex_v3", tvl_filter.clone(), None);
            }
            "pancakeswap_v3" => {
                stream =
                    stream.exchange::<UniswapV3State>("pancakeswap_v3", tvl_filter.clone(), None);
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
            "pancakeswap_infinity_cl" => {
                stream = stream.exchange::<UniswapV4State>(
                    "pancakeswap_infinity_cl",
                    tvl_filter.clone(),
                    None,
                );
            }
            "pancakeswap_infinity_bin" => {
                stream = stream.exchange::<PancakeswapInfinityBinState>(
                    "pancakeswap_infinity_bin",
                    tvl_filter.clone(),
                    None,
                );
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
                stream = stream.exchange::<CurveState>(
                    "vm:curve",
                    tvl_filter.clone(),
                    Some(curve_filter),
                );
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
                stream =
                    stream.exchange::<UniswapV2State>("quickswap_v2", tvl_filter.clone(), None);
            }
            "aerodrome_v1" => {
                stream =
                    stream.exchange::<AerodromeV1State>("aerodrome_v1", tvl_filter.clone(), None);
            }
            "vm:fermiswap" => {
                stream = stream.exchange::<EVMPoolState<PreCachedDB>>(
                    "vm:fermiswap",
                    tvl_filter.clone(),
                    None,
                );
            }
            "vm:bopamm" => {
                stream = stream.exchange::<EVMPoolState<PreCachedDB>>(
                    "vm:bopamm",
                    tvl_filter.clone(),
                    None,
                );
            }
            "lunarbase" => {
                stream = stream.exchange::<LunarBaseState>("lunarbase", tvl_filter.clone(), None);
            }
            "ring_swap_v2" => {
                stream =
                    stream.exchange::<RingSwapV2State>("ring_swap_v2", tvl_filter.clone(), None);
            }
            "vm:balancer_v3" => {
                stream =
                    stream.exchange::<BalancerV3State>("vm:balancer_v3", tvl_filter.clone(), None);
            }
            _ => {
                return Err(miette::miette!("Unknown protocol: {}", protocol));
            }
        }
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn processor(chain: Chain) -> ProtocolStreamProcessor {
        ProtocolStreamProcessor::new(
            chain,
            "http://localhost".to_string(),
            "key".to_string(),
            0.0,
            1.0,
            None,
            false,
            false,
        )
        .unwrap()
    }

    #[test]
    fn robinhood_defaults_include_uniswap_v4_hooks() {
        let defaults = processor(Chain::Robinhood).get_default_protocols_for_chain();
        assert!(
            defaults.contains(&"uniswap_v4_hooks".to_string()),
            "Robinhood defaults must include uniswap_v4_hooks: {defaults:?}"
        );
    }

    #[test]
    fn bsc_defaults_include_lunarbase() {
        let defaults = processor(Chain::Bsc).get_default_protocols_for_chain();
        assert!(
            defaults.contains(&"lunarbase".to_string()),
            "BSC defaults must include lunarbase: {defaults:?}"
        );
    }

    fn threshold(protocol: &str, threshold: f64) -> ProtocolTvlThreshold {
        ProtocolTvlThreshold { protocol: protocol.to_string(), threshold }
    }

    fn enabled(protocols: &[&str]) -> Vec<String> {
        protocols
            .iter()
            .map(|p| p.to_string())
            .collect()
    }

    #[rstest::rstest]
    #[case::integer("lunarbase=10", "lunarbase", 10.0)]
    #[case::fraction("vm:curve=0.5", "vm:curve", 0.5)]
    #[case::zero("lunarbase=0", "lunarbase", 0.0)]
    #[case::whitespace(" lunarbase = 3 ", "lunarbase", 3.0)]
    fn parses_protocol_tvl_threshold(
        #[case] input: &str,
        #[case] protocol: &str,
        #[case] expected: f64,
    ) {
        assert_eq!(input.parse::<ProtocolTvlThreshold>(), Ok(threshold(protocol, expected)));
    }

    #[rstest::rstest]
    #[case::empty("")]
    #[case::no_separator("lunarbase")]
    #[case::no_protocol("=10")]
    #[case::no_threshold("lunarbase=")]
    #[case::not_a_number("lunarbase=ten")]
    #[case::negative("lunarbase=-1")]
    #[case::nan("lunarbase=NaN")]
    #[case::infinite("lunarbase=inf")]
    fn rejects_malformed_protocol_tvl_threshold(#[case] input: &str) {
        assert!(input
            .parse::<ProtocolTvlThreshold>()
            .is_err());
    }

    #[test]
    fn validated_tvl_overrides_returns_only_overridden_protocols() {
        let overrides = [threshold("lunarbase", 10.0), threshold("pancakeswap_infinity_cl", 3.0)];
        let thresholds = validated_tvl_overrides(
            &overrides,
            &enabled(&["uniswap_v3", "lunarbase", "pancakeswap_infinity_cl"]),
        )
        .unwrap();

        assert_eq!(
            thresholds,
            HashMap::from([("lunarbase", 10.0), ("pancakeswap_infinity_cl", 3.0)])
        );
    }

    #[test]
    fn validated_tvl_overrides_accepts_no_overrides() {
        let thresholds = validated_tvl_overrides(&[], &enabled(&["uniswap_v3"])).unwrap();

        assert!(thresholds.is_empty());
    }

    #[test]
    fn validated_tvl_overrides_rejects_protocol_that_is_not_enabled() {
        let overrides = [threshold("lunarbase", 10.0)];

        let err = validated_tvl_overrides(&overrides, &enabled(&["uniswap_v3"])).unwrap_err();

        assert!(err.to_string().contains("lunarbase"), "{err}");
    }

    #[test]
    fn validated_tvl_overrides_rejects_duplicate_protocol() {
        let overrides = [threshold("lunarbase", 10.0), threshold("lunarbase", 3.0)];

        let err = validated_tvl_overrides(&overrides, &enabled(&["lunarbase"])).unwrap_err();

        assert!(err.to_string().contains("twice"), "{err}");
    }
}
