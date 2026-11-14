# Aqua0 RFQ

Aqua0 adds just-in-time liquidity to Uniswap V4 pools that carry its hook. Its backend chooses the
ranges and prices each fill; Tycho reads those prices and, when it encodes a route, asks for a
binding approval that the existing Router V3 and Uniswap V4 executor carry to the hook.

## Feed

```rust,ignore
use tycho_simulation::{
    book::BookFeedConfig,
    rfq::protocols::aqua0::feed::Aqua0FeedBuilder,
};

let feed = Aqua0FeedBuilder::new(BookFeedConfig { chain, tokens, min_tvl_usd: 0.0 }).build()?;
feeds.add("aqua0", feed);
```

The feed takes a chain and the token universe. Every poll it reads `GET /markets?chainId=`
(refreshed every minute, `markets_refresh`), then `GET /state` for each listed market whose tokens
are in the universe, and publishes one component per market. A market the backend stops listing,
or that has no quotable liquidity right now, is left out of the snapshot, which removes it.

Each token is sampled at 1, 10, 100, 1,000 and 10,000 whole units unless `sample_sizes` names
other sizes. Levels expire 10 seconds after the backend prices them. Gas per level is Aqua0's
estimate from its recent fills on the chain with the same number of JIT ranges.

Before a market is published, the client checks that `/state` describes the listed pool and
class, that every address is 20 bytes, and that `keccak256(abi.encode(PoolKey))` from the
returned currencies, fee, tick spacing and hook equals the listed pool id. A market that fails a
check is logged and skipped; the rest of the poll is published.

## Binding quotes

`POST /quote` reserves LP backing for one exact-input fill until its deadline (45 seconds) and
returns signed `hookData`. It is requested inside `encode_solutions`, so encode an Aqua0 route only
to execute it. If anything in front of the API answers `429`, the client stops requesting
approvals and publishes no Aqua0 markets until `Retry-After` has passed, and an encode in the
meantime fails with a recoverable error.

The quote is accepted only if it matches the request: schema, request id, component, chain, tokens,
exact input, router and executor (both the Tycho router), a positive output, non-empty hook data and
an unexpired deadline.

## Execution

- Exact input, same chain, no split routes.
- One Aqua0 leg, the first hop, whose input equals the whole solution's input.
- The `rfq:aqua0` encoder reuses the chain's `uniswap_v4` executor; there is no Aqua0 executor
  contract to deploy.

```bash
cargo test -p tycho-simulation --lib aqua0
cargo test -p tycho-execution --lib aqua0
```
