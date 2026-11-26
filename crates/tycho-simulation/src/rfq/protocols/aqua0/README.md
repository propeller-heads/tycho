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
or that has no quotable liquidity right now, is left out of the snapshot, which removes it. Aqua0
prices each pool when asked, so a poll reads four pools at a time (`state_concurrency`) rather
than all of them at once.

Each token is sampled at 1, 10, 100, 1,000 and 10,000 whole units unless `sample_sizes` names
other sizes. Levels expire 10 seconds after the backend prices them. Gas per level is Aqua0's
estimate from its recent fills on the chain with the same number of JIT ranges.

Besides the requested samples, `/state` adds a level at or just above Aqua0's minimum trade size,
and one at 98% of the largest size Aqua0 can fully back at that moment, so a hold placed after the
poll rarely makes it fail. Prices are linear between backed levels. An amount below the smallest
backed level is refused, and so is one above the largest. `min_tvl_usd` compares the largest
backed fill each way, valued in whichever token of the pair is in the builder's USD stablecoins; a
pair with neither clears only a zero floor.

A malformed entry in `/markets` is skipped with one warning for the list, and a list where every
entry is malformed is a failed read. While `/markets` fails, the last list is used for up to five
refresh periods and read again with a growing wait. A poll where every market with liquidity fails
to load or to pass its checks counts as a failed poll and backs off. Markets with no liquidity do
not count either way.

Before a market is published, the client checks that `/state` describes the listed pool and
class, that every address is 20 bytes, and that `keccak256(abi.encode(PoolKey))` from the
returned currencies, fee, tick spacing and hook equals the listed pool id. A market that fails a
check is logged and skipped; the rest of the poll is published. Pools with V4's dynamic-fee flag
are refused.

The client trusts the Aqua0 backend for the hook address: the pool key check proves that `/state`
and `/markets` agree with each other, not that the hook is Aqua0's.

## Binding quotes

`POST /quote` returns signed `hookData` for one exact-input fill. A quote can be filled for 45
seconds. Its LP backing stays reserved for about 60 seconds, until 15 seconds after that deadline.
It is requested inside `encode_solutions`, so encode an Aqua0 route only to execute it. If anything
in front of the API answers `429`, the client stops requesting approvals and publishes no Aqua0
markets until `Retry-After` has passed (at most five minutes), and an encode in the meantime fails
with a recoverable error. There is no call to release a quote; its hold ends on its own, or at once
if Aqua0 refuses the quote.

Before it asks for the quote, the encoder prices the amount on the state (positive, within the
backed sizes) and runs the V4 encoding once without hook data, so a swap that cannot be encoded
reserves nothing.

The quote is accepted only if it matches the request: schema, request id, component, chain, tokens,
exact input, router and executor (both the Tycho router), a positive output, non-empty hook data and
an unexpired deadline. The deadline travels with the quote as `deadline_timestamp`, and the
encoder checks it again. The caller's V4 `user_data` (such as `skip_unlock`) is kept; its
`hook_data` must be empty, because the quote supplies it.

## Execution

The hook binds the exact input, so a route with an Aqua0 swap must keep these rules:

- One Aqua0 swap, the first hop of the whole route.
- No splits.
- The Aqua0 swap's `estimated_amount_in` equals the solution's input.

The encoder checks only what it sees in its own swap: it is first in its group, it is not split, and
its `estimated_amount_in` is non-zero. The route-level rules are the solver's to keep. A route that
breaks them still gets a binding quote, which holds backing until its hold ends, and then reverts on
chain instead of filling at another amount. In simulation, the state a fill returns quotes nothing
more, so the same market cannot fill twice in a route.

Several open quotes on one market are fine, from one `encode_solutions` call or several. Each new
quote is priced as if the open quotes before it on that market fill: a later quote in the same
direction is priced past the earlier ones, so each can fill when they land in the order they were
signed. Two cases still revert in the hook's `beforeSwap`: an earlier same-direction quote that
lands after a later one (`RangeNotSingleSided` or `PositionNotOnOutputSide`), and a quote in the
other direction that lands after a fill it was signed before (`PositionOverdrawn`). Quotes also
compete for backing, also when they come from the same caller, and Tycho encodes the solutions of
one `encode_solutions` call in parallel. A quote that does not fit the backing left after the open
ones is refused, and then the whole call fails. A quote that reverts or goes unused keeps its
backing held until its hold ends.

Set the gas limit at least 30% above Tycho's gas estimate, or use `eth_estimateGas` on the encoded
transaction. The fill adds and removes its JIT ranges inside the swap, so it needs more gas while
it runs than it is billed for after the refund. The recorded one-range fill on Arbitrum is
estimated at about 758k gas and billed about 731k, but reverts with a gas limit below about 920k.
That was measured on a one-range fill, so for fills with more ranges `eth_estimateGas` is safer.

