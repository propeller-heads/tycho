# Request for Quote Protocols

Request for Quote (RFQ) protocols work differently from on-chain protocols. Instead of reading pool data from the chain, they fetch prices from off-chain market makers via WebSocket or API.

You ask for a quote for a specific trade size, and they return a price. Quotes can be:

* **Indicative** — estimated prices used for simulation.
* **Binding** — firm prices, valid for a short time, used at execution.

Tycho supports streaming, simulating, and executing RFQ quotes as part of multi-protocol swaps.

Currently, Tycho supports the following RFQ protocols:

| Protocol    | Simulation Time |
| ----------- | --------------- |
| `bebop`     | 0.5 µs          |
| `hashflow`  | 0.4 µs          |
| `liquorice` | 0.4 µs          |

## Quickstart

The RFQ quickstart is similar to the other protocols [quickstart](../).

See the code <a href="https://github.com/propeller-heads/tycho-indexer/tree/main/crates/tycho-simulation/examples/rfq_quickstart" target="_blank" rel="noopener noreferrer">here</a>. As of now, <a href="https://docs.bebop.xyz/bebop/bebop-api-pmm-rfq/pmm-rfq-api-intro" target="_blank" rel="noopener noreferrer">Bebop</a>, <a href="https://docs.hashflow.com/hashflow/taker/getting-started-api-v3" target="_blank" rel="noopener noreferrer">Hashflow</a> and <a href="https://liquorice.tech/" target="_blank" rel="noopener noreferrer">Liquorice</a> are the only supported providers.

You need to set up the API credentials of the desired RFQs to access live pricing data and quoting, as well as your private key if you wish to execute against the Tycho Router:

```bash
export BEBOP_USER=<your-bebop-ws-username>
export BEBOP_KEY=<your-bebop-ws-key>
export HASHFLOW_USER=<your-hashflow-api-username>
export HASHFLOW_KEY=<your-hashflow-api-key>
export LIQUORICE_USER=<your-liquorice-api-username>
export LIQUORICE_KEY=<your-liquorice-api-key>
export PRIVATE_KEY=<your-wallet-private-key>
```

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
* RFQ API keys (Have a look at `src/rfq/constants.rs` to see the authentication variables that are expected)
* Private key if you wish to execute the swap against the Tycho Router

To get token information from Tycho Indexer RPC please use [load\_all\_tokens](simulation.md#step-1-fetch-tokens).

### RFQClient

Each RFQ protocol will have its own client. The client can **stream live prices updates** and **request binding quotes**.

Example setup for Bebop:

```rust
let bebop_client = BebopClientBuilder::new(chain, bebop_ws_user, bebop_ws_key)
    .tokens(rfq_tokens)
    .quote_tokens(quote_tokens)
    .tvl_threshold(cli.tvl_threshold)
    .build()
    .expect("Failed to create RFQ clients");
```

**TVL threshold** is specified in USD, as most RFQ quotes are USD-denominated. This setting filters out token pairs with low liquidity on the RFQ side, helping avoid thin or illiquid quotes.

**Quote tokens:** You can optionally specify quote tokens when configuring the RFQ client to define which tokens the client should consider “approved” for TVL normalization purposes. The client uses this approved quote token list exclusively for TVL filtering and does not use it for quote requests or trade execution.

You should specify USD-priced stablecoins (e.g., USDC, USDT, DAI) as quote tokens, since currently-supported RFQ providers quote most of their currently supported liquidity in USD stablecoins. This ensures the client calculates TVL accurately when comparing pairs with different quote tokens. For instance, if you receive price levels for an ETH/WBTC pair where WBTC is the quote token, the client will look up the WBTC price in one of your approved quote tokens (USD stablecoins) to properly calculate the TVL in dollar terms. If you don’t explicitly set quote tokens, the client uses chain-specific defaults.

**Note:** Some RFQ providers may support tokens that Tycho does not. Because execution happens through the Tycho Router, it’s important to ensure that all tokens used in RFQ quotes are also supported by Tycho.

### Stream: Real-Time Price Updates

The `RFQStreamBuilder` handles registration of multiple RFQ clients and merges their message streams. It merges updates from one or more RFQ clients and decodes them into `Update` messages:

```rust
let rfq_stream_builder = RFQStreamBuilder::new()
    .add_client::<BebopState>("bebop", Box::new(bebop_client))
    .set_tokens(all_tokens.clone())
    .await;
```

* Use `add_client()` for each RFQ provider.
* Streams that return errors are removed automatically.

RFQ streams are **timestamped**, not block-based. Each update provides the full known state from the provider at that moment (not just deltas). The `removed_pairs` field indicates any pairs that disappeared since the last update. The `new_pairs` field contains all the currently available pairs.

### Simulation

You can simulate a swap against an RFQ state using:

```rust
state.get_amount_out(amount_in, &sell_token, &buy_token)
```

This returns an indicative output amount, which you can use to decide if this swap is worth including.

### Encoding

After choosing the best swap, you can use Tycho Execution to encode it. This is very similar to the encoding done in the general [quickstart](../#id-4.-encode-a-swap).

#### Create a solution object

The key parameter is **minimum amount out**, which protects against slippage and MEV. The quickstart applies 0.25% slippage tolerance.

{% hint style="warning" %}
For maximum security, you should determine the minimum amount from a **third-party source.**
{% endhint %}

Build the Swap and Solution:

<pre class="language-rust"><code class="lang-rust">let swap =
    Swap::new(component, sell_token.address.clone(), buy_token.address.clone())
        .protocol_state(state)
        .estimated_amount_in(sell_amount.clone());

<strong>let solution = Solution::new(
</strong>    user_address.clone(),
    user_address,
    sell_token.address,
    buy_token.address,
    sell_amount,
    min_amount_out,
    vec![simple_swap],
    )
    .with_user_transfer_type(UserTransferType::TransferFromPermit2);
</code></pre>

When working with RFQs, two fields are **required** in Swap:

*   `protocol_state`: This is needed to enable the runtime generation of a binding quote at encoding time—for example:

    ```rust
    state.request_binding_quote(&GetAmountOutParams { ... }).await
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

* Control parameters like `minAmountOut` and `receiver`
* Sign the permit2 object safely and correctly.

This gives you full control over execution. And it protects you from MEV and slippage risks.
{% endhint %}

### Execution

This step allows you to test or perform real transactions based on the best available swap options. For this step, you need to pass your wallet's private key in the run command. Handle it securely and never expose it publicly.

```bash
cargo run --release --example quickstart -- --swapper-pk $PK
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
