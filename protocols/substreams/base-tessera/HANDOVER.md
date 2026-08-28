# Tessera V (Base) — Integration Research Handover (Phase 1)

Everything learned about Tessera's on-chain design on Base, verified against mainnet state via
archive `eth_call` / `eth_createAccessList` / storage probing (RPC: `base.gateway.tenderly.co`),
2026-08-28. Extends the phase-0 assessment (`docs/research/tessera-base-assessment.md`) and the plan
(`.claude/plans/tessera-base-integration.md`, Confluence page 3930849284). **Three plan assumptions
are revised by this document — see §9.**

> Tessera V is Wintermute's propAMM: no pools, no curves. A per-book **price store** receives an
> operator price post **every block**; a single **treasury** holds all inventory; the verified
> `TesseraSwap` entrypoint settles against it by allowance. Integration type: VM (`vm:tessera`).

---

## 1. Contract topology (revised — per-book stores)

The assessment's "fixed 6-contract set" was wrong: it access-listed only the WETH book. Each book
has its **own store contract** (an EIP-1967 proxy), all pointing at one shared implementation.

| Address | Role | Verified | Code |
|---|---|---|---|
| `0x55555522005BcAE1c2424D474BfD5ed477749E3e` | `TesseraSwap` — swap/quote entrypoint, holds nothing | **yes** | 4.2 KB |
| `0x31e99e05fee3dce580af777c3fd63ee1b3b40c17` | engine — pricing + book registry (TesseraSwap `slot0`) | no | 15.6 KB, **not** a proxy |
| `0x3dbe077e7986657e95e1cc50089f17a5a4af0aae` | treasury — all inventory (TesseraSwap `slot1`) | no | 4.0 KB |
| `0xdbd31ea3de20a2b36a5bd36c7167699f2450b5c6` | owner (TesseraSwap `slot2`) — a **Gnosis Safe**, admin surface | — | 4.0 KB |
| `0x6d9dd143e42b6338f4f6a7c0c26d124658f641cb` | store implementation, **generation 3** (deployed 49,879,701) | no | 18.7 KB |
| `0xfdb7fa3f95e47624b7423b48462564107aa4e684` | per-book pricing lib (WETH, cbBTC books), code-only | no | 1.2 KB |
| `0x9f924c0765815851d9f4982c7030e2189a7828a9` | per-book pricing lib (EURC, VIRTUAL, AERO, VVV, deSPXA), code-only | no | 1.0 KB |
| `0x7034c5c74f66d3337777772c2964db31765db23e` | write-path-only contract (1 slot per swap) — **identity open** | no | 4.4 KB |
| `0x505352DA2918C6a06f12F3d59FFb79905d43439f` | pair-list helper `getTesseraPairs() → address[][]` (view-only convenience) | no | 3.2 KB |
| `0x3b9e5466713910489db4b30e5b7c26c4545bf62f` | impl deployer EOA (top-level deploy of gen-3 impl) | — | EOA |

### Per-book stores (live books, 2026-08-28)

All are 1,971-byte EIP-1967 proxies → impl `0x6d9d…41cb`. Proxy bytecode is **not** byte-identical
across books (embedded immutables) — do not fingerprint by code hash; fingerprint by the init-time
EIP-1967 implementation-slot write (§5).

