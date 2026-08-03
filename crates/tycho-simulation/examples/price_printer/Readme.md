# Price Printer

This example allows you to list all pools over a certain tvl threshold and explore
quotes from each pool.

## How to run

```bash
export RPC_URL=<your-node-rpc-url>
cargo run --release --example price_printer -- --tvl-threshold 1000 --chain <ethereum | base | unichain>
```

Both the token loader and the protocol stream default to TLS (`https`/`wss`). Pass `--no-tls`
(or set `TYCHO_NO_TLS=true`) when pointing at a local dev instance served over plain HTTP
(e.g. `TYCHO_URL=localhost:4242`).
