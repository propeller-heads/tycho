# base-biconomy

Indexes the Biconomy PropAMM venue on Base as one component. The venue is a push-payment
`IPropAMM` contract that serves several pairs from one address and fills against the boards
makers keep in the PropAMM executor.

| | Base |
|---|---|
| Venue (`PropAMMVenue`) | `0x000000Da21a0f02b2626874870b6447Db220C1EF` |
| Executor (`PropAMMExecutor`) | `0x000000d4d7CB15E0FA9aB2B1fd49ca8537CDCA26` |
| Tokens | WETH `0x4200000000000000000000000000000000000006`, USDC `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913`, cbBTC `0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf` |
| Executor deployed | block 52327804 |
| Venue deployed | block 52327812 |

## Component

- id: the venue address
- tokens: every token the venue lists in `getPairs()`
- static attributes: `pamm_address` (the venue) and `executor`
- protocol system `biconomy`, type `biconomy_venue`

## State

The `biconomy` state in `tycho-simulation` decodes these attributes and prices a swap exactly
as `PropAMMVenue.quote` does at the execution block.

| attribute | source |
|---|---|
| `board/{mm}/{tokenIn}/{tokenOut}/{slot}` | executor storage word `slot` (0 to 23) of `boards[mm][tokenIn][tokenOut]` |
| `anchor/{mm}/{token0}/{token1}` | executor storage word of `anchors[mm][token0][token1]` |
| `paused/{mm}` | executor storage word of `makerPaused[mm]` |
| `fee_bps` | `venue.feeBps()` |
| `makers` | `venue.makers()`, 20 bytes per maker, in registry order |
| `inventory/{provider}/{token}` | `provider.available(token)` |

Board, anchor and pause words come from the transaction's storage changes. Every write to them
comes with an executor event naming the maker and pair (`LadderCommitted`, `OffsetsCommitted`,
`AnchorCommitted`, `ControlsCommitted`, `MMFillExecuted`, `MakerPaused`), which is how a
storage slot is matched to its board.

The venue's fee and maker list are read over RPC at the end of any block that changed the
venue's storage.

## Provider inventory

A board's depth is capped by what its provider reports through `available(tokenOut)`. That
value lives in the maker's own contract, so it is read over RPC at the end of a block in which
the provider:

- was bound to a board or filled through,
- or sent or received one of the venue's tokens, or had an allowance set, either directly or
  through the vault it returns from `vault()`.

This covers the provider types in use: balance-based providers, unbounded providers and router
vault providers. A provider whose `available()` depends on anything else is refreshed only on
those triggers. A failing `available()` call is stored as unbounded, as the executor treats it.
The executor calls `available()` with a 50,000 gas budget; the RPC read has none, so a provider
whose view needs more than that would read differently here than on chain.

## Update timing

State is read per block, so quotes move at block boundaries, not per flashblock, the same as
for other aggregators that index the venue. Prices also move with time inside a board (anchor
drift, quote-age widening, premium window, expiry), which the simulation applies from the
execution block's number and timestamp without a state update.