| Book | Store | slot 50 (price signer EOA) |
|---|---|---|
| WETH/USDC | `0xf524c1bc1c64a2c99bc7eccf19ede9a1d89d5a7c` | `0x9dd48a40bb70b21aa94428a712e4ad65bff26bfb` |
| cbBTC/USDC | `0xed57bacdc2a990b631f8817853935791c122c356` | `0x3ebe6b6dfaeaa2b205e79e749390b2e9ea76c978` |
| EURC/USDC | `0x4b963fb4a26f082d94f964fa3c2764821cc06bd4` | `0x5aa7c10470ed455132c0e93e67e712b05bcd3a21` |
| VIRTUAL/USDC | `0xe1191102bdcea1928a93b4d6ea7bf5c4e9207210` | `0x56112ecc5c3a3ad26df2c1e5fc995e2e47aa7b20` |
| AERO/USDC | `0x3b84be4d48888a6bc385eea93e522246b214069e` | `0xf75c8d7864d4d9c039de17cbb74c810094c79e56` |
| VVV/USDC | `0x402e0d314fd6f55348df7cc478bab811826e3e91` | `0xd52f194ff7b52aae71b1485228a50460da1eaefc` |
| deSPXA/USDC | `0x4955d3c5c755f654cd27ada9f085ded00469fbc8` | `0xaa2f721ee57f5f5bafbad95c484b4b131c52cc1f` |
| NVDAc/USDC | `0xede940cdf2a9c5620cbf97e45947594723e29c14` | `0xee6cb1eeb0a141a54b5c5faa94a50859c288e120` |
| USDT/USDC (disabled) | `0x532c782afd8f6a1deed25d274235674a624a002f` | — |

Each book has its **own price-post signer EOA** (store slot 50) — liveness monitoring must be
per book, not per venue.

### Swap access set (what a quote/swap actually touches)

`eth_createAccessList` on `tesseraSwapViewAmounts` / `swapAmount`:
`TesseraSwap (slots 0–2)` + `engine (slots 0, 5, 10, + one keccak slot per book)` + `the book's
store (slots 0–27, 48–51, EIP-1967 impl slot)` + code-only: impl + the book's pricing lib.
No token contracts, no external oracles, no other books' stores. **No DCI needed** — but the
tracked-contract set is *dynamic* (per-book stores + impl generations), see §5/§9.

## 2. Interfaces

### TesseraSwap (verified — see Basescan)

| Selector | Signature | Notes |
|---|---|---|
| `0x77f65f98` | `tesseraSwapViewAmounts(address,address,int256) → (uint256 amountIn, uint256 amountOut)` | `amountSpecified > 0` exact-in, `< 0` exact-out. Sender-independent. Byte-identical to the executable path at a pinned block. |
| — | `tesseraSwapWithAllowances(address,address,int256,uint256 amountCheck,address recipient,bytes swapData)` | The aggregator path. `swapData` may be empty. Exact-in: requires `amountOut >= amountCheck`. |
| — | `tesseraSwapWithCallback(...,bytes callbackData,bytes swapData)` | Flash-style; not needed for Tycho. |
| — | `changeTesseraEngine(address)` / `changeTesseraTreasury(address)` | owner-only, **no event, no timelock**. |
| event | `TesseraTrade(address tokenIn, address tokenOut, uint256 amountIn, uint256 amountOut, address recipient)` | topic0 `0x97ba0cd8…ad6a`, all fields unindexed. |

### Engine (unverified)

| Selector | Meaning | Evidence |
|---|---|---|
| `swapAmount(address,address,int256,address,bytes)` | pricing + settlement inner call; reverts `T37` unless caller is TesseraSwap | verified source of TesseraSwap |
| `swapAmountView(address,address,int256,address)` | open view | quotes served |
| `0xb8744eb4` | **create/configure book** (admin) — first arg is the base token | inner calldata of the deSPXA creation Safe tx `0x1938ddaf…9958` @ 44,091,817 |
| revert `T33` | unknown/unsupported token pair | BSC probe |

Engine storage: `slot0` = `0x…01 ‖ TesseraSwap` (packed flag+address); `slot5` = `0x77359400`
(2e9 — role unknown, open); `slot10` = recent block number (updated with posts; **not** a gate —
overriding it does not change quotes); one keccak-derived slot per book = token → store mapping
(hash preimage not recovered — Solidity/Vyper standard layouts ruled out for bases 0–127; **not
needed**, discovery keys off store creation, §5).

### Store (per book; storage layout of gen-3 impl `0x6d9d…`)

