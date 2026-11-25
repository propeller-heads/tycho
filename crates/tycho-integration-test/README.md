# Tycho Integration Test

A comprehensive integration test that validates the Tycho system by performing end-to-end testing.
This test runs continuously in the cluster but it can also be run locally for debugging.

1. **Stream Processing**: Connects to Tycho indexer and receives live protocol state updates
2. **State Validation**: For each tested protocol update (after the first one, and every
   `--test-every-n-updates`-th), for the components selected from it, and for each swap direction:
    1. **Get Limits**: Retrieves maximum input/output limits for token pairs
    2. **Simulate Swap**: Calls `get_amount_out` with the max input, capped at 5 ETH worth of the
       token (at the token price snapshot) and at 2^96 - 1
    3. **Encode Transaction**: Builds the actual swap transaction via TychoRouterV3
    4. **Execute Simulation**: Runs `debug_traceCall` with necessary state overrides
    5. **Calculate Slippage**: Compares simulated vs actual execution results, and records it as a
       metric

## Supported Protocols

- **On-chain Protocols**: All DEX protocols supported by Tycho (Uniswap V2/V3, Curve, Balancer, etc.)
- **RFQ Protocols**: Request-for-Quote venues (Bebop, Hashflow, Liquorice, Native), each run when its credentials are set, and Euclid, whose gateway is public and which runs on Ethereum without credentials.
- **pAMM Book Feeds**: venues selected by `--pamm` (default: Metric; BaiBai on Base when listed). BaiBai's swaps are simulated, but fail to encode and are skipped with a warning until its executor is deployed and listed in `executor_addresses.json`
- **Efficient PropAMM listing doors**: `--pamm efficient-prop`, Ethereum, Base and BSC (Base and BSC serve a listing only with its gas named in `EFFICIENT_PROP_CREDIT_LISTINGS`; no listing exists on Base yet, the front is not deployed on BSC, and BSC lists no `PropAMMExecutor`); one book per listing door, sampled from the door's own quotes over `RPC_URL` (on Ethereum and BSC at the next block, under the state map `EFFICIENT_PROP_STATE_URLS` serves), executed through the generic `PropAMMExecutor`
- **Efficient pools**: `--pamm flamm`, Base, Ethereum and BSC; one book per pool's hook pair, a ladder of the pool's own `previewSwap` answers read over `RPC_URL`, served for the hooks and price feeds the `FLAMM_*` variables name, executed through the generic `PropAMMExecutor` on Ethereum. Base lists no executor for `flamm` yet and BSC lists no `PropAMMExecutor`, so swaps on both are simulated, fail to encode and are skipped with a warning
- **Price Level Stream**: Titan pAMM price level stream venues (`pricelevelstream:*`, Ethereum only)

## Configuration

### Environment Variables

