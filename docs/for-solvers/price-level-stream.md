---
description: Streaming Titan's pAMM price levels as Tycho protocol components.
---

# pAMM Price Level Stream

Tycho Simulation consumes <a href="https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level" target="_blank" rel="noopener noreferrer">Titan Builder's pAMM price level stream</a>: a WebSocket that pushes, once per second, the quote books Titan simulated for the pAMMs it builds blocks with. `PriceLevelStreamBuilder` turns each frame into the same `Update` messages the protocol stream emits, so you consume it like any other stream. It covers Ethereum Mainnet only.

```rust
use tycho_simulation::price_level_stream::stream::PriceLevelStreamBuilder;

let price_level_stream = PriceLevelStreamBuilder::new()
    .with_known_pamms()       // serve the venues Tycho has measured
    .auto_detect(true)        // also serve any other venue Titan streams
    .with_tokens(all_tokens.clone())
    .build();
```

`with_tokens` takes the same token metadata the protocol stream uses; load it with `load_all_tokens`, as the [Simulation](simulation.md) page describes. The stream connects on its first poll, re-establishes the connection after a disconnect or a 10 s gap between frames, and never terminates on its own, so run it in its own task alongside your protocol stream. `endpoint()` points it at a closer Titan region than the EU default; without that call it honors the `TITAN_PAMM_PRICE_LEVEL_URL` environment variable.