| Slot | Content | Evidence |
|---|---|---|
| 0 | packed price post: byte0 = counter, bytes1–5 = **post block number**, then price mantissa + counters. **Changes every block.** | diffed across consecutive blocks; drives the freshness gate (§4) |
| 1 | owner ‖ flag bytes (`…dbd31ea…0101`) | read |
| 2 | engine address | read |
| 3, 4 | cumulative quote-side / base-side volume counters (slot 3 reappears in the trade event's last data word) | correlated |
| 5–7 | packed config (bps-scale constants: 10000, 1000, 500, 102400, …) — spread/fee params, exact semantics open (VM makes them moot) | read |
| 8–16 | quote ladder: (level, size) pairs; ladder sum ≈ max quotable clip | bisected max ≈ ladder sum |
| 48 | **base token address** | matches all 8 stores incl. disabled USDT |
| 49 | `0x00 06 <base_decimals> ‖ quote token (USDC)` — e.g. `0x000612…` for 18-dp bases | matches all stores |
| 50 | per-book **price signer EOA** | all 7 differ, 0 code |
| 51 | per-book pricing lib address (code-only contract) | matches access lists; **written after creation**, zero at init |
| EIP-1967 impl slot | store implementation | init-written in the creation tx |

Store events: `0x56441808e0dc…326e` = per-fill event — topic1 taker, topic2/3 signed base/quote
deltas, data = [token0, token1, fill counter, cumulative, price ×1e6]. `0x61e15d1624…f23b` = rare
counter event (topic1 = small int, no data) — heartbeat/epoch, not needed. **Price posts emit no
event** — indexing must be storage-driven (as planned).

## 3. Pricing determinism

Pure function of: engine storage + the book store's storage + `block.number` (freshness, §4).
No signature checks, no ecrecover/EIP-1271 on the pricing path, no token reads, no external oracle,
no `block.timestamp`. Cross pairs (e.g. WETH→cbBTC) quote and settle **directly**, bridged through
USDC internally — the hub-and-spoke BopAMM pattern; component model stays one book per base/USDC.

## 4. Freshness gate (measured — revises the assessment)

The quote **decays with the age of the last price post** (store slot0 bytes1–5 vs `block.number`),
then dies:

| post age (blocks) | quote |
|---|---|
| 0–4 | full price |
| 5–7 | ramping down (≈ −10 to −40 bps) |
| 8–19 | ≈ **−100 bps** plateau |
| ≥ ~20 | **0** (dead) |

Measured by state-overriding the embedded post block at a pinned head (two runs, consistent).
The assessment's "no staleness gate" was an artifact of historical probing: state and block env move
together, so a *relative* gate is invisible. Consequences:

- Simulation from indexed state at block N with env N is always fresh (posts land every block) — exact.
- The simulation env's block number **must** equal the indexed block (default `EVMPoolState`
  behavior) — simulating old state under a newer block env understates or zeroes quotes.
- Operator halt ⇒ books quote 0 within ~40 s. Self-limiting; alert if any store's slot0 stops
  changing for N blocks (per book, via its signer EOA).

## 5. Discovery, lifecycle, mutations

**Book creation** (evidence: deSPXA @ 44,091,817, tx `0x1938ddaf…9958`): the owner Safe
`execTransaction` → engine `0xb8744eb4(baseToken, …)`; the engine **internally CREATEs** the store
proxy (no top-level `contractAddress`), and in the same tx: store init writes slots **48**
(base token), **49** (decimals ‖ USDC), **50** (signer), and the **EIP-1967 impl slot**; the engine
writes its token→store mapping slot (value = store address). Slot 51 (pricing lib) is written later.

**Substreams discovery predicate** (layout-independent): a contract created in-block whose init
writes include the EIP-1967 implementation slot **and** slots 48/49 (two addresses, second = USDC)
⇒ new book. Cross-check the engine also wrote a slot whose value = that address in the same tx.
Component id proposal: `0x` + `TesseraSwap (20B) ‖ base token last 12B` — unique per book, stable
across store re-deploys; `contracts = [TesseraSwap, engine, store_i, impl_gen, lib_i, 0x7034…]`.

