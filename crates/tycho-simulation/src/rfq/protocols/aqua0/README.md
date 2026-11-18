# Aqua0 RFQ

Aqua0 adds just-in-time liquidity to Uniswap V4 pools that carry its hook. Its backend chooses the
ranges and prices each fill; Tycho reads those prices and, when it encodes a route, asks for a
binding approval that the existing Router V3 and Uniswap V4 executor carry to the hook.

## Feed

```rust,ignore
use tycho_simulation::{
    book::{quote_tokens::usd_stablecoins_for_chain, BookFeedConfig},
    rfq::protocols::aqua0::feed::Aqua0FeedBuilder,
};

// Polygon and Arc have no curated set; pass your own there (e.g. USDC), or every pair
// clears only a zero `min_tvl_usd`.
let usd = usd_stablecoins_for_chain(chain).unwrap_or_default();
let feed = Aqua0FeedBuilder::new(BookFeedConfig { chain, tokens, min_tvl_usd }, usd).build()?;
feeds.add("aqua0", feed);
```

The feed takes a chain and the token universe. Every poll it reads `GET /markets?chainId=`
(refreshed every minute, `markets_refresh`), then `GET /state` for each listed market whose tokens
are in the universe, and publishes one component per market. A market the backend stops listing,
or that has no quotable liquidity right now, is left out of the snapshot, which removes it.

Each token is sampled at 1, 10, 100, 1,000 and 10,000 whole units unless `sample_sizes` names
other sizes. Levels expire 10 seconds after the backend prices them. Gas per level is Aqua0's
estimate from its recent fills on the chain with the same number of JIT ranges.

Prices are linear between backed samples. An amount below the smallest backed sample, which is at
or above Aqua0's minimum trade size, is refused, and so is one above the largest. `min_tvl_usd`
compares the largest backed fill each way, valued in whichever token of the pair is in the
builder's USD stablecoins; a pair with neither clears only a zero floor.

A malformed entry in `/markets` is skipped. While `/markets` fails, the last list is used for up to
five refresh periods and read again with a growing wait. A poll where every market fails to load
or to pass its checks counts as a failed poll and backs off.

Before a market is published, the client checks that `/state` describes the listed pool and
class, that every address is 20 bytes, and that `keccak256(abi.encode(PoolKey))` from the
returned currencies, fee, tick spacing and hook equals the listed pool id. A market that fails a
check is logged and skipped; the rest of the poll is published. Pools with V4's dynamic-fee flag
are refused.

The client trusts the Aqua0 backend for the hook address: the pool key check proves that `/state`
and `/markets` agree with each other, not that the hook is Aqua0's.

## Binding quotes

`POST /quote` reserves LP backing for one exact-input fill until its deadline (45 seconds) and
returns signed `hookData`. It is requested inside `encode_solutions`, so encode an Aqua0 route only
to execute it. If anything in front of the API answers `429`, the client stops requesting
approvals and publishes no Aqua0 markets until `Retry-After` has passed (at most five minutes), and
an encode in the meantime fails with a recoverable error. There is no call to release a quote;
its reservation ends at the deadline. Quotes compete for the same backing whoever asks for them.

Before it asks for the quote, the encoder prices the amount on the state (positive, within the
backed sizes) and runs the V4 encoding once without hook data, so a swap that cannot be encoded
reserves nothing.

The quote is accepted only if it matches the request: schema, request id, component, chain, tokens,
exact input, router and executor (both the Tycho router), a positive output, non-empty hook data and
an unexpired deadline. The deadline travels with the quote as `deadline_timestamp`, and the
encoder checks it again. The caller's V4 `user_data` (such as `skip_unlock`) is kept; its
`hook_data` must be empty, because the quote supplies it.

## Execution

- Exact input, same chain, no split routes.
- One Aqua0 leg, the first hop, whose input equals the whole solution's input.
- Simulation enforces this only in part: the state a fill returns quotes nothing more, so the same
  market cannot fill twice in a route, but two different Aqua0 markets in one route simulate fine
  and are refused at encoding.
- Chains: Base, Arbitrum, Polygon, Robinhood and Arc (`AQUA0_CHAINS` in tycho-execution's encoder
  registry).
- The `rfq:aqua0` encoder reuses the chain's `uniswap_v4` executor; there is no Aqua0 executor
  contract to deploy.

```bash
cargo test -p tycho-simulation --lib aqua0
cargo test -p tycho-execution --lib aqua0
```
