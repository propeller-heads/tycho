# ethereum-sky

Indexes the Sky (ex-MakerDAO) stablecoin mint/redeem venues on Ethereum mainnet as
protocol system `sky`, with three hardcoded components (`ImplementationType::Custom`,
no factory):

| component_type | Contract | Pair | Id |
|---|---|---|---|
| `psm` | DssLitePsm (`MCD_LITE_PSM_USDC_A`) | DAI↔USDC | `0xf6e72db5454dd049d0788e411b06cfaf16853042` |
| `psm_wrapper` | UsdsPsmWrapper | USDS↔USDC | `0xa188eec8f81263234da3622a406892f3d630f98c` |
| `converter` | DaiUsds | DAI↔USDS | `0x3225737a9bbb6473cb4a45b7244aca2befdb276a` |

All swaps are 1:1 (with 18↔6 decimal rescaling for USDC legs) subject to the PSM fees.
The MKR→SKY converter is deliberately out of scope: its fee ratchets +1pp per quarter
(4% as of 2026-06) and the path is being wound down by governance.

## Scope: Sky vs. Spark, and what lives in which Tycho protocol

Sky (the ex-MakerDAO core protocol, rebranded 2024-09) and Spark (its largest "Star"
subDAO) share one ecosystem, and their venues are split across Tycho protocols by
*mechanism*, not by brand:

- **`sky` (this package)** — Sky core's non-ERC-4626 mint/redeem venues on Ethereum:
  the LitePSM, its USDS wrapper, and the DaiUsds converter. Fixed 1:1 rates with fees,
  no shares, no exchange rate — they need custom PSM math.
- **`erc4626` (`ethereum-erc4626`)** — the savings vaults from both orgs, all standard
  share-appreciating ERC-4626:
  - sUSDS `0xa3931d71877c0e7a3148cb7eb4463524fec27fbd` — Sky core contract (`SUSDS` in
    the chainlog, yield = Sky Savings Rate) but *marketed* as "Spark Savings". Live.
  - sDAI `0x83f20f44975d03b1b09e64809b757c47f942beea` — Sky core (DSR). Configured but
    excluded client-side in tycho-simulation's `erc4626_filter` (reason unrecorded).
  - spUSDC/spUSDT/spETH — Spark's own vaults backed by its Liquidity Layer. Configured;
    intended to be filtered client-side.
  - stUSDS `0x99cd4ec3f88a45940936f469e4bb72a2a701eeb9` — Sky core (Lockstake engine).
    NOT yet added: it has a deposit cap and utilization-gated withdrawals, so the
    generic simulation's `max_redeem = totalSupply()` shortcut must first be replaced
    with a real `maxRedeem` read.
- **Future Spark L2 integrations** (e.g. DXI-11, Arbitrum) — Spark runs PSM3 contracts
  (USDS/sUSDS/USDC no-slippage) on L2s via its Liquidity Layer; on Ethereum that role
  is played by Sky's LitePSM indexed here. A PSM3 package would be structurally close
  to this one.

## Attributes

- `tin` / `tout` (dynamic, BE uint): the LitePSM sell/buy fees, updated from
  `File(bytes32,uint256)` events (decoded via the abigen bindings generated from
  `abi/dss_lite_psm.json`). `type(uint256).max` (`HALTED`) disables the respective
  direction. Both have been zero since deployment.
  - The PSM's seeds are literal zeros: its creation anchor is the deployment
    transaction, where the fee storage is zero-initialized by definition.
  - The wrapper proxies the PSM and is created later, so its fee seeds are
    deterministic eth_call snapshots of the PSM's post-block `tin()`/`tout()` at the
    wrapper's creation block.
- `dai_escrow` / `usds_escrow` (dynamic, BE uint, wrapper only): the joins' internal
  dai escrows (`vat.dai[join]`, wad), which the wrapper's in-flight DAI↔USDS
  conversion burns through — `sellGem`'s DAI payout via DaiJoin, `buyGem`'s full USDS
  input via UsdsJoin — so each bounds its direction alongside the mirrored PSM
  inventory. Seeded at the wrapper's creation via `vat.dai` eth_calls and updated from
  the same Vat storage writes the converter's balances are read from.
- The converter is immutable, feeless and has no attributes beyond `component_type`
  and `gem`.
