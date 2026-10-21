# Off-Chain Priced Venues

Some venues price off the chain. Instead of pool state Tycho reads from the chain and simulates against, they publish a complete **book** — the price levels they will trade at — over a WebSocket or an API, and republish it whenever it changes. Tycho streams those books, simulates against them, and executes them as part of multi-protocol swaps. Their components carry the `rfq:` protocol systems in the table below.

Two kinds share that shape, and the difference shows at execution:

* **RFQ market makers** — Bebop, Hashflow, Liquorice and Native. The streamed book prices **indicatively**, for simulation; at encoding time the maker signs a **binding** quote, firm for a few seconds, and may decline the trade.
* **Off-chain-priced pAMMs** — Metric. The book prices a pool you execute against directly: there is no quote to request and nothing to sign.

| Protocol    | Kind | Simulation Time | Credentials | Chains                                            | Protocol system  |
|-------------|------|-----------------|-------------|---------------------------------------------------|------------------|
| `bebop`     | RFQ  | 0.5 µs          | Required    | Ethereum, BSC, Polygon, Base, Arbitrum, Robinhood | `rfq:bebop`      |
| `hashflow`  | RFQ  | 0.4 µs          | Required    | Ethereum, BSC, Polygon, Base, Arbitrum            | `rfq:hashflow`   |
| `liquorice` | RFQ  | 0.4 µs          | Required    | Ethereum, Arbitrum                                | `rfq:liquorice`  |
| `native`    | RFQ  | 0.4 µs          | Required    | Ethereum, Base, Arbitrum, BSC, Robinhood          | `rfq:native`     |
| `metric`    | pAMM | -               | Required    | Ethereum, BSC, Polygon, Base, Arbitrum, Robinhood | `rfq:metric`     |

A feed runs wherever its venue prices, which is what the table lists. Settling a swap against one additionally needs a Tycho executor for that venue on that chain; `config/executor_addresses.json` in tycho-execution lists the deployments.

On Ethereum, Metric is also available as a pAMM venue on the [pAMM price level stream](price-level-stream.md).

## Quickstart

The book-feed quickstart is similar to the other protocols [quickstart](../).

See the code <a href="https://github.com/propeller-heads/tycho-indexer/tree/main/crates/tycho-simulation/examples/book_quickstart" target="_blank" rel="noopener noreferrer">here</a>. As of now, <a href="https://docs.bebop.xyz/bebop/bebop-api-pmm-rfq/pmm-rfq-api-intro" target="_blank" rel="noopener noreferrer">Bebop</a>, <a href="https://docs.hashflow.com/hashflow/taker/getting-started-api-v3" target="_blank" rel="noopener noreferrer">Hashflow</a>, <a href="https://liquorice.tech/" target="_blank" rel="noopener noreferrer">Liquorice</a>, <a href="https://docs.native.org/" target="_blank" rel="noopener noreferrer">Native</a> and <a href="https://docs.metric.xyz/" target="_blank" rel="noopener noreferrer">Metric</a> are the only supported providers.

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

Metric's feed talks to Metric's public endpoint (`MetricFeedBuilder::base_url` points it elsewhere) and sends the trading key as the Bearer token the authenticated `bid_ask` endpoint requires. The example registers Metric by default; `--disable-pamm` leaves it out.

Then run the example:

```rust
cargo run --release --example book_quickstart
```

{% hint style="info" %}
You’ll need to request credentials directly from each provider.
{% endhint %}

### What it does

The quickstart:

* Connects to the book feeds and fetches live price updates.
* Simulates the best available amount out for a given pair (default: 10 USDC → WETH on mainnet).
* Encodes the swap and prepares calldata to execute it via the Tycho Router.

If you want to see results for a different token, amount, minimum TVL, or chain, you can set additional flags:

```bash
cargo run --release --example book_quickstart -- --sell-token "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913" --buy-token "0x4200000000000000000000000000000000000006" --sell-amount 10 --tvl-threshold 1000 --chain "base"
```

This example would seek the best swap for 10 USDC -> WETH on Base.

### Set up

You’ll need to configure:

* Tycho URL (by default `"tycho-beta.propellerheads.xyz"`)
* Tycho API key
* Provider API keys (the example's `Readme.md` lists the environment variables it reads for each provider; the library itself takes credentials as builder arguments)
* Private key if you wish to execute the swap against the Tycho Router

To get token information from Tycho Indexer RPC please use [load\_all\_tokens](simulation.md#step-1-fetch-tokens).

### Book feeds

Each venue has its own feed type (`BebopFeed`, `HashflowFeed`, `LiquoriceFeed`, `NativeFeed`, `MetricFeed`), built from a shared `BookFeedConfig` plus provider-specific credentials and options. Every feed implements the `SnapshotFeed` trait — a live price feed that publishes the provider's latest complete set of books, or withdraws when nothing is servable (before the first set arrives, and after one goes stale). You read it through `BookFeedStreams`, which owns the channels and the feed tasks: each feed runs in its own task and hands you the newest set through a watch channel, so falling behind costs you skipped snapshots rather than a stalled venue connection. Binding quotes are not part of the feed — for the RFQ venues, the states it emits request them at encoding time through their embedded client.

Example setup for Bebop:

```rust
let config = BookFeedConfig { chain, tokens: Arc::new(rfq_tokens), min_tvl_usd };
let usd_quote_tokens = usd_stablecoins_for_chain(chain).expect("chain has curated USD stablecoins");

let bebop_feed = BebopFeedBuilder::new(config.clone(), usd_quote_tokens.clone(), bebop_key)
    .build()
    .expect("Failed to create Bebop feed");
```

`BookFeedConfig` carries what every book feed needs whatever transport it runs on — the chain, the tradable tokens with their metadata, and the minimum book TVL in USD. Each feed builder takes it by value (clone it when building several feeds); the builders' own setters cover only provider-specific options such as credentials, poll cadence, or Bebop's origin fields. To give one provider a different token set or threshold, build it from a different `BookFeedConfig`.

**Minimum TVL** (`min_tvl_usd`) counts in USD. This setting filters out token pairs whose book is thin on the venue's side, helping avoid illiquid quotes.

**USD quote tokens:** The Bebop, Hashflow, Liquorice and Native builders take a set of USD-priced tokens that their feeds normalize book TVL into before applying `min_tvl_usd`. The feeds use the set only to filter by TVL, never for quote requests or trade execution. Metric's API reports USD TVL directly, so its builder takes none.

You should specify USD-priced stablecoins (e.g., USDC, USDT, DAI) as quote tokens, since the supported venues price most of their liquidity in USD stablecoins. This ensures the feed calculates TVL accurately when comparing pairs with different quote tokens. For instance, if you receive price levels for an ETH/WBTC pair where WBTC is the quote token, the feed will look up the WBTC price in one of your approved quote tokens (USD stablecoins) to properly calculate the TVL in dollar terms. `usd_stablecoins_for_chain` returns the curated set for a chain Tycho has one for, and `None` for the rest; an empty set filters out every pair, so always pass a non-empty one.

**Note:** Some venues may support tokens that Tycho does not. Because execution happens through the Tycho Router, it’s important to ensure that every token you trade out of these books is also supported by Tycho.

### Stream: Real-Time Price Updates

Each feed publishes its complete set of books as a `BookSnapshot` over a `tokio::sync::watch` channel: one `Book` — a component, the state that prices it and the provider's `updated_at` where it reports one — for each book the venue publishes, anchored by the feed's receipt time (`anchor: ReceivedAt`, a `DateTime<Utc>`). What a book is depends on the venue: a pair for Bebop, Native and Metric, a pair and one market maker for Hashflow and Liquorice, since a firm quote comes from the maker you ask it of and so fills against that maker's depth alone.

**A feed withdraws its snapshot** — the watch goes back to `None` — when nobody is refreshing it: once the published book is older than `max_snapshot_age`, and whenever the feed itself ends, because it gave up or because you dropped the stream or watch running it. So `None` means "nothing servable right now", not only "nothing yet", and you should stop quoting a provider whose snapshot has gone away. The knob lives on both feed configs, and `None` keeps the last snapshot until the feed ends. Setting it above the poll interval keeps a book servable from one poll to the next; setting it below turns it into a duty cycle — the book is servable for `max_snapshot_age` and gone until the next poll — which is what you want when a venue's rate limit fixes the cadence and you would rather decline than quote an old book. Zero is the one value no feed takes: it ends with `FeedError::InvalidInput` as soon as it runs, which reaches you as `BookFeedOutcome::Failed`. While a provider keeps failing its feed retries with growing gaps rather than hammering it, so a book that a long outage withdrew can take a while to come back after the provider recovers.

The feed configs have no `Default`: start from the builder's `default_feed_config()`, whose documentation states the values it begins with, and change fields with struct-update syntax.

Add every feed you built to a `BookFeedStreams` under its `PROTOCOL_SYSTEM` — `add` spawns it in its own task, and hands it back in an `Err` if that label already holds a feed — then read events until the set runs dry. The set is a `Stream`, so `StreamExt` applies to all of its feeds at once, and each event arrives with the provider it happened on:

```rust
let mut feeds = BookFeedStreams::new();
feeds.add(bebop::PROTOCOL_SYSTEM, bebop_feed).expect("one feed per protocol system");
// … one `add` per provider you have credentials for

while let Some((provider, event)) = feeds.next().await {
    match event {
        BookFeedEvent::Published(snapshot) => {
            // snapshot.books is the provider's complete current set of books
        }
        BookFeedEvent::Withdrawn => {
            // the provider has nothing servable: stop quoting it until it publishes again
        }
        BookFeedEvent::Ended(BookFeedOutcome::Failed(error)) => {
            // the feed gave up and is gone for good; `error` says why
        }
        BookFeedEvent::Ended(outcome) => {
            // it ran out of books to serve, or its task panicked
        }
    }
}
// every provider has ended
```

A single feed reads the same way through `BookFeedStream::spawn(provider, feed)`. Dropping a `BookFeedStreams` or a `BookFeedStream` aborts the feed tasks it holds.

If you price on demand rather than reacting to every update, read the feed as a `BookFeedWatch` instead: `BookFeedWatch::spawn(provider, feed)` runs it the same way and `receiver()` hands out `watch::Receiver` clones you read with `borrow()`, without awaiting and without slowing the venue's connection. `None` in the channel means nothing is servable — nothing yet, gone stale, or the feed is over — and since those look alike to a reader, `ended()` is what tells the last one apart: it resolves when the feed stops, with a `BookFeedOutcome` saying whether it gave up (and why), ran out, or panicked. Until then the venue is only quiet. `ended()` reports the outcome once and then stays pending, so it sits in a `select!` loop beside your other work without ever firing twice.

Book snapshots are **timestamped**, not block-based: the snapshot's `anchor` is the feed's receipt time (`ReceivedAt`) and each `Book` carries the provider's `updated_at` where the provider reports one (Bebop, Liquorice, Metric). Every snapshot is the provider's full current state, so a pair missing from the current snapshot is one the provider has stopped serving. Because a watch channel only keeps the latest value, a consumer that falls behind skips straight to the freshest prices instead of working through a backlog. To track additions and removals, compare the snapshot's book ids against your previous view.

### Venue components

A component names the directions it quotes in its `swap_directions` static attribute, 40 bytes each, the token sold followed by the token bought. It carries one where its token list does not already say which way round it trades: a book with levels on one side only quotes one direction, and one the venue has stopped pricing quotes none. Build your graph edges from that attribute where a component has it and from both orderings of its two tokens where it does not, and rebuild them when a snapshot changes it. `tycho_simulation::book::{SWAP_DIRECTIONS_ATTRIBUTE, decode_swap_directions}` read it.

By default a feed publishes a component per book — per pair, or per market maker and pair on the two venues that name their makers — and a swap against it leaves the state unchanged. `.all_pairs()` on the Bebop and Native builders, and `.all_pairs(rule)` on Hashflow's and Liquorice's, publishes the venue as a single component covering every pair it prices instead, which is what lets a route stay within the number of quotes the venue will sign for it. Only venues that name their market makers take a rule, because only there is there a choice to make; `QuoteRule` lives in `tycho_simulation::book::all_pairs`. Metric has no quote to sign and keeps one component per pool.

```rust
let hashflow_feed = HashflowFeedBuilder::new(config, usd_quote_tokens, user, key)
    .all_pairs(QuoteRule::OncePerMaker)
    .build()
    .expect("Failed to create Hashflow feed");
```

Such a component carries every token the venue quotes, and the pairs it prices are far fewer than every pair of those tokens, which is what makes `swap_directions` worth reading on it. Its state prices and quotes exactly as the per-pair states do — the books behind it are the same ones.

A swap records the book it used in the `new_state` it returns, and how often one route may quote the venue is its quote rule, carried as the `quote_rule` static attribute:

* Hashflow and Liquorice name their market makers, so their builders take the rule: `QuoteRule::OncePerMaker` lets every maker quote once per route, since a maker's second quote prices as if the first had not happened, and `QuoteRule::OncePerVenue` holds the whole venue to one quote where that independence is not worth relying on.
* Bebop and Native pick the makers behind a quote themselves and sign once per route (`once_per_venue`), so their builders take no rule.

The rule takes effect wherever your algorithm threads `new_state` from one use of the component to the next; a swap it refuses returns a recoverable error saying every book pricing it has already quoted in this route.

Request the firm quote on the state you simulated the swap on, with the simulated amount: that state picks the same book the swap went to, and asks the market maker behind it. On the `new_state` the swap returned, that book's turn is spent.

### Simulation

You can simulate a swap against a book state using:

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

For the RFQ venues, Swap **requires** two fields (Metric needs neither — its encoder works from the component and the tokens alone, because there is no quote to request):

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
After encoding, a signed RFQ quote is valid for only 1–3 seconds. Execution must follow immediately, otherwise the transaction will revert.
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
cargo run --release --example book_quickstart
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

Besides the book feeds above, Tycho Simulation consumes <a href="https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level" target="_blank" rel="noopener noreferrer">Titan Builder's pAMM price level stream</a>: a WebSocket of per-pair quote ladders for the pAMMs Titan builds blocks with, on Ethereum Mainnet. `PriceLevelStreamBuilder` turns those frames into the same `Update` messages the protocol stream emits, so you consume it like any other stream, and it quotes venues no book feed above covers. The [pAMM Price Level Stream](price-level-stream.md) page covers its configuration, freshness contract, message format and metrics.
