# BopAMM (Bebop PMM) — Tycho Integration Handover

Everything learned about BopAMM's on-chain design, its interfaces, and how this Substreams
package indexes it. Written so the next engineer can pick up the remaining work (simulation
adapter, quote-override stream, extractor registration) without re-deriving the protocol.

> All facts below were verified on Ethereum mainnet (archive node + Tenderly + live
> `substreams run`). Addresses are mainnet. The venue is **live but intermittent** (369
> `Swap`s in its lifetime, active blocks ~25.19M–25.27M, idle between maker commits).

---

## 1. What BopAMM is

BopAMM is Bebop's on-chain **PMM (proxy/principal market maker)**. It is **not** a
constant-function AMM: there are no per-pair pool contracts and no reserves held in a pool.
A single **settlement** contract pulls inventory from one **market-maker wallet** and prices
each swap from quote "lanes" the operator commits to a **registry**. It is RFQ-like in
spirit, but — and this is the reason we index it as a Tycho **VM** protocol — the executable
price is a pure function of on-chain state (registry lanes + module config), so we can
reproduce quotes locally from indexed storage without calling Bebop's API.

This package is **distinct from the existing Bebop RFQ integration** in
`tycho-simulation/src/rfq/protocols/bebop/` (that is Bebop's off-chain RFQ product). This one
is named `bopamm`.

---

## 2. Contracts and roles

| Address | Role |
|---|---|
| `0xdB13ad0fcD134E9c48f2fDaEa8f6751a0F5349ca` | **BopAmmV2** — settlement / swap entrypoint. Holds **no** inventory. |
| `0xBC60639345dFa607d73b74e88C2d54D8B8AD7Cc3` | **Pricing module** — asset config, pricing math, global maker slot. EIP-1271/712 signer. |
| `0xDa7AfeeD01fe625CF15d187a19f94B45f00b8C5F` | **PrioUpdateRegistry** — per-book quote lanes. Emits **no events**. |
| `0x6f7a3714d7Fc266e3e84067ac31E7b1A3bE18060` | **Maker** — single global inventory EOA (provides buy token, receives sell token). |
| `0xC5531177169b4576553Df6d4B4e176d0d7C3C826` | **Owner** — hardcoded/immutable; configures the module + registry. |

All three protocol contracts are **unverified** on Etherscan. The takers in observed
settlements are CoW Protocol solvers (`0x9008d19f…0ab41`) — BopAMM runs as a CoW PMM.

### Tokens / books
USDC (`0xA0b8…eb48`) is the **hub/quote**. Each book is `asset/USDC`, keyed by `uint8`
`assetId`: **0 = WETH** (`0xC02a…56cc2`), **1 = WBTC** (`0x2260…C599`). Native ETH is wrapped
to WETH internally. **`bookId == assetId`** (verified: a WETH swap calls `updateState(bookId=0)`
and the WETH asset config is at `assetId=0`).

---

## 3. Interfaces

### BopAmmV2 (settlement)
| Selector | Signature | Notes |
|---|---|---|
| `0x6a33d28e` | `swap(address,address,uint256,uint256,uint256,address)` | tokenIn, tokenOut, amountIn, minOut, expiry, recipient. No maker arg. |
| `0xb6466384` | `quote(address,address,uint256)` view | Pure function of module+registry storage. |
| `0xa20e9599` | `swap(…,address,address,bytes)` | Signed variant accepting an explicit maker + signature (unused by the default path). |
| `0x3e413bee`/`0x3fc8cef3`/`0x7ce91411`/`0x8da5cb5b` | `usdc()`/`weth()`/`pricing()`/`owner()` pure | Immutable constants (module = `pricing()`). |
| `0x8456cb59`/`0x3f4ba83a`/`0x5c975abb` | `pause()`/`unpause()`/`paused()` | OZ Pausable. Emits `Paused`/`Unpaused`. |
| `0xe5711e8b` | `rescueToken(address,address,uint256)` | owner-only. |

Events: `Swap(address,address,address,uint256,uint256)` topic0 `0xcd3829a3…`; a second event
`0xea517215…`; `Paused(address)` `0x62e78cea…`; `Unpaused(address)` `0x5db9ee0a…`.

### Pricing module
| Selector | Signature | Notes |
|---|---|---|
| `0xd4f41cf2` | `(uint8,bool,uint256)` view | Pricing; returns the fill incl. the **maker** address + amounts. |
| `0xe621a64b` | `getAssetConfig(uint8)` view | `(token, decimals, param, minSize)`. |
| `0x1626ba7e` | `isValidSignature(bytes32,bytes)` | EIP-1271 (used by the off-chain update committer, **not** the quote path). |
| `0x70c67144`/`0xef8f6a97`/`0xba7518cf`/`0xb53d54fe` | config setters | owner-only; **no events**. The maker was set via `0x70c67144`. |

### PrioUpdateRegistry
| Selector | Signature | Notes |
|---|---|---|
| `0x16c83adc` | `getState(uint256 bookId, uint32 t0, uint32 t1)` view | Returns `(uint256 ts, uint256[] lanes)`. |
| `0xa9114b0f` | `updateState(address caller, uint256 bookId, uint32 ts, uint256[] lanes)` | **The live commit path.** Decremented on each swap; also called top-level by the operator. |
| `0xe50de8ea` | `batchUpdateStateWithSignature((address,address,uint256 bookId,uint32 ts,uint256[],bytes)[])` | Signed multi-book commit. **Exists in ABI but never called on-chain** (all 1000 sampled commits are `updateState`). |
| `0x43d24a5e`/`0x04b07a5e`/`0x75ceb837` | `addUpdater`/`removeUpdater`/`isUpdater(address,address)` | Authorized price signers (≠ maker). |
| `0x0260ee36`/`0xb278d9b0` | `MAX_UPDATE_AGE()` / `MAX_UPDATE_LEAD_TIME()` | Both return **0**. |

---

## 4. Storage layout (module `0xBC60…`)

| What | Slot |
|---|---|
| Settlement address | `2` |
| Asset config `mapping(assetId => packed)` | `keccak256(abi.encode(assetId, 3))` → `param | decimals | token` (token in low 20 bytes); `+1` = `minSize` |
| Token → assetId | `keccak256(abi.encode(token, 4))` |
| **Maker (global)** | `0x1471eb6eb2c5e789fc3de43f8ce62938c7d1836ec861730447e2ada8fd81017b` |

Verified: `keccak(0,3)=0x3617319a…`, `keccak(1,3)=0xa15bc60c…`. The maker is **global** (the WETH
and WBTC swaps read the same maker slot), not per-book.

Maker ERC20 `balanceOf` mapping slots (in the token contracts, for reference): WETH=3, WBTC=0,
USDC=9. (Not used by this package — see §6.)

---

## 5. Liquidity / pricing model

- **Hub-and-spoke**, USDC at the center. The only on-chain books are `WETH/USDC` and
  `WBTC/USDC`. Cross pairs (e.g. `WETH→WBTC`) are routed **through USDC atomically** in a
  single `swap()` (the maker bridges with transient USDC) — confirmed by trace.
- **Inventory is one shared maker wallet**, so per-pair reserves do not exist; venue TVL =
  Σ `balanceOf(maker)` × price. Settlement holds nothing.
- A swap reverts (rather than caps) when the maker lacks inventory / bridge USDC. Oversize ⇒
  revert is acceptable.
- **Same-block / exact-timestamp gate (the key constraint):** `MAX_UPDATE_AGE()=0` and
  `MAX_UPDATE_LEAD_TIME()=0`. `quote()`/`getState` revert with `StaleUpdate()` (`0x666a2814`)
  unless `block.timestamp == the committed update timestamp`. There is **zero** staleness
  window: the operator commits prices and the swap must land in the same block (via a builder
  bundle). On-chain registry state is written **only** when the maker commits (sporadically,
  not every block), so between commits the indexed state is stale by design.

**Consequence for VM simulation:** pricing is fully reproducible from indexed storage +
bytecode (no external calls, no ecrecover on the read path, no DCI), **but** the simulation
must run with `block.timestamp`/`block.number` pinned to each book's committed snapshot — fed
by an external **quote-override stream** (out of scope for indexing; see §8). Per-book
timestamps are independent, so each book is pinned separately.

---

## 6. How this package indexes it

Type: **VM**, `protocol_system = vm:bopamm`, `protocol_type = bopamm_book` (Swap), one
component **per book** (`asset/USDC`).

Component id = `0x` + `settlement (20 bytes) ‖ assetId (12 bytes, BE)` — exactly 32 hex
bytes, because `tycho-simulation` hex-decodes the id into the adapter's `bytes32 poolId`
(`string_to_bytes32`). tokens = `[asset, USDC]` (sorted ascending).
contracts = `[settlement, module, registry]` (same three for every book). All deployment
addresses + storage slots are passed via **substreams `params`** (`DeploymentConfig`) — no
hardcoded addresses; the package can target another deployment by changing params.

### Module graph (`src/modules/`, one file each)
1. **`map_components`** — books are discovered from **storage writes** (the contracts emit no
   config events): a zero→non-zero write to `keccak(assetId,3)` ⇒ a new book; the token is
   decoded from the packed slot value.
2. **`store_components`** — indexes books by `book:{assetId}` and by **every** token
   (`token:0x{addr}`), so a token or book id resolves back to its component.
3. **`store_maker`** — tracks the global maker from writes to the maker slot (`set` policy ⇒
   rotation propagates).
4. **`map_relative_balances`** — maker inventory TVL from ERC20 `Transfer`s touching the
   maker; USDC deltas are emitted under **every** book (shared quote inventory duplicated,
   not split — by design, since the client does not dedupe and under-reporting risks
   min-TVL filtering).
5. **`store_balances`** — `StoreAddBigInt` → absolute balances.
6. **`map_protocol_changes`** — assembles `BlockChanges`: new components + `balance_owner`
   dynamic attr, absolute balances, full storage+code of the three contracts
   (`extract_contract_changes`, fixed predicate, **no DCI**), the per-book
   `override_block_timestamp` decoded from `updateState`/batch calldata, and `Paused`/`Unpaused`
   pause state. `manual_updates` components are explicitly `mark_component_as_updated` on
   each quote refresh and on shared-module changes.

### Notable decisions
- **Transfer-delta balances, not slot-read.** Slot-read (Balancer-V3 `get_vault_reserves`
  pattern) gives absolute balances but needs each token's `balanceOf` slot, which is unknown
  in-substreams for a *new* asset → would break auto new-market TVL. Transfer-delta is
  universal. Trade-off: a maker rotation to a pre-funded wallet under-reports until an RPC
  re-seed (rotation has never happened; mitigatable).
- **`balance_owner` is a dynamic attribute**, emitted whenever the maker slot is written
  (`extract_maker_changes`) — because the maker is configured *after* book creation, and the
  maker can rotate.
- **`override_block_timestamp`** (8-byte BE u64) is read from the `bookId`/`ts` in the
  registry update **calldata** (not by reverse-engineering lane slots). `tycho-simulation`
  pins `block.timestamp` to it when simulating the book (generic mechanism added in PR
  #1034), which passes the registry's exact-timestamp `StaleUpdate()` gate.
- **`batchUpdateStateWithSignature`** is decoded for completeness (ethabi) but is currently
  only synthetic-tested (never called on-chain).

---

## 7. Lifecycle reference (mainnet)

| Block | Event |
|---|---|
| 25,099,176 | Registry deployed (= package `start_block`, earliest of the three) |
| 25,171,611–12 | Module + settlement deployed |
| 25,171,616 | tx `0x58071cb7…` writes both asset configs → **books 0 & 1 created** |
| 25,171,618 | tx `0x62f8e396…` (`0x70c67144`) sets the maker → `balance_owner` emitted |
| ~25.19M–25.27M | Active trading (369 `Swap`s, ~1000 `updateState` txs) |

---

## 8. Remaining work (next steps)

1. ~~VM swap adapter~~ — **done**: `BopAMMAdapter` under
   `protocols/adapter-integration/evm/src/bopamm/` (sell-side; quote-driven limits via
   bisection; see its manifest). The §5 timestamp gate is handled by the
   `override_block_timestamp` attribute consumed by `tycho-simulation` (PR #1034); no
   separate quote-override stream is needed. When simulating `swap()` (not just `quote()`),
   the maker's token balance must still be present in the simulated token contracts — it is
   *not* in the tracked contract set; pricing doesn't need it but settlement transfers do.
2. **`extractors.yaml` `vm:bopamm` entry** — the entry itself is 8 trivial fields (see PR
   description / repo `extractors.yaml`); the real prerequisite is **releasing the spkg to
   S3** via `protocols/substreams/release.sh ethereum-bopamm` (spkgs are gitignored, fetched
   from S3 at runtime). Params are baked into the spkg by `substreams pack`.
3. **Robustness gaps** (documented, not blocking the MVP):
   - Asset **delisting** (`non-zero→0` config) is not handled → a removed book stays
     "active". Components are immutable; would need a deactivation signal.
   - **Maker rotation** to a pre-funded wallet under-reports TVL until the new maker's first
     fill (or an RPC re-seed).

---

## 9. Verification done

- 8 unit tests (incl. the real `updateState` calldata decode, the `bookId==assetId`
  cross-lock, `asset_config_slot` keccak vs on-chain, batch decode, token-index regression,
  params parse). `clippy -D warnings` + nightly `fmt` clean; host + wasm32 build; `substreams
  pack` succeeds.
- Two adversarial review agents + a live `substreams run` against
  `mainnet.eth.streamingfast.io` over the creation range (25171610–21) and an active range
  (~25.26M): both books created with correct ids/tokens/attrs/creation_tx; the quote
  timestamp attribute on `updateState`; maker balances under both books; `balance_owner` emitted
  at 25171618. Two bugs were found and fixed during review (balance_owner timing; token-index
  sort-order dropping WETH balances).

---

## 10. Reproduction notes

- Bebop API (cross-check only, **not** part of the integration):
  `https://api.bebop.xyz/bopamm/ethereum/v1/{tokens,state,quote}` with header `X-API-Key`.
- Run the package: `SUBSTREAMS_API_TOKEN` (repo `.env`) +
  `substreams run -e mainnet.eth.streamingfast.io:443 ethereum-bopamm-v0.1.0.spkg map_protocol_changes -s 25171610 -t 25171621`.
