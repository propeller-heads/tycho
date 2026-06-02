# RFQ QuickStart

This quickstart guide enables you to:

1. Connect to the RFQ WebSocket stream for real-time pricing data.
2. Leverage Tycho Simulation to get the best quoted prices from RFQ market makers.

## How to run

You need to set up the WebSocket credentials of the desired RFQs to access live pricing data:

```bash
export BEBOP_USER=<your-bebop-ws-username>
export BEBOP_KEY=<your-bebop-ws-key>

export HASHFLOW_USER=<your-ws-hashflow-username>
export HASHFLOW_KEY=<your-ws-hashflow-key>

export LIQUORICE_USER=<your-liquorice-solver>
export LIQUORICE_KEY=<your-liquorice-key>

export METRIC_API_URL=<optional-custom-metric-api-url>
export METRIC_SECRET_KEY=<optional-metric-secret-key>
```

Then, you can run the example with:

```bash
cargo run --release --example rfq_quickstart
```

To run PAMM RFQ protocols without Bebop, Hashflow, or Liquorice credentials:

```bash
cargo run --release --example rfq_quickstart -- --run-pamm-protocols
```

By default, the example will request price levels for 10 USDC -> WETH on Ethereum Mainnet using RFQs.
If we choose a different chain, by default, price levels for USDC -> WETH will be requested on that chain.
If you want a different trade and chain, you can use the following command, replacing the values with the token and
chain that you'd like:

```bash
cargo run --release --example rfq_quickstart -- --sell-token "0x50c5725949A6F0c72E6C4a641F24049A917DB0Cb" --buy-token "0x4200000000000000000000000000000000000006" --sell-amount 10 --chain "base"
```

for 10 USDC -> WETH on Base.

To be able to execute or simulate the best swap, you need to set your private key as an environment variable before
running the quickstart. Be sure not to save it to your terminal history:

```bash
unset HISTFILE
export PRIVATE_KEY=<your-private-key>
...
```

## Important Notes

- **Credentials**: Contact RFQ protocols directly to obtain WebSocket API credentials for accessing live market maker
  quotes. To run PAMM RFQ protocols, pass `--run-pamm-protocols`.

## What you'll see

The example will:

1. Connect to the RFQ's WebSocket API using your credentials
2. Stream live price quotes from market makers for your specified token pair
3. Display the best available quotes with pricing information
4. Allow you to simulate or execute swaps when a private key is provided

TODO: update this once docs are merged
See [here](https://docs.propellerheads.xyz/tycho/for-solvers/tycho-quickstart) a complete guide on how to run the
Quickstart example.