## Hook errors

A failed Aqua0 fill reverts with one of the hook's errors, or one of the asset vaults it settles
through in `afterSwap`. It reaches the router inside Uniswap V4's
`WrappedError(address,bytes4,bytes,bytes)`, which the V4 executor passes up inside an
`Error(string)`. `revert::decode_revert` looks through both and returns the error with its
arguments and the callback it came from; `cause()` names the cause in plain words.

Raised in `beforeSwap`:

| Error | Selector | When it fires |
|---|---|---|
| `EnforcedPause()` | `0xd93c0665` | The hook is paused. A paused vault raises it in `afterSwap`. |
| `UnsignedSwap()` | `0xfbdd5f9b` | The hook data carries no Aqua0 quote. |
| `InvalidSignature()` | `0x8baa579f` | The quote names another pool, or Aqua0 did not sign it. |
| `JitPositionPaused()` | `0x3c853ab7` | Aqua0 paused the pool. |
| `JitSwapMismatch()` | `0x2ecbaf3f` | The swap's direction, exact input or price limit differs from the quote. |
| `JitExecutorMismatch(address,address)` | `0xa2f78b35` | The swap came through another router than the quote names. |
| `ClassNotPool(bytes32,uint256)` | `0xa1380257` | The quote's strategy is not this pool's. |
| `ExpiredDeadline(uint256,uint256)` | `0xc1f738dc` | The quote's 45 second deadline passed. |
| `ECDSAInvalidSignature()` | `0xf645eedf` | Malformed signature. |
| `ECDSAInvalidSignatureLength(uint256)` | `0xfce698f7` | Malformed signature. |
| `ECDSAInvalidSignatureS(bytes32)` | `0xd78bce0c` | Malformed signature. |
| `NonceUsed(uint256)` | `0x1be5cb2f` | The quote already filled once. |
| `NoRanges()` | `0x6a05e465` | The quote names no positions. |
| `TooManyRanges(uint256,uint256)` | `0x34f94fea` | The quote names more positions than the hook allows. |
| `DrawsNotSorted(address,uint8)` | `0xa2e9ebdb` | The quote's positions are out of order. |
| `TokenHasNoVault(address)` | `0x0a057e82` | A pool token has no Aqua0 vault. |
| `RangeNotCommitted(uint256,address,uint8)` | `0x8f9dff3c` | The LP changed or removed its range since the quote. |
| `RangeNotSingleSided(address,uint8)` | `0x1a0fc9ca` | An earlier fill moved the pool's price into the quoted range. |
| `PositionNotOnOutputSide(address,uint8)` | `0xfbeb6447` | An earlier fill moved the pool's price past the quoted range. |
| `PositionOverdrawn(address,uint8)` | `0x9da84719` | The position no longer backs the quote at the pool's price: backing was used, or the pool moved. |
| `ReentrancyGuardReentrantCall()` | `0x3ee5aeb5` | Re-entry. |
| `NotPoolManager()` | `0xae18210a` | Something other than the pool manager called the hook. |

Raised in `afterSwap`, by the hook or a vault it settles through:

| Error | Selector | When it fires |
|---|---|---|
| `FillOutsideItsPositions(uint160)` | `0x8bf35716` | The swap ended outside the quoted ranges. |
| `RanOutOfBacking(uint256,uint256)` | `0xdc7c66a9` | The LP's backing is below what the fill sells. |
| `InsufficientSharedIdle(uint256,uint256)` | `0x0135bcdc` | The vault's idle balance is below the fill. |
| `TooManyDraws(uint256,uint256)` | `0x92fde8f4` | More positions than the vault settles in one fill. |
| `ClassNotRegistered(uint256)` | `0xf1f99109` | The pool's strategy is not registered on the vault. |
| `ClassIsPaused(uint256)` | `0x919a8006` | The strategy is paused on the vault. |
| `StrategyVenueBound(uint256,address)` | `0x7dd3ea88` | The strategy is bound to another adapter. |
| `AccessControlUnauthorizedAccount(address,bytes32)` | `0xe2517d3f` | The hook lacks the vault's settler role. |
| `ZeroAmount()` | `0x1f2a2005` | A zero draw or credit. |
| `ZeroAddress()` | `0xd92e233d` | A zero recipient. |
| `SafeERC20FailedOperation(address)` | `0x5274afe7` | A token transfer failed. |

Anything else, such as a pool manager error or an empty revert, comes back as raw bytes.

## Chains

- Chains: Base, Arbitrum, Polygon, Robinhood and Arc. Each has an `rfq:aqua0` entry in
  tycho-execution's `config/executor_addresses.json`.
- That entry is the chain's `uniswap_v4` executor; there is no Aqua0 executor contract to deploy.

```bash
cargo test -p tycho-simulation --lib aqua0
cargo test -p tycho-execution --lib aqua0
```