| Variable        | Description                | Example                                    |
|-----------------|----------------------------|--------------------------------------------|
| `RUST_LOG`      | Logging configuration      | `tycho_integration_test=info,error`        |
| `TYCHO_URL`     | Tycho indexer endpoint     | `tycho-dev.propellerheads.xyz`             |
| `RPC_URL`       | Blockchain RPC endpoint; the BaiBai, listing-door and Efficient pools feeds poll it too, so it must be a Base node with `--pamm baibai` | `https://eth-mainnet.alchemyapi.io/v2/...` |
| `TYCHO_API_KEY` | API key for Tycho services; defaults to `sampletoken` | `your-api-key`                             |
| `TYCHO_NO_TLS`  | Connect to Tycho over plain HTTP/WS (a local instance) | `true`                     |
| `BEBOP_KEY`, `HASHFLOW_USER` + `HASHFLOW_KEY`, `LIQUORICE_USER` + `LIQUORICE_KEY`, `NATIVE_API_KEY` | RFQ venue credentials; a venue without them is skipped. Euclid needs none and runs on Ethereum | |
| `METRIC_API_KEY` | Metric (pAMM) credentials; required when `--pamm` lists `metric` | |
| `EFFICIENT_PROP_FRONT` | The Efficient PropAMM front proxy; required when `--pamm` lists `efficient-prop` | |
| `EFFICIENT_PROP_FACTORY` | The listing-door factory serving that front; required when `--pamm` lists `efficient-prop` | |
| `EFFICIENT_PROP_QUOTE_LAYOUT` | The layout of the quote contract the makers' updates write, read on Ethereum and BSC: the only value is `builder`, the layout on both. Required with `efficient-prop` on Ethereum and BSC, refused on Base, whose path samples the doors and reads no quote contract | `builder` |
| `EFFICIENT_PROP_CELL_CODEHASH` | The creation code hash of the desk contracts (the front's `CELL_CODEHASH`): a maker's desk contract is the front's CREATE2 child at salt `(1 << 252) \| desk` under it, which the front's `deskCellOf` must confirm. Required with `efficient-prop` on every chain: a listing is served only where its desk contract is approved. No door code hash is configured: the feed reads it from the factory | `<filled at the deploy>` |
| `EFFICIENT_PROP_IMPLEMENTATIONS` | Comma-separated front implementations the feed accepts; a poll that finds the front on another ends the feed. Required when `--pamm` lists `efficient-prop` | |
| `EFFICIENT_PROP_STATE_URLS` | Comma-separated URLs answering `titan_getPammStateOverrides` with the next block's state map, read every poll; each listing is quoted from the first URL whose answer for the block after the latest covers it: the answer writes the listing's quote word with a stamp that block fills (or one carried from the latest block, which the latest block's own word must equal), under the desk epoch and on a desk the latest block shows not stopped, writes no slot of the maker's desk contract but the pairs' words, and has no two entries disagreeing on a slot of it or of the price contract the listing reads. A listing is served only where that answer also writes the price contract it reads (its live source's). Answers that arrive after the other URLs' merge window are not seen until the next poll. Required with `efficient-prop` on Ethereum and BSC, refused on Base. No URL serving BSC is known yet. A route that lands in a block without the maker's update reverts `NoWord`, and the taker pays the gas | `https://<api host>/v1/1/state` |
| `EFFICIENT_PROP_DOOR_CODEHASH` | Optional. The listing doors' creation code hash: a poll that finds the factory answering another ends the feed. Unset, the factory's own answer is taken | |
| `EFFICIENT_PROP_PRICE_SOURCES` | Comma-separated ids of the front's price sources a listing reading a price print may be bound to and be served; unset, only listings reading no print are served. A source's code runs inside the quote and again inside the fill and can tell the two apart, by what the call reads of its transaction and block and by state the fill has already changed when the source runs (the front's lock, the input already at the door), so approve a source only where its code cannot change and its answer comes from its own stored prices and the clock alone; a guarded source needs approval as any other. The same holds of a listing's two tokens, which are not screened: name only tokens in the book configuration whose code cannot change and moves exactly the amounts asked | `<source ids>` |
| `EFFICIENT_PROP_APPROVED_DESKS` | Comma-separated desk contracts (one per maker, the front's `deskCellOf` of a desk id) whose listings are served. Anyone can reserve a listing and post quotes to it, so a listing of any other desk is left out. Unset, nothing is served but what the list below names | |
| `EFFICIENT_PROP_APPROVED_DESKS_URL` | Not served at any URL yet: until a list is, leave it unset and name the desks in `EFFICIENT_PROP_APPROVED_DESKS`, which then rule alone. A URL answering a GET with `{"chainId": 1, "updatedAt": <Unix seconds>, "desks": ["0x…"]}`. The last list read replaces `EFFICIENT_PROP_APPROVED_DESKS` (an empty one approves nobody) until its read is ten minutes old, after which those desks rule less the ones the list left out; a failed read, any status but 200, another chain's answer or an older one changes nothing | |
| `EFFICIENT_PROP_CREDIT_LISTINGS` | The listings with a credit mode, the listings of a stock token's pair (an asset-module pair, whose module's rules run inside every fill), and the listings of a pair and place the feed holds no measured figure for (it holds wstETH/WETH on its pool's hook pair, and wstETH/USDC, wstETH/USDT and USDT/USDC past it, each reading a price print), to serve, each with the gas its fills are charged, comma-separated `listing=custody sell:custody buy:route sell:route buy`: a fill its pool's custody pays, selling the base and buying it, then a fill through the pool's credit route, which borrows or repays at the pool's lender inside the swap. Measure the four on the listing's own pool while it holds some of both tokens, through Ethereum's `PropAMMExecutor`, each less the refund its transaction earns as a single fill (trace it: 19,900 on a custody fill, more on a route), and name the least seen of each over the pool's states: a charge must not be over what a first fill in a fresh block spends, and where a route may draw at more than one lender, name the cheaper one's figure. A listing in no credit mode is charged the custody pair alone: give those two again as its route pair. A fifth figure, optional, is the dearest fill measured on the listing before refunds, on any path and pool state (`...:route buy:dearest before refunds`). The feed refuses one under the dearest of the four plus 19,900: a charge is net of its refund, and every fill traced refunds at least 19,900. The book of a listing with a credit mode names what a swap's call adds to a gas limit (`gas_limit_addition`, that figure with 25% on top) only where you give it, on a pool of its own or on one it shares; the book of a listing in no credit mode sizes that addition from the fifth figure where it is over the custody fill plus 19,900 (`95825:96239:95825:96239:150000` gives 187,500, not 145,174). Unset, no listing with a credit mode, no stock listing and no listing of another pair is served | |
| `FLAMM_FACTORIES` | Efficient pools factories, comma-separated; required when `--pamm` lists `flamm` | |
| `FLAMM_POOLS` | The Efficient pools to serve, comma-separated, whatever their hook and price feed; replaces the lists below | |
| `FLAMM_HOOK_CLASSES` | The hook classes an Efficient pool may run on, comma-separated | |
| `FLAMM_PRICE_FEEDS`, `FLAMM_PRICE_FEED_CODE_HASHES` | The price feeds an Efficient pool may read, by address or by code hash, comma-separated | |

On Ethereum, the deployed values (which `EfficientPropDeployment::ethereum()` and `EfficientPropFeedBuilder::ethereum` hold for a caller building the feed itself, all but the desks and the credit listing's gas) are: `EFFICIENT_PROP_FRONT=0x0000000000403aeB66691063CC2fD42E71d6AcD6`, `EFFICIENT_PROP_FACTORY=0x39eD9993449C96503f6a629cCD2BB5F132D7cc93`, `EFFICIENT_PROP_QUOTE_LAYOUT=builder`, `EFFICIENT_PROP_CELL_CODEHASH=0xb4f7512e65eee793ac5939d62b8383d8544eb1ca850c276fd706e97a0b7938de`, `EFFICIENT_PROP_IMPLEMENTATIONS=0x48C6efab497EeC8501483Ceb23B6623f626d8371` `EFFICIENT_PROP_PRICE_SOURCES=1,2,3,4` (the price contracts of the four pairs listed at the deployment) and `EFFICIENT_PROP_APPROVED_DESKS=0x4f8dd270acE370dE2f6Ac5EBf3230cBE099a0753` (the first desk's contract). The first pool's wstETH/WETH listing borrows at Morpho Blue past its pool's custody: `EFFICIENT_PROP_CREDIT_LISTINGS=1=117990:113508:824095:932904:1134823` (the fifth figure is its dearest fill before refunds, a purchase repaying what the pool owes, so its book's `gas_limit_addition` is 1,418,529), measured on a fork of the deployed contracts with the pool's planned settings; measure again if they change. Its listings follow the first pool's launch; the factory's `doorOf(listing)` gives each listing's door, whose code stands once the door is opened.

On Base, the deployed values (which `EfficientPropDeployment::base()` and `EfficientPropFeedBuilder::base` hold) are the same front, factory and implementation, no `EFFICIENT_PROP_QUOTE_LAYOUT` and no `EFFICIENT_PROP_STATE_URLS`, `EFFICIENT_PROP_CELL_CODEHASH=0x0972dba2b21d750e0679e53404b218ee08e291b24b79194a666701306c1371f1`, `EFFICIENT_PROP_PRICE_SOURCES=1` (the USDe/USDC lane) and `EFFICIENT_PROP_APPROVED_DESKS=0x2b347C8e89520aF26d30e6932f6273B699Ae746A` (the first desk's contract). No fill was measured on Base: name each listing's gas in `EFFICIENT_PROP_CREDIT_LISTINGS`, measured on its pool through Base's `PropAMMExecutor`. No listing exists on Base yet.

On BSC, the front's rehearsal on a fork of the chain (which `EfficientPropDeployment::bnb()` and `EfficientPropFeedBuilder::bnb` hold) puts the same front, factory and implementation, Ethereum's quote layout and desk contract code hash, and the first desk's contract at Ethereum's address, `0x4f8dd270acE370dE2f6Ac5EBf3230cBE099a0753`. None has code on BSC yet, no state URL serves chain 56, and BSC lists no `PropAMMExecutor`, so the feed builds there but serves nothing.

### Command Line Arguments

| Argument                   | Default    | Description                                                                                                      |
|----------------------------|------------|------------------------------------------------------------------------------------------------------------------|
| `--chain`                  | `ethereum` | Blockchain to test against                                                                                       |
| `--protocols`              | -          | Comma-separated list of protocols to test. Defaults to all protocols                                             |
| `--max-simulations`        | `10`       | Maximum simulations (of updated states) per update                                                               |
| `--max-simulations-stale`  | `10`       | Maximum simulations (of stale states) per update per protocol                                                    |
| `--parallel-simulations`   | `5`        | Number of concurrent simulations                                                                                 |
| `--parallel-updates`       | `5`        | Number of concurrent update processors                                                                           |
| `--tvl-threshold`          | chain's medium tier | TVL threshold in native tokens for filtering; without it, the chain's default for about $200K |
| `--tvl-buffer-ratio`       | `1.1`      | A component is removed when its TVL drops below `--tvl-threshold` divided by this                                |
| `--protocol-tvl-thresholds` | -         | Comma-separated `protocol=threshold` pairs. Each one replaces `--tvl-threshold` for its protocol. Every named protocol must be enabled |
| `--metrics-port`           | `9898`     | Port for Prometheus metrics server                                                                               |
| `--disable-onchain`        | `false`    | Skip on-chain protocol testing                                                                                   |
| `--disable-rfq`            | `false`    | Skip the RFQ book feeds (Bebop, Hashflow, Liquorice, Native, Euclid)                                             |
| `--disable-pamm`           | `false`    | Skip the pAMM book feeds. Conflicts with `--pamm`                                                                |
| `--pamm`                   | `metric`   | Comma-separated pAMM venues to run (`metric`, `baibai`, `efficient-prop`, `flamm`); replaces the default list. `baibai` runs on Base only and reads its books over `RPC_URL`; `efficient-prop` (the listing doors) reads its books over `RPC_URL` on Ethereum (its feed refuses to build on Base for now) and needs the `EFFICIENT_PROP_*` variables (`EFFICIENT_PROP_STATE_URLS` on Ethereum only); `flamm` (the pools' own curves) runs on Base, Ethereum and BSC, reads its books over `RPC_URL` and serves what the `FLAMM_*` variables name. A listed venue missing its credentials, or whose feed fails to build, fails the run |
| `--rfq-all-pairs`          | `false`    | Publish each RFQ venue as one component for every pair it prices                                                 |
| `--disable-price-level-stream` | `false` | Skip Titan pAMM price level stream testing (only active on Ethereum)                                            |
| `--price-level-stream-block-interval` | `1` | Emit one sampled price level update per this many blocks                                                    |
| `--price-level-stream-stale-threshold-secs` | `10` | Seconds without a Titan message before marking served pAMMs stale in metrics (0 disables)              |
| `--skip-messages-duration` | `600`      | Book stream skip duration (seconds) after processing a message                                                   |
| `--rpc-poll-attempts`      | `10`       | Attempts to poll the RPC when the update's block is ahead of it                                                  |
| `--rpc-poll-interval-ms`   | `500`      | Milliseconds between those attempts                                                                              |
| `--test-every-n-updates`   | `1`        | Test every Nth protocol update (alias `--test-every-n-blocks`); every update's state is ingested either way    |
| `--partial-blocks`         | `false`    | Stream partial block updates (flashblocks)                                                                       |
| `--stale-threshold-secs`   | `30`       | Seconds without a protocol update before every known protocol is marked stale in metrics (0 disables)            |
| `--min-token-quality`      | -          | Minimum token quality to load (100 when not given)                                                               |
| `--max-days-since-last-trade` | -       | Maximum days since a token last traded (chain-specific default when not given)                                   |
| `--disable-execution`      | `false`    | Simulate only: no swap encoding or execution                                                                     |
| `--bypass-executor-timelock` | `false`  | Mark every known executor as activated in the simulation's state overrides                                       |
| `--always-test-components` | -          | Comma-separated list of component IDs to test every block                                                        |
| `--max-blocks`             | `0`        | Maximum number of blocks to process before exiting (0 = run indefinitely). When set, prints a summary at the end |

## Running Locally

```bash
# Set required environment variables
export RUST_LOG=tycho_integration_test=info,tycho_test=debug,error
export TYCHO_URL=tycho-dev.propellerheads.xyz
export TYCHO_API_KEY=<your-tycho-key>
export RPC_URL=https://your-rpc-endpoint

# Run with default settings
cargo run --package tycho-integration-test

# Run with custom parameters
cargo run --package tycho-integration-test -- \
  --chain ethereum \
  --disable-rfq \
  --disable-pamm \
  --protocols uniswap_v2 \
  --max-simulations 20 \
  --parallel-simulations 10

# Run with specific components to always test
cargo run --package tycho-integration-test -- \
  --always-test-components component_id_1,component_id_2,component_id_3

# Test low-TVL pools of some protocols without lowering the threshold for the others
cargo run --package tycho-integration-test -- \
  --chain base \
  --protocol-tvl-thresholds lunarbase=10,pancakeswap_infinity_cl=3

# Run for a specific number of blocks and get a summary
cargo run --package tycho-integration-test -- \
  --chain ethereum \
  --max-blocks 10 \
  --protocols uniswap_v2,uniswap_v3
```

## Strict packages

A protocol whose package sets `harness: strict` in its `integration_test.tycho.yaml` also gets
limit probes: each tested direction is swapped at 0.1%, 1%, 10% of its limit or the limit itself,
one size per sample in turn, and past the limit as its declared kind claims, and judged with the
declared limits and execution tolerance, the component's own where a test lists it, else the
package's. The yamls are embedded at build time. Outcomes are in
`tycho_integration_check_total` and the "Strict checks" row of the Integration test dashboard
(`grafana/integration-test.json`). See `CLAUDE.md`, "Strict packages".

