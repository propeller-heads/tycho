extern crate tycho_simulation;

mod ui;
pub mod utils;

use std::{env, str::FromStr};

use clap::Parser;
use futures::{future::select_all, StreamExt};
use tokio::{sync::mpsc, task::JoinHandle};
use tycho_client::feed::component_tracker::ComponentFilter;
use tycho_common::{dto::TvlThresholdTier, models::Chain};
use tycho_simulation::{
    evm::{
        engine_db::tycho_db::PreCachedDB,
        protocol::{
            ekubo::state::EkuboState,
            ekubo_v3::{self, state::EkuboV3State},
            filters::{balancer_v2_pool_filter, curve_pool_filter},
            pancakeswap_v2::state::PancakeswapV2State,
            uniswap_v2::state::UniswapV2State,
            uniswap_v3::state::UniswapV3State,
            uniswap_v4::state::UniswapV4State,
            vm::state::EVMPoolState,
        },
        stream::ProtocolStreamBuilder,
    },
    protocol::models::Update,
    utils::{get_default_url, load_all_tokens},
};

#[derive(Parser)]
struct Cli {
    /// The tvl threshold to filter the graph by. Defaults to a chain-appropriate
    /// value targeting ~$200K USD equivalent.
    #[arg(long)]
    tvl_threshold: Option<f64>,
    /// The target blockchain
    #[clap(long, default_value = "ethereum")]
    pub chain: String,
}

fn register_exchanges(
    mut builder: ProtocolStreamBuilder,
    chain: &Chain,
    tvl_filter: ComponentFilter,
) -> ProtocolStreamBuilder {
    match chain {
        Chain::Ethereum => {
            builder = builder
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV2State>("sushiswap_v2", tvl_filter.clone(), None)
                .exchange::<PancakeswapV2State>("pancakeswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("pancakeswap_v3", tvl_filter.clone(), None)
                .exchange::<EVMPoolState<PreCachedDB>>(
                    "vm:balancer_v2",
                    tvl_filter.clone(),
                    Some(balancer_v2_pool_filter),
                )
                .exchange::<EVMPoolState<PreCachedDB>>(
                    "vm:curve",
                    tvl_filter.clone(),
                    Some(curve_pool_filter),
                )
                .exchange::<EkuboState>("ekubo_v2", tvl_filter.clone(), None)
                .exchange::<EkuboV3State>("ekubo_v3", tvl_filter.clone(), Some(ekubo_v3::filter_fn))
                .exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4_hooks", tvl_filter.clone(), None);
            // COMING SOON!
            // .exchange::<EVMPoolState<PreCachedDB>>("vm:maverick_v2", tvl_filter.clone(), None)
        }
        Chain::Base => {
            builder = builder
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
        }
        Chain::Bsc => {
            builder = builder
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None)
                .exchange::<PancakeswapV2State>("pancakeswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("pancakeswap_v3", tvl_filter.clone(), None)
        }
        Chain::Unichain => {
            builder = builder
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None)
        }
        _ => {}
    }
    builder
}

#[tokio::main]
async fn main() {
    utils::setup_tracing();
    // Parse command-line arguments into a Cli struct
    let cli = Cli::parse();

    let chain =
        Chain::from_str(&cli.chain).unwrap_or_else(|_| panic!("Unknown chain {}", cli.chain));

    let tycho_url = env::var("TYCHO_URL").unwrap_or_else(|_| {
        get_default_url(&chain).unwrap_or_else(|| panic!("Unknown URL for chain {}", cli.chain))
    });

    let tycho_api_key: String =
        env::var("TYCHO_API_KEY").unwrap_or_else(|_| "sampletoken".to_string());

    // Perform an early check to ensure `RPC_URL` is set.
    // This prevents errors from occurring later during UI interactions for curve.
    if chain == Chain::Ethereum {
        env::var("RPC_URL").expect("RPC_URL env variable should be set");
    }

    // Create communication channels for inter-thread communication
    let (tick_tx, tick_rx) = mpsc::channel::<Update>(12);

    let tycho_message_processor: JoinHandle<anyhow::Result<()>> = tokio::spawn(async move {
        let all_tokens = load_all_tokens(
            tycho_url.as_str(),
            false,
            Some(tycho_api_key.as_str()),
            true,
            chain,
            None,
            None,
        )
        .await
        .expect("Failed loading tokens");
        let tvl_threshold = cli
            .tvl_threshold
            .unwrap_or_else(|| chain.default_tvl_threshold(TvlThresholdTier::Medium));
        let tvl_filter = ComponentFilter::with_tvl_range(tvl_threshold, tvl_threshold);
        let protocol_stream =
            register_exchanges(ProtocolStreamBuilder::new(&tycho_url, chain), &chain, tvl_filter)
                .auth_key(Some(tycho_api_key.clone()))
                .skip_state_decode_failures(true)
                .set_tokens(all_tokens)
                .await
                .build()
                .await
                .expect("Failed building protocol stream");
        tokio::pin!(protocol_stream);

        // Loop through block updates
        while let Some(msg) = protocol_stream.next().await {
            tick_tx
                .send(msg.unwrap())
                .await
                .expect("Sending tick failed!")
        }
        anyhow::Result::Ok(())
    });

    let terminal = ratatui::init();
    let terminal_app = tokio::spawn(async move {
        ui::App::new(tick_rx)
            .run(terminal)
            .await
    });
    let tasks = [tycho_message_processor, terminal_app];
    let _ = select_all(tasks).await;
    ratatui::restore();
}
