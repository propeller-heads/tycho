# base-biconomy

Indexes the Biconomy PropAMM venue on Base as one component. The venue is a push-payment
`IPropAMM` contract that serves several pairs from one address and fills against the boards
makers keep in the PropAMM executor.

| | Base |
|---|---|
| Venue (`PropAMMVenue`) | `0x0000009E5F98cDB4dd1E1d2E15B220217c88B3b8` |
| Executor (`PropAMMExecutor`) | `0x0000006f9bBA066e4efda2e0CAbE4FF8cf9030aC` |
| Tokens | WETH `0x4200000000000000000000000000000000000006`, USDC `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913`, cbBTC `0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf`, EURC `0x60a3E35Cc302bFA44Cb288Bc5a4F316Fdb1adb42`, USDT `0xfde4C96c8593536E31F229EA8f37b2ADa2699bb2`, cbETH `0x2Ae3F1Ec7F1F5012CFEab0185bfc7aa3cf0DEc22`, wstETH `0xc1CBa3fCea344f92D9239c08C0568f6F2F0ee452`, AERO `0x3f5bF3EdAc59d650D8fDf9657a71F2f904A3a4Ee`, and 10 tokenized stocks: MSTRc `0xb2000000000000000000004884b426556b92883d`, METAc `0xb2000000000000000000008bC8786B856E61707C`, SNDKc `0xb200000000000000000000397293Cb8cda9a10c5`, NVDAc `0xb20000000000000000000078ee7ce2fE4908108C`, MUc `0xb200000000000000000000Fd2f87532B90095211`, GOOGLc `0xb2000000000000000000002D0BA3164cc74f58B7`, AMZNc `0xb200000000000000000000d9192b6B456483C2E8`, SPCXc `0xb2000000000000000000007b9fcbd005511aCBd5`, MSFTc `0xB200000000000000000000Ab99cFa739E253872B`, TSLAc `0xb2000000000000000000001e800a7f5189430cD0` |
| Executor deployed | block 52547671 |
| Venue deployed | block 52547680 |

## Component

- id: the venue address
- tokens: the `tokens` param. It covers the pairs `getPairs()` lists (WETH, USDC, cbBTC) and
  assets planned for listing (EURC, USDT, cbETH, wstETH, AERO and 10 tokenized stocks), so
  adding those pairs needs no resync as long as an already configured provider or vault holds
  them.
- static attributes: `pamm_address` (the venue) and `executor`
- protocol system `biconomy`, type `biconomy_venue`
- balances: per token, the inventory the configured providers can pay out (see below). The venue
  itself holds nothing between swaps.

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
| `provider/{provider}` | provider config from params: kind and vault |
| `signer/{provider}` | the provider's `signer` storage word (bootstrap value from params) |
| `approved_executor/{provider}` | the provider's `approvedExecutor` storage word (bootstrap value: `executor`) |
| `router_count/{vault}`, `router/{vault}` | the vault's router array length and first element, which `router()` returns |
| `balance/{token}/{holder}` | the token's `balanceOf` storage word for a provider or vault |
| `allowance/{token}/{owner}/{spender}` | the token's `allowance` storage word, vault to provider |

Board, anchor and pause words come from the transaction's storage changes. Every write to them
comes with an executor event naming the maker and pair (`LadderCommitted`, `OffsetsCommitted`,
`AnchorCommitted`, `ControlsCommitted`, `MMFillExecuted`, `MakerPaused`), which is how a
storage slot is matched to its board.

Commits on the executor are permissionless, so the package keeps an event only when its maker is
in the venue's `makerList` at that transaction and both tokens are in the `tokens` param. The
venue lists a maker before the maker commits, so nothing is lost in the normal flow.

