# Request for Quote Protocols

Request for Quote (RFQ) protocols work differently from on-chain protocols. Instead of reading pool data from the chain, they fetch prices from off-chain market makers via WebSocket or API.

You ask for a quote for a specific trade size, and they return a price. Quotes can be:

* **Indicative** — estimated prices used for simulation.
* **Binding** — firm prices, valid for a short time, used at execution.

Tycho supports streaming, simulating, and executing RFQ quotes as part of multi-protocol swaps.

Currently, Tycho supports the following RFQ protocols:

| Protocol    | Simulation Time | Credentials              |
| ----------- | --------------- | ------------------------ |
| `bebop`     | 0.5 µs          | Required                 |
| `hashflow`  | 0.4 µs          | Required                 |
| `liquorice` | 0.4 µs          | Required                 |
| `native`    | 0.4 µs          | Required                 |
| `metric`    | -               | Required                 |

## Quickstart

The RFQ quickstart is similar to the other protocols [quickstart](../).

See the code <a href="https://github.com/propeller-heads/tycho-indexer/tree/main/crates/tycho-simulation/examples/rfq_quickstart" target="_blank" rel="noopener noreferrer">here</a>. As of now, <a href="https://docs.bebop.xyz/bebop/bebop-api-pmm-rfq/pmm-rfq-api-intro" target="_blank" rel="noopener noreferrer">Bebop</a>, <a href="https://docs.hashflow.com/hashflow/taker/getting-started-api-v3" target="_blank" rel="noopener noreferrer">Hashflow</a>, <a href="https://liquorice.tech/" target="_blank" rel="noopener noreferrer">Liquorice</a>, <a href="https://docs.native.org/" target="_blank" rel="noopener noreferrer">Native</a> and Metric are the only supported providers.

The feed builders take credentials as plain arguments; how you obtain them is up to your application. The example reads them from the environment, so set the ones for the providers you want, plus your private key if you wish to execute against the Tycho Router:

```bash
unset HISTFILE # to not save your credentials to your shell history
export BEBOP_KEY=<your-bebop-api-key>
export HASHFLOW_USER=<your-hashflow-api-username>
export HASHFLOW_KEY=<your-hashflow-api-key>
export LIQUORICE_USER=<your-liquorice-api-username>
export LIQUORICE_KEY=<your-liquorice-api-key>
export NATIVE_API_KEY=<your-native-api-key>
export METRIC_API_KEY=<your-metric-trading-key>
export PRIVATE_KEY=<your-wallet-private-key>
```

Metric's feed talks to Metric's public endpoint (`MetricFeedBuilder::base_url` points it elsewhere) and sends the trading key as the Bearer token the authenticated `bid_ask` endpoint requires. The example registers Metric under the `--run-pamm-protocols` flag, which is on by default.

Then run the example:

```rust
cargo run --release --example rfq_quickstart
```

{% hint style="info" %}
You’ll need to request credentials directly from RFQ providers.
{% endhint %}

### What it does

The quickstart:

* Connects to the RFQ stream and fetches live price updates.
* Simulates the best available amount out for a given pair (default: 10 USDC → WETH on mainnet).
* Encodes the swap and prepares calldata to execute it via the Tycho Router.

If you want to see results for a different token, amount, minimum TVL, or chain, you can set additional flags:

```bash
cargo run --release --example rfq_quickstart -- --sell-token "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913" --buy-token "0x4200000000000000000000000000000000000006" --sell-amount 10 --tvl-threshold 1000 --chain "base"
```

This example would seek the best swap for 10 USDC -> WETH on Base.

### Set up

You’ll need to configure:

* Tycho URL (by default `"tycho-beta.propellerheads.xyz"`)
* Tycho API key
* RFQ API keys (the example's `Readme.md` lists the environment variables it reads for each provider; the library itself takes credentials as builder arguments)
* Private key if you wish to execute the swap against the Tycho Router

To get token information from Tycho Indexer RPC please use [load\_all\_tokens](simulation.md#step-1-fetch-tokens).

### RFQ Feeds

Each RFQ protocol has its own feed type (`BebopFeed`, `HashflowFeed`, `LiquoriceFeed`, `NativeFeed`), built from a shared `CommonConfig` plus provider-specific credentials and options. Every feed implements the `SnapshotFeed` trait — a live price feed: `subscribe()` consumes the feed and returns a watch receiver that always holds the provider's latest complete set of books (as `Option<BookSnapshot<ReceivedAt>>`, `None` until the first snapshot arrives or after the last one was withdrawn as stale), together with the feed future that keeps it fresh. Binding quotes are not part of the feed — the states it emits request them at encoding time through their embedded client.

Example setup for Bebop:

```rust
let common = CommonConfig { chain, tokens: Arc::new(rfq_tokens), min_tvl_usd };
let usd_quote_tokens = usd_stablecoins_for_chain(&chain).expect("chain has curated USD stablecoins");

let bebop_feed = BebopFeedBuilder::new(common.clone(), usd_quote_tokens.clone(), bebop_key)
    .build()
    .expect("Failed to create Bebop feed");
```

`CommonConfig` carries what every provider needs — the chain, the tradable tokens with their metadata, and the minimum book TVL in USD. Each feed builder takes it by value (clone it when building several feeds); the builders' own setters cover only provider-specific options such as credentials, poll cadence, or Bebop's origin fields. To give one provider a different token set or threshold, build it from a different `CommonConfig`.

**Minimum TVL** (`min_tvl_usd`) is specified in USD. This setting filters out token pairs with low liquidity on the RFQ side, helping avoid thin or illiquid quotes.

**USD quote tokens:** The Bebop, Hashflow, Liquorice and Native builders take a set of USD-priced tokens that their feeds normalize book TVL into before applying `min_tvl_usd`. The set is used exclusively for TVL filtering, never for quote requests or trade execution. Metric's API reports USD TVL directly, so its builder takes none.

You should specify USD-priced stablecoins (e.g., USDC, USDT, DAI) as quote tokens, since currently-supported RFQ providers quote most of their currently supported liquidity in USD stablecoins. This ensures the feed calculates TVL accurately when comparing pairs with different quote tokens. For instance, if you receive price levels for an ETH/WBTC pair where WBTC is the quote token, the feed will look up the WBTC price in one of your approved quote tokens (USD stablecoins) to properly calculate the TVL in dollar terms. `usd_stablecoins_for_chain` returns the curated set for Ethereum and Base and `None` elsewhere; an empty set filters out every pair, so always pass a non-empty one.

**Note:** Some RFQ providers may support tokens that Tycho does not. Because execution happens through the Tycho Router, it’s important to ensure that all tokens used in RFQ quotes are also supported by Tycho.

### Stream: Real-Time Price Updates

Each feed publishes its complete set of books as a `BookSnapshot` over a `tokio::sync::watch` channel: one `Book` (component + state, plus the provider's `updated_at` where reported) per pair, anchored by the feed's receipt time (`anchor: ReceivedAt`, a `DateTime<Utc>`). A feed withdraws its snapshot (the watch goes back to `None`) when nobody is refreshing it: an HTTP feed after `max_missed_polls` failed polls in a row, a WebSocket feed once the book is older than `max_book_age`, and either of them when the feed itself ends — because it gave up, or because you dropped its future. Both knobs live on the feed config; set them to `None` to keep the last snapshot until the feed ends. The feed configs have no `Default`: start from the builder's `default_feed_config()`, whose documentation states the values it starts from, and change fields with struct-update syntax. Call `subscribe()`, spawn the returned feed future, and read the receiver. To follow several providers at once, wrap the receivers in a `StreamMap`, and keep the feed futures in a `JoinSet` to observe why a feed stopped:

```rust
let mut feeds: StreamMap<String, WatchStream<Option<BookSnapshot<ReceivedAt>>>> = StreamMap::new();
let mut drivers: JoinSet<(&'static str, Result<(), FeedError>)> = JoinSet::new();
// for each built feed (Bebop shown; the other providers are identical):
let (rx, feed) = bebop_feed.subscribe();
feeds.insert(bebop::PROTOCOL_SYSTEM.to_string(), WatchStream::new(rx));
drivers.spawn(async move { (bebop::PROTOCOL_SYSTEM, feed.await) });

loop {
    tokio::select! {
        Some((protocol_system, snapshot)) = feeds.next() => {
            let Some(snapshot) = snapshot else { continue }; // nothing servable yet
            // snapshot.books is the provider's complete current set of books
        }
        Some(Ok((name, result))) = drivers.join_next() => {
            // the feed stopped: Err(e) means it gave up permanently
        }
        else => break, // every provider terminated
    }
}
```

RFQ snapshots are **timestamped**, not block-based: the snapshot's `anchor` is the feed's receipt time (`ReceivedAt`) and each `Book` carries the provider's `updated_at` where the provider reports one (Bebop, Liquorice, Metric). Every snapshot is the provider's full current state, never a delta: a pair missing from the current snapshot no longer exists. Because a watch channel only keeps the latest value, a consumer that falls behind skips straight to the freshest prices instead of working through a backlog. To track additions and removals, compare the snapshot's book ids against your previous view.

### Simulation

You can simulate a swap against an RFQ state using:

```rust
state.get_amount_out(amount_in, &sell_token, &buy_token)
```

This returns an indicative output amount, which you can use to decide if this swap is worth including.

### Encoding

After choosing the best swap, you can use Tycho Execution to encode it. This is very similar to the encoding done in the general [quickstart](../#id-4.-encode-a-swap).

#### Create a solution object

The key parameters are the quoted **expected amount out** and the **min amount out** you accept below it. The router uses both as guardrails, protecting you against slippage and MEV. The quickstart sets the min amount out 0.25% below the quote.

{% hint style="warning" %}
For maximum security, you should determine the quoted amount from a **third-party source.**
{% endhint %}

Build the Swap and Solution:

<pre class="language-rust"><code class="lang-rust">let swap =
    Swap::new(component, sell_token.clone(), buy_token.clone(), gas_usage)
        .with_protocol_state(state)
        .with_estimated_amount_in(sell_amount.clone());

// 0.25% below the quote
let min_amount_out = &expected_amount * BigUint::from(9975u64) / BigUint::from(10_000u64);

<strong>let solution = Solution::new(
</strong>    user_address.clone(),
    user_address,
    sell_token.address,
    buy_token.address,
    sell_amount,
    expected_amount, // the quoted output
    min_amount_out,  // the smallest acceptable output
    vec![simple_swap],
    )
    .with_user_transfer_type(UserTransferType::TransferFromPermit2);
</code></pre>

When working with RFQs, two fields are **required** in Swap:

*   `protocol_state`: This is needed to enable the runtime generation of a binding quote at encoding time—for example:

    ```rust
    state
        .as_indicatively_priced()?
        .request_signed_quote(GetAmountOutParams { ... })
        .await
    ```
* `estimated_amount_in` : This represents the estimaed input amount for the quote request. It’s especially important when the swap path is complex (e.g., involving multiple hops), where the actual input amount may differ slightly because of slippage. We recommend setting `estimated_amount_in` a bit higher than your expected value. Many RFQs enforce that execution can only occur for amounts **less than or equal to** the quoted base amount—so setting it conservatively helps avoid dropping funds. If the actual required input exceeds your estimate, any leftover tokens will remain in the Tycho Router.

This mechanism also makes RFQs composable with other on-chain swaps. That enables hybrid routing strategies, such as a path like **Uniswap → RFQ → Curve**, seamlessly combining RFQ-based and traditional on-chain routes.

{% hint style="warning" %}
After encoding, quotes are valid for only 1–3 seconds. Execution must follow immediately, otherwise the transaction will revert.
{% endhint %}

#### Encode solution

```rust
let swap_encoder_registry = SwapEncoderRegistry::new_with_defaults(Chain::Ethereum)
    .expect("Failed to get default SwapEncoderRegistry");
    
let encoder = TychoRouterEncoderBuilder::new()
    .chain(chain)
    .swap_encoder_registry(swap_encoder_registry)
    .build()
    .expect("Failed to build encoder");

let encoded_solution = encoder
    .encode_solutions(vec![solution.clone()])
    .expect("Failed to encode router calldata")[0]
```

#### Encode full method calldata

You need to build the full calldata for the router. Tycho handles the swap encoding, but you control the full input to the router method. This quickstart provides helper functions (`encode_tycho_router_call` and `sign_permit`)

Use it as follows:

```rust
let tx = encode_tycho_router_call(
    named_chain.into(),
    encoded_solution.clone(),
    &solution,
    chain.native_token().address,
    signer.clone(),
)
.expect("Failed to encode router call");
```

{% hint style="danger" %}
These functions are only examples intended for use within the quickstart. **Do not use them in production.** You must write your own logic to:

* Control parameters like `expectedAmountOut`, `minAmountOut` and `receiver`
* Sign the permit2 object safely and correctly.

This gives you full control over execution. And it protects you from MEV and slippage risks.
{% endhint %}

### Execution

This step allows you to test or perform real transactions based on the best available swap options. It needs the `PRIVATE_KEY` environment variable from [Quickstart](#quickstart). Handle that key securely and never expose it publicly.

```bash
cargo run --release --example rfq_quickstart
```

Once the best swap is found you can:

1. **Simulate the swap:** Tests the swap without executing it on-chain. It simulates an approval (for permit2) and a swap transaction on the node. If the status is `false`, the simulation has failed. You can print the full simulation output for detailed failure information.
2. **Execute the swap:** Performs the swap on-chain using your real funds. The process performs an approval (for permit2) and a swap transaction. You'll receive transaction hashes and statuses. After a successful execution, the program will exit. If the transaction fails, the program continues to stream new price updates.
3. **Skip this swap:** Ignores this swap. Then the program resumes listening for price updates.

{% hint style="warning" %}
**Important Note**

Market conditions can change rapidly. Delays in your decision-making can lead to transaction reverts, especially if you've set parameters like minimum amount out or slippage. Always ensure you're comfortable with the potential risks before executing swaps.
{% endhint %}

{% hint style="info" %}
Because the RFQ will only let you swap up to the amount of tokens specified in the quote, when the RFQ swap happens after another protocol in a sequential swap, if positive slippage occurs during the preceding swap, any additional input tokens beyond the permitted quote amount will remain in the Tycho Router and not be sent to the RFQ protocol.
{% endhint %}

## pAMM Price Level Stream

Besides the RFQ clients above, Tycho Simulation consumes <a href="https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level" target="_blank" rel="noopener noreferrer">Titan Builder's pAMM price level stream</a>: a WebSocket of complete per-pair quote snapshots for a subset of the pAMMs Titan serves. It only serves Ethereum Mainnet.

`PriceLevelStreamBuilder` turns those snapshots into the same `Update` messages the protocol stream emits, so you consume it like any other stream:

```rust
use tycho_simulation::price_level_stream::stream::PriceLevelStreamBuilder;

let price_level_stream = PriceLevelStreamBuilder::new()
    .with_known_pamms()       // serve the venues Tycho has measured
    .auto_detect(true)        // also serve any other venue Titan streams
    .with_tokens(all_tokens.clone())
    .build();
```

Quotes target the block currently being built, so the stream marks every update partial and supersedes the previous one for the pairs it contains. The stream never terminates — run it in its own task alongside your protocol stream.

Components arrive as `pricelevelstream:{pamm}`, where `{pamm}` is the venue name for a known venue or its address for an auto-detected one. Venues on Titan's PropAMMRouter whitelist arrive as `propammfallback:{pamm}` instead: `tycho-execution` routes those swaps through the router, which falls back to a single-hop Uniswap V3 pool when the venue reverts on a stale quote. The builder reads the whitelist once, at `build()`, through the node at `RPC_URL`; without that variable it warns and keeps every venue on the direct path. `without_fallback_router()` skips the read altogether.
