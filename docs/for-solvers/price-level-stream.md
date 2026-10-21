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

## What the stream serves

Every frame carries a flat list of books, one per venue and pair, and the stream emits one component per venue and pair. A component id concatenates the venue address with the pair's two token addresses, sorted ascending, so it survives a venue rename. The `pamm_address` static attribute carries the venue address your encoder sends the swap to.

`with_known_pamms()` registers the nine venues Tycho has measured — `bebop`, `elzorro`, `fermiswap`, `goliath`, `kipseli`, `metric`, `stelaxis`, `taurusfi` and `tempest` — each with its own per-swap gas estimate, and names their components after the venue. Titan quotes neither every venue nor every pair at all times, so the live set is usually smaller.

`auto_detect(true)` additionally serves every other venue Titan streams, naming it by its full lowercase hex address (`pricelevelstream:0x5979…`) and quoting it with a conservative gas estimate — the maximum over the measured venues, which `auto_detected_gas_cost` overrides. A venue keeps its component ids and its `pamm_address` when a later release measures it and the name replaces the address.

`add_pamm` registers one venue explicitly, overriding both the defaults and auto-detection; `deny_pamm` drops a venue and blocks auto-detecting it. Between those two the later call wins, and neither loses to `with_known_pamms()`, in any call order.

## Component families and execution

The stream emits components under one of two protocol system families, and that choice decides how tycho-execution executes their swaps:

* **`fallback:{pamm}`** (the default) — tycho-execution routes the swap through `TychoFallbackRouter`, which retries a reverted pAMM swap on the fallback pool your solution names in the swap's `user_data`. A pAMM quote targets the block Titan is building, so a venue reverts a fill against an older ladder; the fallback pool absorbs that case instead of losing the route.
* **`pricelevelstream:{pamm}`** — `without_fallback_router()` keeps every venue on this direct path. The swap then executes on the venue itself, and a stale maker quote reverts the whole route.

`{pamm}` is the venue name for a registered venue and the venue's lowercase hex address for an auto-detected one.

Each registered venue reports **one swap gas value for all its pairs**, calibrated from fills on the live venue. For most venues the gas barely varies by pair. Metric's does: it reports 280k, the p90 of its fills, while a USDC/USDT swap typically uses ~165k and a WBTC/USDC swap ~550k. Account for this if your router weighs gas against output on Metric's WBTC pairs.

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

A frame names its assets by address and prices every level in human units:

```json
{
  "slot": 15440991,
  "block": 26201953,
  "ts": 1792115918290990905,
  "books": [
    {
      "maker": "Fermi",
      "quoteAddress": "0x5979458912f80b96d30d4220af8e2e4925a33320",
      "token1": "WBTC",
      "token2": "USDC",
      "token1Address": "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599",
      "token2Address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
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
* `token1Address` and `token2Address` — the pair's base and quote assets. These are the identity the stream resolves.
* `token1` and `token2` — the same two assets' symbols. Display labels: the stream reads them for its DEBUG logs and nothing else.
* `price` — `token2` per `token1`, in human units, as the average execution price of the level's whole cumulative size.
* `cumVolume` — volume cumulated from the top of the book, in whole `token1`. It never decreases down a side, and consecutive levels can repeat a value.
* `maker` — Titan's display label for the venue, which the stream sometimes leaves empty. Nothing reads it; identity rests on `quoteAddress` alone.

Ask prices sit above bid prices, and either side can arrive empty when the venue quotes one direction of the pair only. The parser ignores every other field the stream carries.

## Which pairs are served

A book is served when your `with_tokens` metadata carries both of the addresses it names, and those tokens supply the decimals the conversion applies. There is nothing else to configure: Titan names each asset by address, so the stream resolves the identity directly rather than inferring one.

**Symbols are never resolved.** They travel on the book as display labels, and the stream reads them only for the DEBUG log beside a skipped book. A token in your metadata calling itself `WBTC` therefore neither captures another asset's quotes nor keeps the real one from being served.

A book the stream leaves unpriced is counted under `price_level_stream_skipped_books_total{reason}`:

* `unadmitted_token` — your token metadata does not carry one of the two addresses. Admit the asset to serve the pair.
* `malformed_address` — an address is not 20 bytes, so it identifies no asset. This is a producer-side fault, kept apart from `unadmitted_token` so it cannot hide inside the ordinary miss rate.
* `one_asset_pair` — both addresses name one asset, so the book states no pair to quote.

Of the 33 books in the 2026-10-16 capture, 29 name pairs of the five assets the test metadata admits; the other four quote QNT, LINK, AAVE and NVDAx against USDC, and serving them takes only admitting those tokens:

```rust
let price_level_stream = PriceLevelStreamBuilder::new()
    .with_known_pamms()
    .with_tokens(all_tokens.clone())
    .build();
