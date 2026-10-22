# base-biconomy

Indexes the Biconomy PropAMM venue on Base as one component. The venue is a push-payment
`IPropAMM` contract that serves several pairs from one address and fills against the boards
makers keep in the PropAMM executor.

| | Base |
|---|---|
| Venue (`PropAMMVenue`) | `0x0000009E5F98cDB4dd1E1d2E15B220217c88B3b8` |
| Executor (`PropAMMExecutor`) | `0x0000006f9bBA066e4efda2e0CAbE4FF8cf9030aC` |
| Tokens | WETH `0x4200000000000000000000000000000000000006`, USDC `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913`, cbBTC `0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf` |
| Executor deployed | block 52547671 |
| Venue deployed | block 52547680 |

## Component

- id: the venue address
- tokens: every token the venue lists in `getPairs()`
- static attributes: `pamm_address` (the venue) and `executor`
- protocol system `biconomy`, type `biconomy_venue`

## State

The `biconomy` state in `tycho-simulation` decodes these attributes and prices a swap exactly
as `PropAMMVenue.quote` does at the execution block. The package makes no RPC calls: every
value comes from storage changes or from the package params, so it runs on Flashblocks.

| attribute | source |
|---|---|
| `board/{mm}/{tokenIn}/{tokenOut}/{slot}` | executor storage word `slot` (0 to 23) of `boards[mm][tokenIn][tokenOut]` |
| `anchor/{mm}/{token0}/{token1}` | executor storage word of `anchors[mm][token0][token1]` |
| `paused/{mm}` | executor storage word of `makerPaused[mm]` |
| `fee_bps` | venue storage slot 0 (`feeBps` is its low 16 bits) |
| `maker_count`, `maker/{i}` | venue `makerList` length (slot 1) and elements |
| `provider/{provider}` | provider config from params: kind, signer and vault |
| `balance/{token}/{holder}` | the token's `balanceOf` storage word for a provider or vault |
| `allowance/{token}/{owner}/{spender}` | the token's `allowance` storage word, vault to provider |

Board, anchor and pause words come from the transaction's storage changes. Every write to them
comes with an executor event naming the maker and pair (`LadderCommitted`, `OffsetsCommitted`,
`AnchorCommitted`, `ControlsCommitted`, `MMFillExecuted`, `MakerPaused`), which is how a
storage slot is matched to its board.

## Provider inventory and signer

The executor caps a board's depth at its provider's `available(tokenOut)` and keeps a board dark
unless the provider's `signer()` is the maker. The simulation computes both from the provider
config in the params and the tracked token storage:

| kind | `available(token)` |
|---|---|
| `balance` | the provider's own token balance |
| `vault` | the smaller of the vault's balance and its allowance to the provider |
| `unbounded` | no cap |

Token balance and allowance slots are given per token in the `tokens` param (WETH 3 and 4, USDC
and cbBTC 9 and 10 on Base; USDC keeps a blacklist flag in the top bit of the balance word,
which the simulation masks). Balances and allowances that predate `bootstrap_block` go in the
`balances` and `allowances` params; none do for the providers configured today.

A provider that is not in the params has no inventory, so its boards stay dark: the package can
miss a quote but never overstates depth. Adding a maker's provider is a params change to this
package.

## Update timing

Updates arrive per flashblock on Tycho's Base stream. Prices also move with time inside a board
(anchor drift, quote-age widening, premium window, expiry), which the simulation applies from
the execution block's number and timestamp without a state update.