- `gem` (static, address): the token moved by `sellGem`/`buyGem` (USDC) or burned by
  `usdsToDai` (USDS). Both the simulation decoder and the swap encoder derive token
  roles and call direction from it, so component token order never matters.

## Balances

- `psm`: DAI held by the PSM itself (sell-side buffer, refilled by keepers via
  `fill`/`trim` up to `buf`) and USDC held by its `pocket`
  (`0x37305b1cd40574e4c5ce33f8e8306be057fd7341`). Seeded like the other components
  (zero: the PSM deploys empty) and tracked as transfer-deltas from the following
  block.
- `psm_wrapper`: the wrapper is stateless; its liquidity IS the PSM's. Its balances
  mirror the PSM's inventory (deliberately double-counting the shared inventory so both
  entry points survive min-TVL filtering, per the shared-inventory guidance), with the
  DAI side relabelled as USDS since the wrapper converts in-flight via the joins. The
  mirror is seeded at its creation and follows the PSM's absolute balances
  afterwards. The in-flight conversion is additionally bounded by the join escrows,
  tracked as the `dai_escrow`/`usds_escrow` attributes (see Attributes); in every
  state the protocol has ever been in post-launch the mirrored inventories bind
  first, but pre-launch the zero USDS escrow is the true (zero) USDS -> USDC limit.
- `converter`: mints/burns via the joins and holds no reserves. As its capacity/TVL
  proxy it reports the joins' internal dai escrows (`vat.dai[join]`, converted to
  wad) — the exact amount convertible per side. Escrow equals join-backed token
  supply, so it is immune both to raw burns (which strand escrow) and to hypothetical
  non-join USDS mints (which a supply proxy would wrongly count as convertible).
  Unlike the PSM's transfer-delta tracking, the escrows are read as absolute values
  straight from Vat storage writes to the joins' `dai` slots (each write carries the
  post-change value, so the delta store is bypassed), which also captures every Vat
  channel that can touch them — `join`/`exit` as well as `move`/`frob`/`suck`/`fold`
  donations. USDS's escrow was zero until the Sky launch (block 20770191), so the
  USDS -> DAI limit is correctly zero for that early range.

All state follows one seeding rule: at a component's creation block, balances (and
the wrapper's fees and escrows) are seeded with deterministic `eth_call` snapshots
(`balanceOf`/`vat.dai`/`tin`/`tout`, post-block state), and event-driven updates only
count strictly after that block — in-block activity is already inside the snapshots.
The same boundary gates the wrapper's mirroring of PSM state. Params carry only
contract addresses and creation anchors (see `substreams.yaml`); no values are
configured.


## Module graph

One Rust file per substreams module, numbered by graph order:

1. `map_protocol_components` — emits the three components, anchored to their creation
   transactions (components sharing a transaction are grouped under one entry).
2. `store_components` — records which components this run has created, so downstream
   modules never emit changes for components outside the indexed window (production
   syncs from the initial block are unaffected; scoped windows, e.g. the integration
   test harness, rely on it).
3. `map_relative_balances` — seeds the PSM's and wrapper's balances at their creation
   blocks and routes ERC20 Transfers touching the PSM/pocket to inventory deltas.
4. `store_component_balances` — aggregates relative deltas into absolute balances.
5. `map_protocol_changes` — assembles `BlockChanges`: components, fee seeds and
   `File`-event updates, absolute balances (the converter's read directly from Vat
   storage changes, also emitted as the wrapper's escrow attributes), and the
   wrapper's mirrored PSM state.

## Known limitations

- The converter and the wrapper track every Vat write to the joins' escrow slots
  network-wide, so both receive updates (and re-simulation churn) on virtually every
  block; the converter's reported TVL is the combined escrows (multi-billion) —
  deliberate, but skews TVL-ranked views upward (the wrapper's escrows are
  attributes and do not affect its TVL).
- Governance/catastrophe-grade halt conditions are not indexed: the pocket revoking
  its USDC approval to the PSM, `vat.cage()` (global settlement), USDC blacklisting of
  psm/pocket/wrapper, or an upgrade of the USDS proxy changing event semantics. The
  tracked `tin`/`tout` HALTED sentinel covers the governance halt path that has an
  on-chain event.