**Book removal** = disable: quote returns `(amountIn, 0)`; the store keeps its code, slots 48/49,
and even gets impl upgrades. Delisted books self-disable in `getLimits` (0). No component-removal
handling needed.

**Impl upgrades are routine.** Three generations observed: `0xf3be571a…` (at first store creation
37,518,780) → `0x10182fda…` (deployed 43,832,837) → `0x6d9d…41cb` (deployed 49,879,701 by EOA
`0x3b9e5466…`, top-level; fleet-wide upgrade including disabled stores). USDT + ZORA were
**upgrade-test books**: created in the gen-3 deploy window, 9 trades each over ~87 min
(49,881,382–49,883,986), then disabled. Substreams must track EIP-1967 impl-slot writes on every
store; a new impl address is a new tracked contract whose deployment must be witnessed (see §9 risk).

**Treasury rotated once**: `0xc2ca2485618af14135e79487492c3a4f2a062ed5` → `0x3dbe077e…` at block
**37,737,344** (~2.5 days post-launch). `balance_owner` must be a dynamic attribute keyed off
TesseraSwap `slot1` writes. Engine (slot0) and owner (slot2) unchanged since deploy.

## 6. Balance / inventory model

Single treasury backs all books (≈ $1.27M across 8 tokens); unlimited allowance to TesseraSwap.
No per-book reserves. BopAMM model applies unchanged: `balanceOf` snapshot seeding on discovery and
treasury rotation, ERC20 `Transfer` + WETH `Deposit`/`Withdrawal` deltas, USDC duplicated under
every book, `self_contained_tokens` static attribute.

## 7. Sides, limits, connectivity

- Exact-in (`amountSpecified > 0`) and exact-out (`< 0`) both work and round-trip exactly.
- Oversize / disabled book → `(amountIn, 0)`, never reverts. Unknown token → revert `T33`.
- Max clip ≈ quote-ladder sum (store slots 8–16); WETH ≈ 139 WETH (~$348k) at probe time.
  Configured in storage, not read from balances — `getLimits` bisection is correct.
- All books quote vs USDC; cross pairs bridge internally. Component per book vs USDC.

## 8. Lifecycle reference

| Block | Event |
|---|---|
| 37,518,648 | TesseraSwap + engine deployed (= earliest family deploy, package `initialBlock`) |
| 37,518,780 | WETH store created (impl gen-1 `0xf3be571a…`) |
| 37,737,344 | treasury rotation `0xc2ca2485…` → `0x3dbe077e…` |
| 43,832,837 | impl gen-2 `0x10182fda…` deployed |
| 44,091,817 | deSPXA book created (Safe tx `0x1938ddaf…9958`) |
| 49,879,701 | impl gen-3 `0x6d9d…41cb` deployed (EOA `0x3b9e5466…`) |
| 49,881,382–49,883,986 | USDT + ZORA test books live (9 trades each), then disabled |
| 50,526,653 | NVDAc/USDC book created (tokenized NVIDIA, 8 dp; Safe tx `0x3447f2ec…a5cb` proposed by `0x3b9e5466…` — the same EOA that deployed the gen-3 impl) — the token set is actively growing |

BSC: same TesseraSwap/engine/treasury addresses, different owner (`0xae3c0084…`); WBNB/USDT quotes
revert `T33` — **venue not configured/live on BSC** (2026-08-28). Re-check before planning BSC.

## 9. Plan revisions (vs `.claude/plans/tessera-base-integration.md`)

1. **D2/D3 revised — the tracked-contract set is dynamic, not a fixed 6-address params list.**
   Per-book stores are created by the engine at book creation; impls rotate (3 generations in 10
   months). Still **no DCI**: every creation is witnessable in-block. The substreams needs a
   Curve-style dynamic predicate (store module accumulating tracked addresses) instead of a fixed
   predicate. Params carry the *stable* addresses (TesseraSwap, engine, USDC, deployer EOA) + the
   discovery slot constants (48/49/50/51 + EIP-1967 slot).
