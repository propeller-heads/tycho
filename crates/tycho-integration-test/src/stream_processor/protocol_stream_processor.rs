use std::{collections::HashMap, time::Duration};

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
        protocol::{
            aerodrome_slipstreams::state::AerodromeSlipstreamsState,
            cowamm::state::CowAMMState,
            ekubo::state::EkuboState,
            ekubo_v3::{self, state::EkuboV3State},
            erc4626::state::ERC4626State,
            filters::{balancer_v2_pool_filter, erc4626_filter, fluid_v1_paused_pools_filter},
            fluid::FluidV1,
            pancakeswap_v2::state::PancakeswapV2State,
            rocketpool::state::RocketpoolState,
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

use crate::stream_processor::{StreamUpdate, UpdateType};

pub struct ProtocolStreamProcessor {
    chain: Chain,
    tycho_url: String,
    tycho_api_key: String,
    tvl_threshold: f64,
    tvl_buffer_ratio: f64,
    protocols: Option<Vec<String>>,
    partial_blocks: bool,
}

impl ProtocolStreamProcessor {
    pub fn new(
        chain: Chain,
        tycho_url: String,
        tycho_api_key: String,
        tvl_threshold: f64,
        tvl_buffer_ratio: f64,
        protocols: Option<Vec<String>>,
        partial_blocks: bool,
    ) -> miette::Result<Self> {
        Ok(Self {
            chain,
            tycho_url,
            tycho_api_key,
            tvl_threshold,
            tvl_buffer_ratio,
            protocols,
            partial_blocks,
        })
    }

    pub async fn run_stream(
        &self,
        all_tokens: &HashMap<Bytes, Token>,
        tx: Sender<miette::Result<StreamUpdate>>,
    ) -> miette::Result<JoinHandle<()>> {
        info!("Starting protocol stream processor for chain {:?}", self.chain);
        let mut stream = self.build_stream(all_tokens).await?;
        let handle = tokio::spawn(async move {
            info!("Protocol stream processor started");
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
                    update_type: UpdateType::Protocol,
                    update,
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
        let tvl_filter = ComponentFilter::with_tvl_range(
            self.tvl_threshold / self.tvl_buffer_ratio,
            self.tvl_threshold,
        );

        let protocols_to_enable = match &self.protocols {
            Some(protocols) => protocols.clone(),
            None => self.get_default_protocols_for_chain(),
        };
        info!("Building protocol stream for protocols: {:?}", protocols_to_enable);

        for protocol in &protocols_to_enable {
            protocol_stream =
                self.add_protocol_to_stream(protocol_stream, protocol, &tvl_filter)?;
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
                "vm:liquidityparty".to_string(),
            ],
            Chain::Base => vec![
                "uniswap_v2".to_string(),
                "uniswap_v3".to_string(),
                "uniswap_v4".to_string(),
                "pancakeswap_v3".to_string(),
                "aerodrome_slipstreams".to_string(),
            ],
            Chain::Bsc => vec![
                "uniswap_v2".to_string(),
                "uniswap_v3".to_string(),
                "uniswap_v4".to_string(),
                "pancakeswap_v2".to_string(),
                "pancakeswap_v3".to_string(),
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
            "ekubo_v2" => {
                stream = stream.exchange::<EkuboState>("ekubo_v2", tvl_filter.clone(), None);
            }
            "ekubo_v3" => {
                stream = stream.exchange::<EkuboV3State>(
                    "ekubo_v3",
                    tvl_filter.clone(),
                    Some(ekubo_v3::filter_fn),
                );
            }
            "vm:curve" => {
                stream = stream.exchange::<EVMPoolState<PreCachedDB>>(
                    "vm:curve",
                    tvl_filter.clone(),
                    None,
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
            "rocketpool" => {
                stream = stream.exchange::<RocketpoolState>("rocketpool", tvl_filter.clone(), None);
            }
            "cowamm" => {
                stream = stream.exchange::<CowAMMState>("cowamm", tvl_filter.clone(), None);
            }
            "vm:liquidityparty" => {
                stream = stream.exchange::<EVMPoolState<PreCachedDB>>(
                    "vm:liquidityparty",
                    tvl_filter.clone(),
                    None,
                );
            }
            "quickswap_v2" => {
                stream =
                    stream.exchange::<UniswapV2State>("quickswap_v2", tvl_filter.clone(), None);
            }
            _ => {
                return Err(miette::miette!("Unknown protocol: {}", protocol));
            }
        }
        Ok(stream)
    }
}
