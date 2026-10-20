# ElFomoFi orderbook example

Streams helper orderbooks on new block heads, discovers pair/token changes automatically, and quotes locally with integer interpolation. Supports Base (default), BSC and Robinhood; protocol ID `elfomofi`. Tycho execution quotes and swaps use partner ID **299**.

## Run

From the repository root:

```bash
RPC_URL=https://your-base-rpc \
WS_RPC_URL=wss://your-base-websocket-rpc \
  cargo run -p tycho-simulation --example elfomofi_rfq
```

Options:

- `--chain bsc` / `--chain robinhood`: select the chain; both RPC endpoints must match.
- `--poll-ms 2000`: use HTTP polling without `WS_RPC_URL`.
- `--amount-in <raw>`: override the default one whole quote token per book.
- `--tycho-router <address>`: also fetch fresh unsigned execution quotes.
- `--max-age-seconds <seconds>`: book TTL, default 10.

SDK: build `pamm::protocols::elfomofi::ElfomofiFeedBuilder::new(chain)` and consume it with `book::BookFeedStream::<ElfomofiBlock>::spawn("elfomofi", feed)`. Each snapshot replaces the complete book set; withdrawal means stop quoting until fresh state arrives. RPC reads default to a 5-second timeout. The example makes read calls only.

## Execution

Register `ElfomofiSwapEncoder` with a deployed, allowlisted executor address and attach protocol state and estimated input to each swap. Settlement uses `swapWithContractBalance` with partner 299 and direct token funding.

Book quotes are indicative. Encoding refreshes the execution quote; the caller sets the route's expected/minimum output. Production executor addresses are not configured in this checkout.

## Tests

```bash
cargo test -p tycho-simulation --lib elfomofi
cargo test -p tycho-execution --lib elfomofi
forge test --root crates/tycho-execution/contracts --match-contract '^ElfomofiExecutorTest$' --offline
```

The Solidity test also verifies the executor runtime fixture. Fixture regeneration uses the shared `forge script script/WriteRuntimeBytecodeFixtures.s.sol` tooling from the contracts directory.

For pinned real-contract fork tests, set `BASE_RPC_URL`, `BSC_RPC_URL` and `ROBINHOOD_RPC_URL` to archive RPCs, then run:

```bash
forge test --root crates/tycho-execution/contracts --match-contract '^Elfomofi(Bsc|Robinhood)?ForkTest$' --offline
```

## Routing test

The [runner](../../../../protocols/testing/scripts/run_elfomofi_routing.py) compares 100 USDC → WETH quotes against a real Base Uniswap V2 pool, requires ElFomoFi to win, and verifies exact execution output and partner 299. Reads are pinned to one real block; the RPC must support `debug_traceCall` with state overrides. No transactions are broadcast.

```bash
RPC_URL=https://your-base-rpc \
  python3 protocols/testing/scripts/run_elfomofi_routing.py -- \
  --report /tmp/elfomofi-routing.json
```

Use `--no-build` to reuse the compiled example; `--report` saves quotes and execution results.
