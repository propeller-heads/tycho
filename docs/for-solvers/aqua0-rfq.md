# Aqua0 RFQ

Aqua0 is exposed to Tycho as `rfq:aqua0`. It is an indicatively priced RFQ component, not an
on-chain indexed AMM. Aqua0's backend selects the JIT ranges, reserves the backing, and signs the
Uniswap V4 hook data. Tycho reconstructs the advertised levels and executes a binding quote through
its existing Router V3 and Uniswap V4 executor.

## Supported execution chains

| Chain | Chain ID | Router V3 and V4 executor |
|---|---:|---|
| Base | 8453 | Supported |
| Arbitrum | 42161 | Supported |
| Polygon | 137 | Supported |
| Robinhood Chain | 4663 | Supported |

Avalanche, Celo, and Monad are Aqua0 chains, but the Tycho revision pinned by this integration has no
matching chain configuration and execution deployment for them. Aqua0 state can still be read from the
backend on those chains, but `Aqua0Client` refuses to construct an executable Tycho market there.

Tycho's hosted data endpoints are a separate concern. Their smaller chain list does not determine where
an RFQ client can use a deployed Router V3.

## Backend API

The client base URL ends at `/api/tycho/rfq` and exposes:

- `GET /chains`: support and router map.
- `GET /state`: a short-lived full snapshot for one `(chainId, poolId, classId)` market. It contains
  fully backed sampled levels, backend-selected ranges, and serialized route plans. Active global
  backing and withdrawal reservations are subtracted before levels are built.
- `POST /quote`: an exact-input binding request. It re-quotes, atomically reserves backing through
  Aqua0's existing fleet ledger, and returns adapter-valid `hookData` signed for the configured Router
  V3.

State reads use `X-API-Key`. Binding quotes use `X-Operator-Key` because they reserve capital and mint a
short-lived signature.

## Client registration

Each client represents one pool and class. Add more clients when a solver wants more Aqua0 markets.

```rust
use tycho_common::models::Chain;
use tycho_simulation::rfq::{
    protocols::aqua0::{
        client_builder::Aqua0ClientBuilder,
        models::Aqua0Market,
        state::Aqua0State,
    },
    stream::RFQStreamBuilder,
};

let market = Aqua0Market {
    pool_id: "0xbf948948cda5a93e50edb496cf32e565c46d8c7335c6bac7854a8d95eab25375".into(),
    class_id: "1".into(),
    amount0_samples: vec!["1000000000000000".into(), "10000000000000000".into()],
    amount1_samples: vec!["1000000".into(), "10000000".into()],
};

let aqua0 = Aqua0ClientBuilder::new(
    Chain::Base,
    "https://api.example.com/api/tycho/rfq".into(),
    market,
)
.credentials(read_key, operator_key)
.build()?;

let rfq_stream = RFQStreamBuilder::new()
    .set_tokens(token_metadata)
    .await
    .add_client::<Aqua0State>("aqua0", Box::new(aqua0));
```

When a snapshot is decoded from serialized Tycho state, the decoder reads
`AQUA0_RFQ_API_KEY` and `AQUA0_RFQ_OPERATOR_KEY` for its reconstructed binding client.

## Routing constraints

The current Aqua0 V4 authorization binds an exact input amount and the Router V3 address. Therefore:

- Exact input only.
- Same chain only.
- Aqua0 must be the first hop or the only hop.
- One Aqua0 leg per solution.
- Amounts above the largest fully backed sample are rejected rather than extrapolated.
- Expired state is rejected before simulation or binding.

`Aqua0SwapEncoder` obtains the binding quote at encoding time, reads its `hook_data`, and delegates byte
packing to Tycho's existing `UniswapV4SwapEncoder`. The configured executor address is the existing
Uniswap V4 executor. No custom router or Solidity executor is introduced.

## Deployment requirement

The Aqua0 adapter defaults to a closed router allowlist. An Aqua0 admin must allow the official Router
V3 on every enabled chain:

```solidity
adapter.setJitTriggerRouterAllowed(tychoRouterV3, true);
```

The Aqua0 contracts repository's `ConfigureV4Router` timelock script supports Tycho's delegatecall
shape. Set `V4_EXECUTOR` to Router V3 and `V4_DELEGATE_EXECUTOR` to Tycho's Uniswap V4 executor. The
script verifies that the delegated executor is active in Router V3 and that its PoolManager matches the
Aqua0 adapter before it schedules the allowlist operation.

For Base, the current addresses at the pinned Tycho revision are:

- Router V3: `0x9bA632d83e9eF57571256Cf4cc951b8aF1158e9C`
- Uniswap V4 executor: `0x78db9684220541601E9215bB16b219e5DF6cF0fb`
- Aqua0 adapter: `0xACaF2945890AB6caea62bDa459d1922532A500C8`

The official Router V3 is not currently allowlisted by the deployed Base Aqua0 adapter. That mainnet
admin transaction is intentionally outside this source integration and must happen before enabling live
order flow.

## Focused verification

```bash
cargo +1.91.1 test -p tycho-simulation rfq::protocols::aqua0 --lib
cargo +1.91.1 test -p tycho-execution aqua0 --lib

cd crates/tycho-execution/contracts
forge test --match-path test/Aqua0TychoBaseFork.t.sol --use 0.8.33 -vv
```

The Aqua0 contracts repository also contains the composed Base fork proof. It executes one signed JIT
swap through the official deployed Router V3 and V4 executor, the real Base PoolManager, and a local
Aqua0 adapter and vault fleet.