The builder opens a second Titan socket as well, carrying the account state Titan simulated each venue on to price its ladders. It attaches that snapshot to the states whose books name it, which is what lets you simulate a pAMM swap on the state behind its quote instead of on mined state; [Simulation overrides](#simulation-overrides) covers the contract, and `without_simulation_overrides()` turns the whole path off.

## What the stream serves

Every frame carries a flat list of books, one per venue and pair, and the stream emits one component per venue and pair. A component id concatenates the venue address with the pair's two token addresses, sorted ascending, so it survives a venue rename. The `pamm_address` static attribute carries the venue address your encoder sends the swap to.

`with_known_pamms()` registers the six venues Tycho has measured — `bebop`, `fermiswap`, `kipseli`, `metric`, `taurusfi` and `tempest` — each with its own per-swap gas estimate, and names their components after the venue. Titan quotes neither every venue nor every pair at all times, so the live set is usually smaller.

`auto_detect(true)` additionally serves every other venue Titan streams, naming it by its full lowercase hex address (`pricelevelstream:0x5979…`) and quoting it with a conservative gas estimate — the maximum over the measured venues, which `auto_detected_gas_cost` overrides. A venue keeps its component ids and its `pamm_address` when a later release measures it and the name replaces the address.

`add_pamm` registers one venue explicitly, overriding both the defaults and auto-detection; `deny_pamm` drops a venue and blocks auto-detecting it. Between those two the later call wins, and neither loses to `with_known_pamms()`, in any call order.

## Component families and execution

The stream emits components under one of two protocol system families, and that choice decides how tycho-execution executes their swaps:

* **`fallback:{pamm}`** (the default) — tycho-execution routes the swap through `TychoFallbackRouter`, which retries a reverted pAMM swap on the fallback pool your solution names in the swap's `user_data`. A pAMM quote targets the block Titan is building, so a venue reverts a fill against an older ladder; the fallback pool absorbs that case instead of losing the route.
* **`pricelevelstream:{pamm}`** — `without_fallback_router()` keeps every venue on this direct path. The swap then executes on the venue itself, and a stale maker quote reverts the whole route.

`{pamm}` is the venue name for a registered venue and the venue's lowercase hex address for an auto-detected one.

Distinct component ids do not imply distinct liquidity. A venue the stream serves may also reach you through another integration path — an on-chain integration under `vm:{pamm}`, or an RFQ client for the same market maker (see [Request for Quote Protocols](request-for-quote-protocols.md)) — and both paths then price the same inventory. Deduplicate by venue through `pamm_address` wherever double-counting matters, such as routing over the combined liquidity.

## Freshness contract

Titan's frames are best effort, not complete snapshots: a venue or a pair can drop out of one frame and return in the next — in a 15 minute capture, 7.7% of frames omitted something the surrounding frames carried — so the stream never reads absence as retirement. Instead it serves a component only while an accepted frame carried it within the last `stale_after`, 24 s (two slots) by default.

### Which frames the stream accepts

The tracker accepts a frame only if its `ts`

* is younger than `stale_after`,
* lies no more than one slot ahead of the local wall clock, and
* is no older than the newest accepted frame's — an equal timestamp passes, because Titan re-emits a frame within one build round,

and only if its `block` neither falls below the newest accepted block nor exceeds it by more than one block per elapsed slot plus two. A frame that fails any of these changes nothing and increments `price_level_stream_frames_rejected_total` under the matching reason. The beacon `slot` carries no freshness decision, so the tracker judges a frame without one exactly like a frame carrying one.

### How components turn stale

A component that no accepted frame has carried for `stale_after` turns stale. The stream then lists it in `removed_pairs`, together with every other component that turned stale at the same instant, and the next accepted frame carrying it re-adds it in `new_pairs`. Removal and re-add always arrive as separate updates. Silence, a disconnect, keepalive-only or unparsable traffic, and replayed frames all end in this same removal.

Recovery runs per component: a frame carrying one pair re-adds that pair and nothing else, and a frame carrying one direction of a pair re-adds the pair with the other direction unquotable. Treat a removal like any other — stop routing through the component until it reappears in `new_pairs`. A stale removal and a retired venue look identical from the outside, by design.

### The one-slot quote guard

Every state the stream emits refuses `spot_price`, `get_amount_out` and `get_limits` once its frame's `ts` is one slot (12 s) old, so you cannot quote a ladder past the block it targeted even before the removal arrives. A venue rejects a fill against an older ladder as stale, which makes such a quote unexecutable in any case. The deadline lives on this process's monotonic clock and never crosses serialization, so a replayed recording always quotes.

`without_quote_guard()` emits states that never refuse. Opt out only for a consumer that quotes a state more than one slot after it arrived by design — a batch simulator, a validation harness — and never on a live router: `stale_after` then becomes the only bound on how old a quoted ladder can be, and the venue may no longer fill such a quote.

### Block numbers and partial updates

Quotes target the block currently being built, so the stream marks every update partial: an update supersedes the previous one for the pairs it contains and says nothing about the pairs it omits. Each update carries the block its frame targets, a removal the newest accepted block. That number never decreases while the stream serves something. Once every component has turned stale, the stream judges the next accepted frame as a first frame, which may carry a lower block — that is how it recovers from a frame with an implausible block, and why you must not order updates by block number across such a gap.

`stale_after(duration)` overrides the window, up to one hour. Set it shorter than the age frames arrive with and the stream rejects every one of them as `too_old`, which the rejection counter and a WARN log both show.

## The V2 message

A frame names its tokens by symbol and prices every level in human units:

```json
{
  "slot": 15387623,
  "block": 26148760,
  "ts": 1791475493806326998,
  "books": [
    {
      "maker": "Fermi",
      "quoteAddress": "0x5979458912f80b96d30d4220af8e2e4925a33320",
      "token1": "WBTC",
      "token2": "USDC",
      "calculated_from": 283851543383726791766048878,
      "bids": [{ "price": 80970.38, "cumVolume": 0.0005 }],
      "asks": [{ "price": 80997.89405475458, "cumVolume": 0.00012346 }]
    }
  ]
}
```

* `block` — the L1 block the quotes target, the one Titan is building.
* `slot` — the beacon slot, when Titan reports one.
* `ts` — when Titan built the frame, in nanoseconds since the Unix epoch. Titan re-emits a frame within one build round under the same `ts`, which makes it a freshness marker rather than an identity.
* `quoteAddress` — the venue key, which is not necessarily the venue's executable address. See [Venue identity](#venue-identity).
* `token1` and `token2` — the pair's base and quote symbols. `token1` is the unit of `cumVolume`, `token2` the unit of `price`.
* `price` — `token2` per `token1`, in human units, as the average execution price of the level's whole cumulative size.
* `cumVolume` — volume cumulated from the top of the book, in whole `token1`. It never decreases down a side, and consecutive levels can repeat a value.
* `calculated_from` — the version of the pair the venue priced both of this book's sides at. It names the snapshot on Titan's second socket that [Simulation overrides](#simulation-overrides) joins the book's ladders to. The parser reads it leniently, so a book naming a reference it cannot read still quotes.
* `maker` — Titan's display label for the venue, which the stream sometimes leaves empty. Nothing reads it; identity rests on `quoteAddress` alone.

Ask prices sit above bid prices, and either side can arrive empty when the venue quotes one direction of the pair only. The parser ignores every other field the stream carries.

## Trusted symbol bindings

A symbol is not an identity. If the stream resolved `WBTC` against the symbols in your token metadata, any token calling itself `WBTC` would capture the bound asset's quotes, so resolution never reads your tokens' symbols at all. A book's symbol prices a pair only when both of these hold:

1. A binding names an address for the symbol — either Tycho's `SYMBOL_BINDINGS` default or one of your own `with_symbol_bindings` entries, which replace a default binding and add symbols the defaults omit.
2. Your `with_tokens` metadata carries that address, and that token supplies the decimals the conversion applies.

Matching is exact, case included, so the stream leaves `wbtc` unbound rather than guessing at it. The defaults bind five assets: WETH, USDC, USDT, WBTC and cbBTC.

**The stream serves bound symbols only.** A book naming an unbound symbol stays unserved, and `price_level_stream_skipped_books_total{reason="unbound_symbol"}` counts it alongside a DEBUG log that names the symbol. Of the 20 books in the 2026-10-08 capture, 19 named bound pairs; the remaining one, TaurusFi's `NVDAx`/USDC, names a symbol the defaults deliberately leave unbound — inferring an address from a symbol is exactly what the bindings prevent, so that book stays unserved until someone resolves `NVDAx` independently. Bind it yourself to serve it:

```rust
let price_level_stream = PriceLevelStreamBuilder::new()
    .with_known_pamms()
    .with_symbol_bindings(HashMap::from([("NVDAx".to_string(), nvdax_address)]))
    .with_tokens(all_tokens.clone())
    .build();
```

A bound symbol whose address your token metadata omits counts under `unadmitted_token` instead, and two symbols that resolve to one asset count under `one_asset_pair`. The stream therefore preserves the quoting model for bound assets, subject to the float reconstruction [Conversion semantics](#conversion-semantics) documents; it does not promise to quote every pair Titan streams.

## Venue identity

A venue's identity in Tycho is its **public identity**: the address a swap goes to. Titan streams some venues under a different `quoteAddress` — a join key across its streams rather than an executable address — so the stream normalizes every such address at every boundary. Kipseli is today's only case: its books arrive under `0xc9a956b5196dfe110debe8781857bc8b97d32091`, while its swaps go to the `KipseliPropAMMWrapper` router at `0x71e790dd841c8a9061487cb3e78c288e75ce0b3d`.

Component ids, venue names, gas costs, denials and the `pamm_address` attribute therefore always carry the public identity, never the streamed one, and your encoder always sends the swap to the public identity. `add_pamm` and `deny_pamm` accept either of a venue's addresses and normalize what you pass, so you can configure a venue by whichever address you have. A venue the alias table does not name already streams under its public identity, so for it the two coincide.

## Conversion semantics

A book prices its pair in human units while the states serve integer ladders, so the conversion applies the resolved tokens' decimals. The stream reads `price` as the average execution price of the level's cumulative size, in `token2` per `token1`, and `cumVolume` as whole `token1`. The bids therefore sell `token1` and the asks sell `token2`:

```text
bids (token1 -> token2):
  amount_in  = round(cumVolume · 10^decimals(token1))
  amount_out = round(price · cumVolume · 10^decimals(token2))

asks (token2 -> token1):
  amount_in  = round(price · cumVolume · 10^decimals(token2))
  amount_out = round(cumVolume · 10^decimals(token1))
```

Each level becomes one absolute quote: `amount_in` for a total `amount_out` that already contains every smaller level's liquidity. A state interpolates linearly between its knots, mirroring how Titan densifies its own simulated levels, and holds no information outside the quoted range: it rejects an input below the smallest level outright, and answers an input above the deepest level with a partial result at that level, as other level-based protocols do. `get_limits` reports the deepest level of the direction you ask about. The two sides convert independently, so a rejected side never costs the other one — within a frame that typed, which [Rejected sides](#rejected-sides) draws the line of.

**Precision.** Titan quantizes both `price` and `cumVolume` to `f64` before sending them, so the ladders carry Titan's own precision and nothing tighter. The conversion rounds every amount to the nearest base unit and keeps amounts beyond `u64::MAX` exact — an 18-decimal ladder passes that bound at about 18.5 tokens — but it promises no atomic-unit parity with whatever the venue computes internally.

### Rejected sides

When a side's levels do not state a usable price function, the stream drops that whole side instead of the offending level: the ladder interpolates between its knots, so dropping one level would turn a broken price function into a plausible invented one. Four cases reject a side, and `price_level_stream_rejected_sides_total` counts each one:

| `reason`            | Cause                                                                                                                                                      |
| ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `unsupported_level` | A level's `price` or `cumVolume` is not a finite positive number, so the level states no price at all.                                                      |
| `non_finite_amount` | A level's amount overflows `f64`, which leaves its size unknown.                                                                                           |
| `zero_amount`       | A level rounds to a zero `amount_in` or `amount_out`. A knot at zero is not a quote, and interpolating across it fabricates depth the venue never offered.  |
| `conflicting_knots` | Two levels round to the same `amount_in` but disagree on `amount_out`, so the side is not a function of the input amount.                                   |

A rejection costs one trade direction only: the opposite side of the same book keeps serving, exactly as it does when the venue quotes one side empty. Levels that repeat a knot and agree on its price conflict with nothing — Titan does repeat a cumulative volume across consecutive levels.

That isolation holds for sides the parser typed successfully, which is where these four reasons live. A frame whose JSON does not read into the typed numbers at all — a syntactically valid `1e400`, say, which overflows the `f64` that `price` and `cumVolume` are read as — fails before any side exists to isolate, so the stream drops the whole frame and counts it under `price_level_stream_frames_rejected_total{reason="parse_error"}`. Freshness fails closed there: nothing from that frame is served, and the components it would have carried go stale on the usual `stale_after` clock.

One corrupt shape slips past all four: a side whose `amount_in` strictly increases while its cumulative `amount_out` falls converts cleanly, so the state refuses it when you quote it instead. `get_amount_out` then returns a `RecoverableError` reporting that the quote ladder `is not monotonically increasing in amount_out` around the amount you asked for. That refusal fires only when the two levels enclosing your amount fall, so the rest of the ladder keeps quoting until the next frame replaces it.

## Simulation overrides

A pAMM ladder prices the block Titan is building, so an `eth_call` against mined state simulates a venue Titan never quoted: the venue rejects the fill as stale, or your route pays a fallback price. Titan therefore serves a second WebSocket, `pamm_quote_stream_v2`, carrying the account state it simulated each venue on to produce that venue's ladders. The stream reads that socket by default, holds every snapshot it accepts until its build's slot runs out, and attaches one to each state whose books name the exact build it belongs to. `simulation_overrides()` hands you what a state carries:

```rust
let Some(overrides) = state.simulation_overrides() else {
    // The ladders quote as they always do; nothing here says what Titan simulated the venue on.
    return;
};
```

Quoting never reads that field: `spot_price`, `get_amount_out` and `get_limits` answer off the ladders alone. A quote stream that refuses the connection, falls silent, or sends a payload this parser declines therefore costs the states their snapshot and nothing else — it removes no component, changes no quote, and delays no update.

**A stamp carries Titan's claim, not a fact this stream establishes.** One book says the venue priced its ladders at one version of its pair; one venue entry says one snapshot is the state it priced that version on. That both statements describe a single simulation is an assumption about the producer, and neither message lets this code check it. Tycho validates that assumption at the release gate, by re-simulating stamped states and holding the result against the ladders they arrived with.

### The sequence contract

A book names the version behind its ladders in `calculated_from`. A venue entry of the quote stream advertises `pairSequences`, one sequence per entry of `assetPairs`:

```json
{
  "venues": [
    {
      "maker": "TaurusFi",
      "quoteAddress": "0x3ce2672aa806138585920421406bf0decb8130cb",
      "blockNumber": 26148760,
      "slot": 15387623,
      "timestamp": 1791475494775213874,
      "assetPairs": [
        {
          "token_1": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
          "token_2": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        }
      ],
      "pairSequences": [283851543383726791766048835],
      "stateOverride": {
        "0xda7afeed021eafc1c1af9c362de477dad0396b81": {
          "balance": "0x0",
          "nonce": "0x1",
          "stateDiff": {
            "0x8dc4adfc36586ffb9fe5c2dba18756239463dda9032bbfa7fd13f3bb3b16358f": "0x6ac7bf2b01000000000001a11c42ae030000007600000076000000000003969f"
          }
        }
      }
    }
  ]
}
```

That example abridges the captured entry, which carries two pairs and three accounts, plus a `pricedAddresses` list the parser ignores as it ignores every other field either message adds.

The join runs on the **full identity** — the venue, the pair, and the sequence — and matches exactly:

* **A sequence alone identifies nothing.** Titan stamps a batch of pairs with one sequence: one captured entry repeats a single sequence across all eleven pairs it advertises. Sequences also run past `u64::MAX` — the 2026-10-08 capture's sit near 2.8 · 10²⁶ — so the stream reads them as `u128`.
* **Membership in a venue's entry is no match either.** The same capture holds a Fermi book whose reference that very Fermi entry advertises — for WBTC/USDT and cbBTC/USDT, while the entry's own WETH/USDC pair carries a different sequence. That book stays unstamped. The stream never substitutes a neighboring pair's snapshot, however close the two look.
* **Orientation does not matter.** The key sorts the pair's two addresses, so a book quoting `token1`/`token2` the other way round still finds the entry's pair.
* **Venue identity resolves the same way on both sockets.** Each socket names a venue by the address Titan streams it under, and the stream normalizes both through the alias table of [Venue identity](#venue-identity) — so Kipseli's snapshot reaches the ladders Kipseli serves under its public identity.
* **Build time orders two snapshots of one version, not arrival time.** An entry's `timestamp` is the nanosecond Titan built it. A newer build of a pair version replaces the one the stream holds; an older one changes nothing. Titan re-stamps a pair several times a slot, and the two sockets deliver independently, so the receive order decides nothing here.

### Reading the embedded overrides

`PammSimulationOverrides`, in `tycho_simulation::price_level_stream::overrides`, carries plain data, shaped for an `eth_call`-style state override:

| field             | what it holds                                                                                                                                                                                |
| ----------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `accounts`        | the overridden accounts, keyed by address. Each carries `balance` (Titan's hex quantity as minimal big-endian bytes), `nonce`, and `state_diff` — 32-byte storage slot to 32-byte word.    |
| `block_number`    | the L1 block to simulate the snapshot on, when the entry named one.                                                                                                                          |
| `block_timestamp` | the Unix second to date that block at, which comes from the entry's beacon slot and from nothing else. `None` when the entry named no plausible slot: a simulation then dates the block itself. |
| `built_at_ns`     | the nanosecond Titan built the entry. It orders two snapshots of one pair version by source recency.                                                                                          |
| `expires_at`      | the Unix second from which the snapshot no longer describes anything live. See [Expiry](#expiry).                                                                                             |

An absent field means Titan's simulation read the chain's value, never that the value is zero: `state_diff` is a diff rather than a full storage image, and an account that overrode only storage carries neither a balance nor a nonce. `code` and a whole-storage `state` replacement never appear — the validation declines an entry carrying either, whole, instead of quietly dropping the field.

Pass `accounts` as the geth State Override Set (`eth_call` param 3, or `eth_simulateV1`'s `stateOverrides`) and `block_number` with `block_timestamp` as `BlockOverrides` (param 4). Merge `accounts` into whatever overrides you already build rather than replacing them: a snapshot describes the venue, and it says nothing about your own sender balance and allowance.

The block timestamp matters as much as the storage does: a pAMM reads the block time in its own staleness check, so a call that applies the storage overrides and keeps the mined timestamp can still revert. Param 4 changes the EVM context only, not the state param 2 selects — so simulate promptly against latest rather than reading `block_number` as a base-state selector.

### Expiry

`expires_at` is **off-chain guidance only**, in Unix seconds: the earlier of two deadlines — the Unix second the state's own ladders stop quoting at, and the second the snapshot's own lifetime ends, at most one slot after Titan built it. Simulate before it.

Nothing about it claims on-chain validity. A call that overrides the block timestamp can succeed long after `expires_at`, and such a success says nothing about what the venue would fill — the deadline tells you when to stop trusting the result, and makes staleness diagnosable. The narrowing lands on the copy the state carries; the snapshot the stream still holds for other pairs of the same entry keeps its own, unnarrowed deadline.

`without_quote_guard()` does not widen the field. A state that keeps quoting past its ladder deadline still reports the second its ladders went stale, so a consumer quoting late by design can still see how old the pre-state under its simulation is.

**The snapshot half of that deadline is a fact about the build, not about when the stream admitted it.** It is the second the build's own deadline falls in, floored once and therefore never later than the instant the stream itself stops serving the build. Two consequences worth relying on: the same build re-sent by Titan reports the same second however late the repeat arrives, including across a removal and a re-admission; and the second never moves later than the first one reported for that build. Both hold under the clock model the stream supports — a wall clock and a monotonic clock that advance together, neither stepping backwards. A host whose wall clock is stepped backwards between two frames is outside it, and no deadline is promised across such a step.

### When a state carries no snapshot

A pair the stream serves either carries a snapshot or arrives unstamped, and `price_level_stream_override_match_total{outcome}` names which case it fell into:

| `outcome`   | What happened                                                                                                                                                      |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `matched`   | The state carries the snapshot of the exact build its books named.                                                                                                 |
| `missing`   | The stream holds no snapshot for that venue, that pair and that sequence — the entry never arrived, never named this pair, or validation refused it.                |
| `ambiguous` | Several books of one venue and pair disagreed about their version, so nothing attributes the served ladders to a single snapshot. A stream anomaly; the capture holds none. |
| `expired`   | The stream held a build of that version and no longer serves it: its slot of lifetime passed, or what it held was removed while the build was still barred from re-admission ([Bounds and limits](#bounds-and-limits)). |
| `poisoned`  | Two payloads claimed to be one build of that version, so what the venue priced it on stays unknowable. The stream declines that build for the rest of its lifetime; a strictly newer build of the same version supersedes it as the served value, and stamps if the key has a slot for it ([Bounds and limits](#bounds-and-limits)). |

One further case produces no outcome at all, because it reaches no lookup: a book whose `calculated_from` the parser cannot read, and a book that names none, both leave nothing to look up. `price_level_stream_invalid_provenance_total` counts the unreadable case alone, so a parse regression cannot hide inside the ordinary rate of books that reference nothing. `without_simulation_overrides()` looks nothing up either — with no second socket the match-outcome, buffer and refusal series do not exist at all, while `price_level_stream_invalid_provenance_total` remains, because provenance is read off the price-level socket whether or not anything stamps.

A refused venue entry and a dropped build get no outcome of their own: both arrive as `missing`, because the price level side still looks the build up and simply finds none. Validation declines a whole entry that contradicts the documented shape — a malformed identity, a quantity that does not decode, an entry carrying no build stamp, no pair or no account, `pairSequences` and `assetPairs` of unequal length, `code` or whole-storage `state`, or a payload past the per-entry caps — and `price_level_stream_refused_override_entries_total{reason}` names which; every pair that entry carried then goes unstamped for the rest of its build. The buffer's own retention rules ([Bounds and limits](#bounds-and-limits)) drop or refuse a build the same way. Read both of those counters beside the match outcomes rather than instead of them: they name the cause a `missing` does not.

**Two honest contracts.** A state without a snapshot does not mean your simulation needs none: absence means the join found nothing usable, and simulating that leg against mined state can still revert or pay the fallback price. A state with a snapshot does not cover a whole route either: each state carries the snapshot of its own venue and pair alone, so a route across several pAMM legs reaches only as far as its weakest leg, and merging snapshots that target different builds remains a best-effort decision you make rather than a guarantee the stream hands you.

### Bounds and limits

Per venue entry, validation declines the whole entry under `over_cap` past 32 accounts, 512 storage words, or 64 KiB of storage. The byte cap counts the **raw streamed bytes** of the entry's slots and words — the decoded 32-byte widths, which keeps the check deterministic and free of any re-serialization; the equivalent JSON text runs more than twice as large — 2.1× to 2.3× on the committed fixtures, depending on how much of the formatting you count — so a text-measured 64 KiB would bind before the 512-word cap ever could. At the 32-byte width a word validates to, 512 words is 32 KiB and the word cap binds first; the byte cap bounds a payload that grows its words. The widest entry of either committed capture fixture overrode 9 accounts with 16 storage words, 1,024 raw bytes — 64× below the byte cap.

The stream also bounds what it retains between the two sockets, because Titan rather than a consumer decides that: 8 builds per venue and pair, 1,024 venue-and-pair keys, 16 venue entries per frame, and 32 pairs per entry. Past a bound the stream drops the oldest build of that pair, or the key whose traffic it saw longest ago. One exception keeps the retention honest: a key holding a record the stream still needs — a live poison record, the payload-less record of a build still barred from re-admission, or a snapshot still serving a build whose deadline was anchored to the admission that clamped its stamp — never loses it to eviction, because dropping any of the three would let the next replay of that build pass as if nothing had contradicted it and nothing had already served it. Such records count under the same two caps as the snapshots do, so a buffer whose every key protects one refuses a new key outright, under `rejected_keys_exhausted`. Each of those windows closes within a slot of the stamp it protects, and the record then goes when a sweep reaches it, so a buffer that is refusing frees itself without help. A rotating sweep visits every key within 32 frames, so a pair Titan stops streaming does not hold its last build until something asks for it. The 2026-10 capture streamed nine venues with 38 pairs between them, well below the key cap, which exists to bound a misbehaving upstream rather than Titan; the per-key depth is the one that can bind on ordinary traffic, as the next two paragraphs describe.

**A build's lifetime is fixed once, and a removal does not reset it.** A snapshot is served until its build's slot runs out — one slot from the stamp for a build Titan stamped at or before the clock that admitted it, which is the ordinary case and gives a late arrival only the remainder of its slot; one slot from that admission for a build stamped ahead of it — and no repetition of it extends that: Titan re-sending an envelope the stream already holds changes nothing, including its deadline. That rule needs one piece of bookkeeping when your host's clock trails Titan's — a build stamped *ahead* of the clock that admits it is given a slot from the admission rather than from its stamp, so re-admitting it later would compute a later deadline. The stream therefore keeps such a build's record, holding no payload, once expiry, supersession or contradiction has taken what it held, and refuses the identical envelope under `rejected_spent_replay` (or `rejected_older_than_poison`, for a build older than the one the record stands for) for as long as re-admission could outlast the first lifetime. Eviction never takes such a record at all: pressure refuses the arriving build instead, which the two paragraphs below cost out. Past that point the record is swept and the build is refused by arithmetic alone, under `rejected_expired`. A build stamped at or before the admitting clock is unaffected throughout: it is forgotten whole on removal, since every admission of it computes the same deadline.

**Two lifetimes, not one.** A snapshot is *served* for at most one slot — 12 s — and that is the only number that bounds what you can simulate on. A payload-less record of a build stamped ahead of the local clock is *retained* for up to one slot past that, so retention runs to 24 s in the worst case the accepted future-skew allowance permits. Retained is not served: such a record answers `expired`, carries no snapshot, and is never reported as a match.

**The cost of that retention is capacity, and it is accepted fail-closed.** Those records sit inside the same 8-builds-per-key bound as snapshots — the stream rejects an admission rather than discard a record it still needs — so capacity, not just contradiction, decides whether a build gets in. Concretely: once your host's clock trails Titan's by more than the delivery margin — a coherent skew as small as **+1 ms** does it — every build arrives stamped ahead of that clock, so every build is fenced; one distinct build per second for one venue and pair then fills all eight protected slots of its key, and the ninth build, the newest one its ladders would ask for, is refused under `rejected_key_saturated` until a horizon passes and frees a slot. Nothing is mis-served when that happens — the pairs keep quoting, and the states simply carry no snapshot — but recovery by a newer build is conditional on a slot being free, so this is a coverage cost rather than a correctness one. Watch `rejected_key_saturated` alongside your match outcomes, and keep the host's clock from drifting behind Titan's, which is what puts a key into this regime at all.

What they bound to is worth stating, because the product of the caps is not small: 1,024 keys × 8 builds, each build able to reference a snapshot of up to 512 storage words, puts the adversarial worst case on the order of **0.5 GiB of retained storage data** — against about 3 KiB on the measured stream, whose nine venue entries carried 3,136 bytes of raw slot-and-word data between them, the widest of them 1,024 bytes. That ceiling is finite, enforced, and fails closed, and nothing about it changes a quote; but it is the same process memory your quoting runs in, so retained memory is worth a place on the canary watch list when you roll this out.

`price_level_stream_override_buffer_total{event}` reports each of those decisions. Read it as retention, never as coverage: an eviction there says the stream dropped a snapshot it held, not that a ladder failed to find one.

### Coverage, as one capture measured it

In a 75 s capture of both EU sockets on 2026-10-08, **676 of 1,076 resolved books (62.8%)** had a prior exact-pair match, counted after the warmup — the upper bound reaches 64.35% if you credit books whose symbols never resolved. The misses decompose: 399 books named a pair version with no exact-pair occurrence anywhere in the capture (one more matched only an entry that arrived later); separately, 120 books had a prior hit at their venue under the *wrong* pair, all of them Fermi, and of the 280 with no prior venue-and-sequence hit, 263 had no such occurrence anywhere and 17 only later. For the books that did match, the snapshot arrived a median 495.8 ms and at most 1.897 s before its ladders.

Read those as finite-capture observations, not as steady-state guarantees, and not as a predicted rate of overrides on your routes — they count raw match opportunities, before poisoning, before the caps, and before anything admission refuses, which includes a future-skewed build, a saturated key and an exhausted buffer. An unmatched reference means Titan did not publish that snapshot during this capture, and nothing more. Lower coverage is truthful best effort: it is never a licence to stamp a snapshot from a different pair.

### Turning it off, and pointing it elsewhere

`without_simulation_overrides()` connects no second socket: every state then arrives unstamped, the stream holds no snapshot, and the match-outcome, buffer and refusal series do not exist at all — `price_level_stream_invalid_provenance_total` is the one that remains, because provenance is read off the price-level socket whether or not anything stamps. Use it for a consumer that only quotes the ladders — it spares the connection and the memory the unmatched snapshots occupy.

`override_endpoint(url)` names the quote stream socket, exactly as `endpoint` names the price level one; without it the stream honors the `TITAN_PAMM_QUOTE_STREAM_URL` environment variable, else the EU default. Name both endpoints when you move region: the two sockets of one region describe one build round, and nobody has established whether one region's snapshot joins another region's ladders.

## Metrics

The stream emits `price_level_stream_*` metrics through the `metrics` facade; a consumer that installs a `metrics` recorder receives them with no further setup, and without a recorder every call does nothing. The `venue` label carries the registered venue name, or the address of an auto-detected venue — the one exception to the rule that no label ever carries a value from the stream. A streamed token symbol never becomes a label, and neither does a component id, a pair version or any other address; the DEBUG log accompanying the counter names those instead. The `socket` label names one of the two Titan sockets, `price_levels` or `quote_stream`, and never anything else.

Per-venue series start at zero for every registered venue, and every `reason` of the two book counters starts at zero, so a reason that never fires reads as a visible zero rather than a missing series. The same holds for the override path and for the `quote_stream` socket while stamping stays on — `price_level_stream_invalid_provenance_total` alone starts at zero either way: 19 socket series exist then, against 12 with `without_simulation_overrides()`, which connects no second socket. The five rejection reasons `quote_stream` cannot carry stay deliberately unregistered, so a series that exists is one something can move.

**The `socket` label changes the identity of two already-released series.** `price_level_stream_frames_rejected_total` and `price_level_stream_reconnects_total` carried `reason` alone before this release. On deploy each old series goes stale and a labelled one starts from zero — harmless for `rate` and `increase`, and indistinguishable from a restart, but any recording rule, alert or `sum without(...)` keyed on the exact label set sees both the stale and the new series for the length of your staleness window.

* `price_level_stream_frames_accepted_total` — one increment per frame the tracker accepts. Titan pushes one frame per second, so a rate much below that points at delivery gaps, or at the rejections the next counter breaks down.
* `price_level_stream_frames_rejected_total{socket, reason}` — one increment per frame the stream drops, under `parse_error`, `too_old`, `in_future`, `out_of_order`, `block_regression` or `block_jump`. A steady rate on any reason means the upstream, the local clock, or your `stale_after` disagrees with the freshness contract. Only `parse_error` ever carries `socket = quote_stream`; every other reason is a freshness check of a price level frame.
* `price_level_stream_frame_age_seconds` — a histogram of every accepted frame's age at acceptance: the local wall clock minus the frame's `ts`, negative for a frame stamped ahead of the local clock. It measures Titan's lag plus delivery delay plus clock skew; above one slot, that frame's states refuse to quote.
* `price_level_stream_last_seen_timestamp_seconds{venue}` — the `ts` of the newest accepted frame that carried the venue, in seconds since the Unix epoch; 0 until the first such frame. A value that stops advancing means the venue went quiet.
* `price_level_stream_served_components{venue}` — how many components the stream currently serves for the venue. A drop to zero means every pair of that venue turned stale.
* `price_level_stream_stale_removals_total{venue}` — one increment per component the stream removes at its stale deadline. A burst across every venue at once points at the connection rather than at the venues.
* `price_level_stream_serving_state` — 0 while the stream serves nothing, 1 while it serves at least one component.
* `price_level_stream_reconnects_total{socket, reason}` — one increment per Titan connection the stream gives up on, or fails to establish, before it backs off: `idle_timeout`, `ended`, `closed`, `read_error`, `connect_failed` or `connect_timeout`, each on `price_levels` or `quote_stream`. A `quote_stream` socket that keeps dropping is a stamping outage where the same on `price_levels` is a quoting outage, and only the label separates the two.
* `price_level_stream_unregistered_pamm_entries_total` — one increment per venue and frame that the stream passes over because neither a registration nor auto-detection covers that venue, however many books the frame carried for it. A steady rate means Titan streams liquidity your configuration leaves on the table.
* `price_level_stream_skipped_books_total{reason}` — one increment per book the stream leaves unpriced, under `unbound_symbol`, `unadmitted_token` or `one_asset_pair`. A steady rate means an asset a venue quotes goes unserved; bind or admit it to serve it.
* `price_level_stream_rejected_sides_total{reason}` — one increment per book side the stream drops whole, under `unsupported_level`, `non_finite_amount`, `zero_amount` or `conflicting_knots`. Each increment leaves one trade direction unquotable while the opposite one keeps serving.

The four counters below cover the override path. Three move only while stamping stays on; `price_level_stream_invalid_provenance_total` moves either way, because a book's reference is read off the price-level socket whether or not anything stamps:

* `price_level_stream_override_match_total{outcome}` — one increment per served pair the stream joined to the snapshots it holds for that venue, under `matched`, `missing`, `ambiguous`, `expired` or `poisoned` ([When a state carries no snapshot](#when-a-state-carries-no-snapshot)). `matched` against the rest is the share of served pairs carrying the state behind their price. A pair whose books named no version produces no increment at all.
* `price_level_stream_override_buffer_total{event}` — one increment per decision of one ingest: `admitted`, `restamped`, `replayed`, `older_build`, `conflict_poisoned`, `rejected_poisoned_build`, `rejected_older_than_poison`, `rejected_spent_replay`, `rejected_future_skew`, `rejected_expired`, `rejected_key_saturated`, `rejected_keys_exhausted`, `rejected_frame_entry_cap`, `rejected_entry_pair_cap`, `degenerate_pair`, `evicted_entry`, `evicted_key` or `swept_expired`. These are retention events, not lookup misses — read them against [Bounds and limits](#bounds-and-limits), and read coverage off the counter above.
* `price_level_stream_refused_override_entries_total{reason}` — one increment per venue entry the validation declined whole, before any retention rule applied: `not_a_venue`, `not_an_account`, `malformed_pair`, `malformed_storage`, `malformed_quantity`, `unstamped`, `sequence_mismatch`, `no_data`, `unsupported_override` or `over_cap`. A steady rate means the producer sends something this parser does not accept, where a buffer event above means this process's own retention rule did its work.
* `price_level_stream_invalid_provenance_total` — one increment per **served** book whose version reference the parser cannot read. A book naming no reference never counts here, and neither does a book the stream left unpriced, so a parse regression stands out instead of blending into the ordinary miss rate. It counts per book where the match counter counts per pair, so do not ratio the two against each other.
