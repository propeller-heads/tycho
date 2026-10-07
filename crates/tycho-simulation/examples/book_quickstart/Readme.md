# Off-Chain Book QuickStart

This quickstart guide enables you to:

1. Open a live feed per off-chain book venue — the RFQ market makers and the pAMMs — and read
   each venue's complete current book.
2. Leverage Tycho Simulation to get the best quoted prices across them.

## How to run

The example loads the chain's token list from Tycho, and each venue needs its own credentials to
serve live pricing data:

```bash
export TYCHO_URL=<tycho-api-url-for-chain>
export TYCHO_API_KEY=<your-tycho-api-key>

export BEBOP_KEY=<your-bebop-api-key>

export HASHFLOW_USER=<your-ws-hashflow-username>
export HASHFLOW_KEY=<your-ws-hashflow-key>

export LIQUORICE_USER=<your-liquorice-solver>
export LIQUORICE_KEY=<your-liquorice-key>

export NATIVE_API_KEY=<your-native-api-key>

export METRIC_API_KEY=<your-metric-trading-key>
```

`TYCHO_URL` defaults to the hosted endpoint for the chain and `TYCHO_API_KEY` to `sampletoken`,
which works against a local dev instance.

Then, you can run the example with:

```bash
cargo run --release --example book_quickstart
```

The pAMM feeds (Metric) run alongside the RFQ venues by default, and need only `METRIC_API_KEY` —
so the example works with no RFQ credentials at all. Pass `--disable-pamm-feeds` to leave them out:

```bash
cargo run --release --example book_quickstart -- --disable-pamm-feeds
```

By default, the example will request price levels for 10 USDC -> WETH on Ethereum Mainnet.
If we choose a different chain, by default, price levels for USDC -> WETH will be requested on that chain.
If you want a different trade and chain, you can use the following command, replacing the values with the token and
chain that you'd like:

```bash
cargo run --release --example book_quickstart -- --sell-token "0x50c5725949A6F0c72E6C4a641F24049A917DB0Cb" --buy-token "0x4200000000000000000000000000000000000006" --sell-amount 10 --chain "base"
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

- **Credentials**: Contact each venue directly to obtain API credentials for accessing its live
  book. The pAMM feeds need only `METRIC_API_KEY`; `--disable-pamm-feeds` leaves them out.

## What you'll see

The example will:

1. Open a feed per venue you have credentials for
2. Stream each venue's complete book for your specified token pair, republished whenever it
   changes
3. Display the best available quotes with pricing information
4. Allow you to simulate or execute swaps when a private key is provided

TODO: update this once docs are merged
See [here](https://docs.propellerheads.xyz/tycho/for-solvers/tycho-quickstart) a complete guide on how to run the
Quickstart example.
