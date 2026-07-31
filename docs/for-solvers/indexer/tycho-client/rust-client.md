# Rust Client

The rust crate provides a flexible library for developers to integrate Tycho’s real-time data into any Rust application.

{% hint style="info" %}
Tycho offers another packaged called Tycho Simulation, which uses Tycho Client to handle data streams and also implements simulations, allowing you to leverage the full power of Tycho. If your goal is to simulate the protocol's behavior, please check our [simulation.md](../../simulation.md "mention") guide.
{% endhint %}

## Setup Guide

To use Tycho Client in Rust, add the following crates to your `Cargo.toml`:

```toml
[dependencies]
tycho-client = "x.y.z"
tycho-common = "x.y.z"
```

{% hint style="info" %}
**Note:** Replace `x.y.z` with the latest version from <a href="https://crates.io/crates/tycho-client" target="_blank" rel="noopener noreferrer">crates.io</a>.
{% endhint %}

**Step 2: Use Tycho-client**

From there it is easy to add a Tycho stream to your rust program like so:

```rust
// Import required dependencies
use tracing_subscriber::EnvFilter;
use tycho_client::{feed::component_tracker::ComponentFilter, stream::TychoStreamBuilder};
use tycho_common::dto::Chain;

/// Example of using the Tycho client to subscribe to exchange data streams
///
/// This example demonstrates how to:
/// 1. Initialize a connection to the Tycho service
/// 2. Set up filters for specific exchanges and pools
/// 3. Receive and process real-time updates
#[tokio::main]
async fn main() {
    // Initialize the tracing subscriber with environment-based filter configuration
    // Set RUST_LOG environment variable (e.g., RUST_LOG=info) to control logging level
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // Create a new Tycho stream for Ethereum blockchain
    // The first returned value is a JoinHandle that we're ignoring here (_)
    let (_, mut receiver) =
        TychoStreamBuilder::new("tycho-beta.propellerheads.xyz", Chain::Ethereum)
            // Set authentication key
            // In production, use environment variable: std::env::var("TYCHO_AUTH_KEY").expect("...")
            .auth_key(Some("your-api-key".into()))
            // Subscribe to Uniswap V2 pools with TVL above 1000 ETH and remove the ones below 900 ETH
            .exchange("uniswap_v2", ComponentFilter::with_tvl_range(900.0, 1000.0))
            // Subscribe to specific Uniswap V3 pools by their pool IDs (contract addresses)
            .exchange(
                "uniswap_v3",
                ComponentFilter::Ids(vec![
                    // Include only these 2 UniswapV3 pools.
                    "0xCBCdF9626bC03E24f779434178A73a0B4bad62eD".to_string(), // USDC/WETH 0.3% pool
                    "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640".to_string(), // USDC/WETH 0.05% pool
                ]),
            )
            // Build the stream client
            .build()
            .await
            .expect("Failed to build tycho stream");

    // Process incoming messages in an infinite loop
    // NOTE: This will continue until the channel is closed or the program is terminated
    while let Some(msg) = receiver.recv().await {
        // Print each received message to stdout
        println!("{:?}", msg);
    }
}
```

You can also use the client to interact with Tycho RPC for fetching static information. For example, you can fetch tokens (available at [#v1-tokens](../tycho-rpc.md#v1-tokens "mention") endpoint) with the following:

```rust
use tycho_client::rpc::HttpRPCClient;
use tycho_common::dto::Chain;

let client = HttpRPCClient::new("insert_tycho_url", Some("my_auth_token"));

let tokens = client
    .get_all_tokens(
        Chain::Ethereum,
        Some(51_i32), // min token quality to filter for certain token types
        Some(30_u64), // number of days since last traded
        1000,         // pagination chunk size
    )
    .await
    .unwrap();
    
/// Token quality is between 0-100, where:
///  - 100: Normal token
///  - 75: Rebase token
///  - 50: Fee token
///  - 10: Token analysis failed at creation
///  - 5: Token analysis failed on cronjob (after creation).
///  - 0: Failed to extract decimals onchain
```
