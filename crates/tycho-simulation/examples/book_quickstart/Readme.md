# Off-Chain Book QuickStart

This quickstart guide enables you to:

1. Open a live feed per off-chain book venue — the RFQ market makers and the pAMMs — and read
   each venue's complete current book.
2. Use Tycho Simulation to find the best quoted price across them.

## How to run

The example loads the chain's token list from Tycho, and each venue needs its own credentials to
serve live pricing data:

```bash
export TYCHO_URL=<tycho-api-url-for-chain>
export TYCHO_API_KEY=<your-tycho-api-key>

export BEBOP_KEY=<your-bebop-api-key>

export HASHFLOW_USER=<your-hashflow-username>
export HASHFLOW_KEY=<your-hashflow-key>

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

The pAMM feeds run alongside the RFQ venues when configured, so RFQ credentials are optional.
Pass `--disable-pamm` to leave them out:

```bash
cargo run --release --example book_quickstart -- --disable-pamm
```

`--all-pairs` builds the four RFQ venues as one component per venue instead of one per book, which
is the layout a router uses to hold a route to the quotes a venue will sign for it:

```bash
cargo run --release --example book_quickstart -- --all-pairs
```

By default, the example requests price levels for 10 USDC -> WETH. Where it knows no default tokens for a chain it
says so and stops, so pass `--sell-token` and `--buy-token` there. To pick your own trade and chain, replace the values
below with the tokens and chain you want:

```bash
cargo run --release --example book_quickstart -- --sell-token "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913" --buy-token "0x4200000000000000000000000000000000000006" --sell-amount 10 --chain "base"
```

for 10 USDC -> WETH on Base.

To be able to execute or simulate the best swap, you need to set your private key as an environment variable before
running the quickstart. Be sure not to save it to your terminal history:

```bash
unset HISTFILE
export PRIVATE_KEY=<your-private-key>
export RPC_URL=<your-rpc-url>
...
```

## Important Notes

- **Credentials**: Contact each venue directly to obtain API credentials for accessing its live
  book. pAMM feeds use their configured credentials or RPC endpoints; `--disable-pamm` leaves them out.

## What you'll see

The example will:

1. Open a feed per venue you have credentials for
2. Stream each venue's complete book for your specified token pair, republished whenever it
   changes
3. Display the best available quotes with pricing information
4. Allow you to simulate or execute swaps when a private key is provided

See [here](https://docs.propellerheads.xyz/tycho/for-solvers/request-for-quote-protocols) a complete guide to the
book feeds.

## ElFomoFi

Enable ElFomoFi with `--enable-elfomofi`. On Base, BSC and Robinhood, set `RPC_URL` and
`WS_RPC_URL` for the selected chain.
`propamm:elfomofi` fetches complete helper books on new block heads, discovers pair changes,
and interpolates quotes locally. Use `--book-polling` for HTTP-only providers. Settlement uses
`swapWithContractBalance`, partner **299**, and no venue approval.

```bash
cargo run --release --example book_quickstart -- --chain base --enable-elfomofi
```

The quickstart needs a configured executor address for simulation or execution.

Validate execution with the local executor fixture through read-only RPC state overrides:

```bash
cargo run -p tycho-integration-test -- --chain base --enable-elfomofi \
  --disable-onchain --disable-rfq --disable-price-level-stream \
  --max-blocks 1 --skip-messages-duration 0 \
  --executor-fixture propamm:elfomofi=protocols/testing/fixtures/Elfomofi.runtime.json
```