2. **Freshness gate exists** (relative, §4). No `override_block_timestamp` machinery needed, but
   the adapter/harness must simulate with block env = indexed block (default behavior — assert it
   in tests). Add per-book post-liveness monitoring (store slot0 vs head).
3. **Impl-generation bootstrap risk**: a gen-4 impl will be deployed *before* any store references
   it; a fixed predicate would miss its creation code. Mitigations, in order: (a) track top-level
   creations from the impl-deployer EOA `0x3b9e5466…` as candidate contracts; (b) runbook — on
   impl-slot change alert, add the address to params and re-sync. Decide in Phase 2.

## 9.5 Adversarial-review findings (2026-08-28, addressed in-package)

An independent review of the Phase 2–4 diff found and the package now fixes:

1. **Components must not reference not-yet-deployed contracts** (sync-breaking): the storage
   layer resolves every `contracts` entry against known accounts and fails the flush on a miss.
   Components now carry only `[TesseraSwap, engine, own store]`; the code-only satellites are
   delivered as plain account changes via the tracked predicate (production syncs from
   initialBlock, so their creations are witnessed) and via `initialized_accounts` in the
   integration-test yaml.
2. **Seed-skip granularity** (balance drift): snapshot suppression of event deltas is per
   `(token, component)` — a new book's USDC seed no longer swallows same-block USDC deltas on
   the other books.
3. **Rotation-block accounting**: event deltas in a rotation block are matched against the
   *old* custodian and the re-seed is `balanceOf(new) − balanceOf(old)` at end-of-block —
   exact even when inventory migrates within the rotation block. (Verified live: the 37,737,344
   rotation had zero in-block flows — the old treasury was drained in earlier blocks.)
4. **Store re-deploy resilience**: `all_books` dedupes by component id so a store re-deploy for
   an existing base token cannot double the USDC fan-out (which would panic the balance store on
   duplicate ordinals).
5. **Pricing-lib alert**: writes to the store's lib slot (51) now emit a `book_lib` attribute
   (like `engine` / `store_impl`) so a new lib generation missing from `tracked` params alerts
   instead of silently breaking that book.
6. Balance deltas sort by `(tx index, ordinal)` so one transaction's deltas stay contiguous for
   the downstream aggregation; `store_treasury` uses the padded word decoder.

## 10. Open questions (not blocking Phase 2)

- **Pre-rotation balance drift (~+2.3–2.5%)**: the rotation-range live run shows the accumulated
  balances entering block 37,737,344 exceeded the old treasury's true (zero) balance by
  +3,444,538 USDC-wei (~$3.44) and +0.00093 WETH — i.e. a small fraction of the old epoch's
  *outflows* was missed by the Transfer-delta tracking, proportional to flow. The rotation
  re-seed is delta-based, so the drift persists additively. Mechanism unidentified (all known
  transfer paths emit logs); reconcile the accumulated balances against `balanceOf(treasury)` at
  head after a full-range sync, and use the NVDAc-range harness balance check (in-range
  self-consistent) to separate seed semantics from event-tracking misses.

- Identity of `0x7034c5c7…` (one slot written per swap — nonce/accounting?). Tracked regardless.
- Engine slot5 (`2e9`) and store slots 5–7 config semantics (VM executes them; labels only).
- Engine token→store mapping hash preimage (nice for cross-checks; discovery does not need it).
- Exact decay-curve shape between blocks 5–8 and the precise dead cutoff (19 vs 20).
- A store re-deploy for an existing base token would re-emit the component creation (same id,
  new `price_store` attribute) — behavior of the extractor on a duplicate creation is untested;
  the balance path is now safe (dedupe), monitoring would see the `store_impl`/`book_lib`
  attributes move.
- Whether the two per-book pricing libs (`0xfdb7…`, `0x9f924c…`) are generations or book-class
  variants (majors vs tail) — watch which lib new books get.