Writes made while a maker is not listed (before its first listing, or between a removal and a
re-listing) are dropped for good. A later depth commit rewrites the board's header, meter and
levels, but not its controls (block cap, widening, premium), its pair anchor or the maker's pause
flag. So a maker that sets controls, moves its anchor or pauses while unlisted is indexed without
them once listed: the simulation can then quote it with no block cap or widening, or while it is
paused, and overstate what the venue fills. Biconomy re-lists a removed maker only after it
recommits controls and anchor and unpauses; otherwise the package needs a resync from 52547671.

`map_state_writes` collects the venue, provider, vault and token writes and `store_state` keeps
their latest values, which `map_protocol_changes` reads to check the maker list and to compute
balances at each transaction.

## Provider inventory, signer and executor

The executor caps a board's depth at its provider's `available(tokenOut)` and keeps a board dark
unless the provider's `signer()` is the maker. It does not check `approvedExecutor`: the venue
still routes to a maker whose provider dropped this executor, and the swap reverts with
`FillLegFailed`. The simulation does the same: the board stays in the merge, and a quote whose
plan allocates to it fails, as does `get_limits`. Inventory comes from the provider config in the
params and the tracked storage:

| kind | `available(token)` |
|---|---|
| `balance` | the provider's own token balance |
| `vault` | the smaller of the vault's balance and its allowance to the provider, and zero unless the vault's `router()` is the provider |
| `unbounded` | no cap |

The component balance of each token is the sum of `available(token)` over the configured
providers (unbounded providers add nothing), emitted whenever a tracked balance, allowance or
router changes.

Providers whose owner can change the signer or executor are listed in `provider_slots` with the
two storage slots, so the package follows those writes. Providers without an entry are taken as
immutable: the signer in `providers` and `executor` hold for good. A vault in `vault_routers`
keeps `router()` as the first element of a dynamic array at the given slot (slot 2 for the
Tempest vault), and the package follows that array. Both providers configured today keep the
signer in slot 0 and `approvedExecutor` in slot 1.

Token balance and allowance slots are given per token in the `tokens` param. On Base: WETH 3 and
4; USDC, cbBTC and EURC 9 and 10; USDT 0 and 1; wstETH 1 and 2; cbETH 51 and 52. AERO (the token
Aero launched on 2026-10-21) is on Solady's `ERC20`, which derives both slots from fixed seeds
(`address:solady`). The tokenized stocks are B20 tokens, Base's node-native standard (their code
is the single byte `0xef`): balances and allowances are mappings at offsets 4 and 5 of the
`base.b20` ERC-7201 namespace (`address:b20`), as laid out in `base/base`
(`crates/common/precompiles/src/common/core_storage.rs`). Each was checked against holders'
`balanceOf` and `allowance` on chain. A B20 asset also carries an ERC-8056 `multiplier`;
`balanceOf` and transfers are in raw units, so the package and the venue never apply it. USDC
and EURC keep a blacklist flag in the top bit of the balance word; the package and the simulation
mask that bit on every token's balance word. Balances and allowances that predate `bootstrap_block` go in the
`balances` and `allowances` params; none do for the providers configured today.

A provider that is not in the params has no inventory, so its boards stay dark: the package can
miss a quote there but not overstate depth. The one case where it can overstate is a maker's
writes while unlisted, above.

## Adding a provider or a token

Provider configs, bootstrap values and the component's tokens are emitted once, in the creation
transaction. A running extractor does not pick up a params change, so adding a provider, a vault
or a token means:

1. Add it to the params (`providers`, and `provider_slots` or `vault_routers` where they apply;
   `tokens` for a new token).
2. Add `balances` and `allowances` bootstrap values for any provider or vault that held funds at
   `bootstrap_block`. A provider deployed after it needs none: its storage writes are indexed.
3. Resync the package from block 52547671.

Biconomy deploys a new provider when a maker rotates its signer or moves to a new executor
generation, so each of these is a resync.

## Update timing

Updates arrive per flashblock on Tycho's Base stream. Prices also move with time inside a board
(anchor drift, quote-age widening, premium window, expiry), which the simulation applies from
the execution block's number and timestamp without a state update.