```

The stream therefore preserves the quoting model for every pair whose assets you admit, subject to the float reconstruction [Conversion semantics](#conversion-semantics) documents; it does not promise to quote every pair Titan streams.

## Venue identity

A venue's identity in Tycho is its **public identity**: the address a swap goes to. Titan streams some venues under a different `quoteAddress` — a join key across its streams rather than an executable address — so the stream normalizes every such address at every boundary. Two venues differ today:

| venue | books arrive under | swaps go to |
|---|---|---|
| Kipseli | `0xc9a956b5196dfe110debe8781857bc8b97d32091` | `0x71e790dd841c8a9061487cb3e78c288e75ce0b3d`, the `KipseliPropAMMWrapper` router |
| Metric | `0xe844701934d9c6085b292e428f41b911cc0b3872` | `0x99ab7c1c5eab47801f7937075998527a550fba8a` |

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

Each level becomes one absolute quote: `amount_in` for a total `amount_out` that already contains every smaller level's liquidity. A state interpolates linearly between its knots, mirroring how Titan densifies its own simulated levels, and holds no information outside the quoted range: it rejects an input below the smallest level outright, and answers an input above the deepest level with a partial result at that level, as other level-based protocols do. `get_limits` reports the deepest level of the direction you ask about. The two sides convert independently, so a rejected side never costs the other one.

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

One corrupt shape slips past all four: a side whose `amount_in` strictly increases while its cumulative `amount_out` falls converts cleanly, so the state refuses it when you quote it instead. `get_amount_out` then returns a `RecoverableError` reporting that the quote ladder `is not monotonically increasing in amount_out` around the amount you asked for. That refusal fires only when the two levels enclosing your amount fall, so the rest of the ladder keeps quoting until the next frame replaces it.

## Metrics

The stream emits `price_level_stream_*` metrics through the `metrics` facade; a consumer that installs a `metrics` recorder receives them with no further setup, and without a recorder every call does nothing. The `venue` label carries the registered venue name, or the address of an auto-detected venue — the one exception to the rule that no label ever carries a value from the stream. A streamed token symbol never becomes a label; the DEBUG log accompanying its counter names it instead. Per-venue series start at zero for every registered venue, and every `reason` of the two book counters starts at zero, so a reason that never fires reads as a visible zero rather than a missing series.

* `price_level_stream_frames_accepted_total` — one increment per frame the tracker accepts. Titan pushes one frame per second, so a rate much below that points at delivery gaps, or at the rejections the next counter breaks down.
* `price_level_stream_frames_rejected_total{reason}` — one increment per frame the stream drops, under `parse_error`, `too_old`, `in_future`, `out_of_order`, `block_regression` or `block_jump`. A steady rate on any reason means the upstream, the local clock, or your `stale_after` disagrees with the freshness contract.
* `price_level_stream_frame_age_seconds` — a histogram of every accepted frame's age at acceptance: the local wall clock minus the frame's `ts`, negative for a frame stamped ahead of the local clock. It measures Titan's lag plus delivery delay plus clock skew; above one slot, that frame's states refuse to quote.
* `price_level_stream_last_seen_timestamp_seconds{venue}` — the `ts` of the newest accepted frame that carried the venue, in seconds since the Unix epoch; 0 until the first such frame. A value that stops advancing means the venue went quiet.
* `price_level_stream_served_components{venue}` — how many components the stream currently serves for the venue. A drop to zero means every pair of that venue turned stale.
* `price_level_stream_stale_removals_total{venue}` — one increment per component the stream removes at its stale deadline. A burst across every venue at once points at the connection rather than at the venues.
* `price_level_stream_serving_state` — 0 while the stream serves nothing, 1 while it serves at least one component.
* `price_level_stream_reconnects_total{reason}` — one increment per Titan connection the stream gives up on, or fails to establish, before it backs off: `idle_timeout`, `ended`, `closed`, `read_error`, `connect_failed` or `connect_timeout`.
* `price_level_stream_unregistered_pamm_entries_total` — one increment per venue and frame that the stream passes over because neither a registration nor auto-detection covers that venue, however many books the frame carried for it. A steady rate means Titan streams liquidity your configuration leaves on the table.
* `price_level_stream_skipped_books_total{reason}` — one increment per book the stream leaves unpriced, under `unadmitted_token`, `malformed_address` or `one_asset_pair`. A steady rate means an asset a venue quotes goes unserved; admit it to serve it.
* `price_level_stream_rejected_sides_total{reason}` — one increment per book side the stream drops whole, under `unsupported_level`, `non_finite_amount`, `zero_amount` or `conflicting_knots`. Each increment leaves one trade direction unquotable while the opposite one keeps serving.
