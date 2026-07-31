# QuickStart

This quickstart guide enables you to:

1. Retrieve data from the Tycho Indexer.
2. Leverage Tycho Simulation to get the best amount out of a trade.

## How to run

```bash
export RPC_URL=<your-rpc-url>
export TYCHO_API_KEY=<your-api-key>
cargo run --release --example quickstart
```

By default, the example will trade 10 USDC -> WETH on Ethereum Mainnet. Setting the chain will by default trade 10
USDC -> WETH on that chain.
If you want a different trade or chain, you can do:

```bash
export RPC_URL=<rpc-url-for-chain>
cargo run --release --example quickstart -- --sell-token "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913" --buy-token "0x4200000000000000000000000000000000000006" --sell-amount 10 --chain "base"
```

for 10 USDC -> WETH on Base.

To be able to execute or simulate the best swap, you need to set your private key as an environment variable. Be sure not to save it to your terminal history:

```bash
unset HISTFILE
export PRIVATE_KEY=<your-private-key>
cargo run --release --example quickstart
```

See [here](https://docs.propellerheads.xyz/tycho) a complete guide on how to run the
Quickstart example.