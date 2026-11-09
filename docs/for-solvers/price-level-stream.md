---
description: Streaming Titan's pAMM price levels as Tycho protocol components.
---

# pAMM Price Level Stream

Tycho Simulation consumes <a href="https://docs.titanbuilder.xyz/propamms/takers#pamm-price-level" target="_blank" rel="noopener noreferrer">Titan Builder's pAMM price level stream</a>: a WebSocket that pushes, once per second, the quote books Titan simulated for the pAMMs it builds blocks with. `PriceLevelStreamBuilder` turns each message into the same `Update`s the protocol stream emits, so you consume it like any other stream. It covers Ethereum Mainnet only.

```rust
use tycho_simulation::price_level_stream::stream::PriceLevelStreamBuilder;

let price_level_stream = PriceLevelStreamBuilder::new()
    .with_known_pamms()       // serve the venues Tycho has measured
    .auto_detect(true)        // also serve any other venue Titan streams
    .with_tokens(all_tokens.clone())
    .build();
```

`with_tokens` takes the same token metadata the protocol stream uses; load it with `load_all_tokens`, as the [Simulation](simulation.md) page describes. The stream connects on its first poll, re-establishes the connection after a disconnect or a 10 s gap between messages, and never terminates on its own, so run it in its own task alongside your protocol stream. `endpoint()` points it at a closer Titan region than the EU default; without that call it honors the `TITAN_PAMM_PRICE_LEVEL_URL` environment variable. The stream also reads Titan's quote stream by default, to attach [Simulation overrides](#simulation-overrides) to the states it emits; nothing about quoting depends on it.

## What the stream serves

Every message carries a flat list of books, one per venue and pair, and the stream emits one component per venue and pair. A component id concatenates the venue address with the pair's two token addresses, sorted ascending, so it survives a venue rename. The `pamm_address` static attribute carries the venue address your encoder sends the swap to.

`with_known_pamms()` registers the nine venues Tycho has measured — `bebop`, `elzorro`, `fermiswap`, `goliath`, `kipseli`, `metric`, `stelaxis`, `taurusfi` and `tempest` — each with its own per-swap gas estimate, and names their components after the venue. Titan quotes neither every venue nor every pair at all times, so the live set is usually smaller.

`auto_detect(true)` additionally serves every other venue Titan streams, naming it by its full lowercase hex address (`pricelevelstream:0x5979…`) and quoting it with a conservative gas estimate — the maximum over the measured venues, which `auto_detected_gas_cost` overrides. A venue keeps its component ids and its `pamm_address` when a later release measures it and the name replaces the address.

`add_pamm` registers one venue explicitly, overriding both the defaults and auto-detection; `deny_pamm` drops a venue and blocks auto-detecting it. Between those two the later call wins, and neither loses to `with_known_pamms()`, in any call order.

## Component families and execution

The stream emits components under one of two protocol system families, and that choice decides how tycho-execution executes their swaps:

* **`fallback:{pamm}`** (the default) — tycho-execution routes the swap through `TychoFallbackRouter`, which retries a reverted pAMM swap on the fallback pool your solution names in the swap's `user_data`. A pAMM quote targets the block Titan is building, so a venue reverts a fill against an older price it has not refreshed; the fallback pool absorbs that case instead of losing the route.
* **`pricelevelstream:{pamm}`** — `without_fallback_router()` keeps every venue on this direct path. The swap then executes on the venue itself, and a stale maker quote reverts the whole route.

`{pamm}` is the venue name for a registered venue and the venue's lowercase hex address for an auto-detected one.

Each registered venue reports **one swap gas value for all its pairs**, calibrated from fills on the live venue. For most venues the gas barely varies by pair. Metric's does: it reports 280k, the p90 of its fills, while a USDC/USDT swap typically uses ~165k and a WBTC/USDC swap ~550k. Account for this if your router weighs gas against output on Metric's WBTC pairs.

Distinct component ids do not imply distinct liquidity. A venue the stream serves may also reach you through another integration path — an on-chain integration under `vm:{pamm}`, or an RFQ client for the same market maker (see [Request for Quote Protocols](request-for-quote-protocols.md)) — and both paths then price the same inventory. Deduplicate by venue through `pamm_address` wherever double-counting matters, such as routing over the combined liquidity.

## Freshness contract

Titan's messages are best effort, and none lists every venue: a venue or a pair can drop out of one message and return in the next — in a 15 minute capture, 7.7% of messages omitted something the surrounding messages carried — so the stream never reads absence as retirement. Instead it serves a component only while an accepted message carried it within the last `stale_after`, 24 s (two slots) by default.

### Which messages the stream accepts

The tracker accepts a message only if its `ts`

* is younger than `stale_after`,
* lies no more than one slot ahead of the local wall clock, and
* is no older than the newest accepted message's — an equal timestamp passes, because Titan can send several messages with one `ts`,

and only if its `block` neither falls below the newest accepted block nor exceeds it by more than one block per elapsed slot plus two. A message that fails any of these changes nothing and increments `price_level_stream_frames_rejected_total` under the matching reason. The beacon `slot` carries no freshness decision, so the tracker judges a message without one exactly like a message carrying one.

### How components turn stale

A component that no accepted message has carried for `stale_after` turns stale. The stream then lists it in `removed_pairs`, together with every other component that turned stale at the same instant, and the next accepted message carrying it re-adds it in `new_pairs`. Removal and re-add always arrive as separate updates. Silence, a disconnect, keepalive-only or unparsable traffic, and replayed messages all end in this same removal.

Recovery runs per component: a message carrying one pair re-adds that pair and nothing else, and a message carrying one direction of a pair re-adds the pair with the other direction unquotable. Treat a removal like any other — stop routing through the component until it reappears in `new_pairs`. A stale removal and a retired venue look identical from the outside, by design.

### The one-slot quote guard

Every state the stream emits refuses `spot_price`, `get_amount_out` and `get_limits` once its message's `ts` is one slot (12 s) old, so you cannot quote price levels a slot after Titan built them, even before the removal arrives. The deadline lives on this process's monotonic clock and never crosses serialization, so a replayed recording always quotes.

`without_quote_guard()` emits states that never refuse. Opt out only for a consumer that quotes a state more than one slot after it arrived by design — a batch simulator, a validation harness — and never on a live router: `stale_after` then becomes the only bound on how old the quoted price levels can be, and the venue may no longer fill such a quote.

The guard is deliberately loose. Between quoting and inclusion the builder usually refreshes a pAMM's state, so a quote for the previous block often still fills, and the fallback router covers the cases where it does not: the price moved past your slippage tolerance, or the builder did not refresh the venue or does not support pAMMs. In a recording on 2026-10-21, a later block followed 789 of the 795 messages; 688 of those 789 (87.2%) could still quote when the first message of the next block arrived, with a median of 5.3 s left on the guard.

### Block numbers and partial updates

Quotes target the block currently being built, so the stream marks every update partial: an update supersedes the previous one for the pairs it contains and says nothing about the pairs it omits. Each update carries the block its message targets, a removal the newest accepted block. That number never decreases while the stream serves something. Once every component has turned stale, the stream judges the next accepted message as a first message, which may carry a lower block — that is how it recovers from a message with an implausible block, and why you must not order updates by block number across such a gap.

`stale_after(duration)` overrides the window, up to one hour. Set it shorter than the age messages arrive with and the stream rejects every one of them as `too_old`, which the rejection counter and a WARN log both show.

## The V2 message

A message names its assets by address and prices every level in human units:

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
      "calculated_from": 284836009221452523116691695,
      "bids": [{ "price": 80970.38, "cumVolume": 0.0005 }],
      "asks": [{ "price": 80997.89405475458, "cumVolume": 0.00012346 }]
    }
  ]
}
```

* `block` — the L1 block the quotes target, the one Titan is building.
* `slot` — the beacon slot, when Titan reports one.
* `ts` — when Titan built the message, in nanoseconds since the Unix epoch. Titan can send several messages with one `ts`, which makes it a freshness marker rather than an identity.
* `quoteAddress` — the venue key, which is not necessarily the venue's executable address. See [Venue identity](#venue-identity).
* `token1Address` and `token2Address` — the pair's base and quote assets. These are the identity the stream resolves.
* `token1` and `token2` — the same two assets' symbols. Display labels: the stream reads them for its DEBUG logs and nothing else.
* `price` — `token2` per `token1`, in human units, as the average execution price of the level's whole cumulative size.
* `cumVolume` — volume cumulated from the top of the book, in whole `token1`. It never decreases down a side, and consecutive levels can repeat a value.
* `maker` — Titan's display label for the venue, which the stream sometimes leaves empty. Nothing reads it; identity rests on `quoteAddress` alone.
* `calculated_from` — the quote sequence Titan priced the book at; the stream matches the book to the quote stream's account state by it (see [Simulation overrides](#simulation-overrides)). The stream reads it leniently: a value it cannot read costs the book its overrides, never its quotes.

Ask prices sit above bid prices, and either side can arrive empty when the venue quotes one direction of the pair only. The parser ignores every other field the stream carries.

## Which pairs are served

A book is served when your `with_tokens` metadata carries both of the addresses it names, and those tokens supply the decimals the conversion applies. There is nothing else to configure: Titan names each asset by address, so the stream resolves the identity directly rather than inferring one.

**The stream never resolves symbols.** They travel on the book as display labels, and the stream reads them only for its DEBUG logs. A token in your metadata calling itself `WBTC` therefore neither captures another asset's quotes nor keeps the stream from serving the real one.

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

A book prices its pair in human units while the states serve integer amounts, so the conversion applies the resolved tokens' decimals. The stream reads `price` as the average execution price of the level's cumulative size, in `token2` per `token1`, and `cumVolume` as whole `token1`. The bids therefore sell `token1` and the asks sell `token2`:

```text
bids (token1 -> token2):
  amount_in  = round(cumVolume · 10^decimals(token1))
  amount_out = round(price · cumVolume · 10^decimals(token2))

asks (token2 -> token1):
  amount_in  = round(price · cumVolume · 10^decimals(token2))
  amount_out = round(cumVolume · 10^decimals(token1))
```

Each level becomes one absolute quote: `amount_in` for a total `amount_out` that already contains every smaller level's liquidity. A state interpolates linearly between its knots, mirroring how Titan densifies its own simulated levels, and holds no information outside the quoted range: it rejects an input below the smallest level outright, and answers an input above the deepest level with a partial result at that level, as other level-based protocols do. `get_limits` reports the deepest level of the direction you ask about. The two sides convert independently, so a rejected side never costs the other one.

**Precision.** Titan quantizes both `price` and `cumVolume` to `f64` before sending them, so the converted amounts carry Titan's own precision and nothing tighter. The conversion rounds every amount to the nearest base unit and keeps amounts beyond `u64::MAX` exact — an 18-decimal token passes that bound at about 18.5 tokens — but it promises no atomic-unit parity with whatever the venue computes internally.

### Rejected sides

When a side's levels do not state a usable price function, the stream drops that whole side instead of the offending level: the state interpolates between levels, so dropping one level would turn a broken price function into a plausible invented one. Four cases reject a side, and `price_level_stream_rejected_sides_total` counts each one:

| `reason`            | Cause                                                                                                                                                      |
| ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `unsupported_level` | A level's `price` or `cumVolume` is not a finite positive number, so the level states no price at all.                                                      |
| `non_finite_amount` | A level's amount overflows `f64`, which leaves its size unknown.                                                                                           |
| `zero_amount`       | A level rounds to a zero `amount_in` or `amount_out`. A knot at zero is not a quote, and interpolating across it fabricates depth the venue never offered.  |
| `conflicting_knots` | Two levels round to the same `amount_in` but disagree on `amount_out`, so the side is not a function of the input amount.                                   |

A rejection costs one trade direction only: the opposite side of the same book keeps serving, exactly as it does when the venue quotes one side empty. Levels that repeat a knot and agree on its price conflict with nothing — Titan does repeat a cumulative volume across consecutive levels.

One corrupt shape slips past all four: a side whose `amount_in` strictly increases while its cumulative `amount_out` falls converts cleanly, so the state refuses it when you quote it instead. `get_amount_out` then returns a `RecoverableError` reporting that the quotes are `not monotonically increasing in amount_out` around the amount you asked for. That refusal fires only when the two levels enclosing your amount fall, so the rest of the side keeps quoting until the next message replaces it.

## Simulation overrides

A pAMM quote prices the block Titan is building. If you simulate a pAMM swap with `eth_call` on mined state, you simulate a venue state Titan never priced: the venue rejects the swap as stale, or a wrapper pays a fallback price. Titan publishes the account state it simulated each venue on over a second WebSocket, `pamm_quote_stream_v2`. The stream reads it by default and attaches to each state the venue entry that matches its book, which `simulation_overrides()` returns:

```rust
let Some(overrides) = state.simulation_overrides() else {
    // The state quotes as it always does; you have no account state to simulate it on.
    return;
};
```

### Two kinds of simulation

Two different things on this page go by the name simulation, and they fail in different ways.

**Tycho's quote.** `get_amount_out`, `spot_price` and `get_limits` on `PriceLevelStreamState` do integer arithmetic over the quotes the stream built from Titan's price levels. That path makes no node call and reads no account state, and it is what you route on. It refuses only for the reasons `price_level_stream_quotes_refused_total{reason}` lists.

**Your own `eth_call`.** You send the encoded swap to a node with the venue's account state applied as state overrides. This call can revert, and it can return an amount different from the one Tycho quoted.

The simulation overrides let you run the second to check the first. **Use them to check Tycho's answer, not as an input to a route:** nothing in the quoting path reads them, and a state without overrides quotes exactly as one with them.

### How a state gets its overrides

Every book names, in `calculated_from`, the quote sequence Titan priced it at. Every venue entry of the quote stream lists, in `pairSequences`, the quote sequence its account state carries for each pair in `assetPairs`. Titan does not publish an entry for every quote sequence, so most books name a quote sequence that no entry carries. A book arrives about a second after the entries Titan published before it, so the stream already holds those entries when the book arrives, as long as you poll the stream again as soon as you have handled each update. The stream reads both sockets only while you poll it, and then in no fixed order: after a pause, it can read a book before entries that reached their socket earlier, and that book's state gets an earlier entry or none.

The stream therefore gives each state the newest entry for the book's venue and pair that comes from the book's beacon slot and has a quote sequence at or below the book's: the latest account state Titan published before it priced the book.

* **The venue and the pair both have to match.** Titan prices a batch of pairs at once and gives them all one quote sequence, so the stream never takes an entry that another pair or another venue carries, however closely its quote sequence fits.
* **The slot has to match.** A quote sequence carries its beacon slot in its high 64 bits, so the stream never gives a state an entry from another slot. Titan does not document that encoding; `price_level_stream_sequence_slot_mismatch_total` watches it.
* **Some venues read more than one quote address.** An entry's `pricedAddresses` lists every venue whose swaps read its state. The stream also stores each entry under those venues, and merges it into each venue's own entry; the venue's own entry wins wherever both set a value. Another quote address's entry follows the same rule as the venue's own: it merges only when its quote sequence is at or below the book's. The stream compares the numbers only, so it cannot tell whether that quote address counts on the venue's counter. In the 2026-10-21 recording, three quote addresses named Metric's router in `pricedAddresses`. The first is Metric's own. The entries of the second, `0x30a88bf8…`, merged, and some Metric swaps revert without its state. The third, `0x6dca2500…`, numbers its quote sequences by the millisecond, so its entries always sat above Metric's books and never merged. That cost nothing: its entries set one storage slot of `0xe844…`, and on 2026-10-22 none of 16 traced Metric WETH/USDT swaps read that slot. All 16 reproduced their quote to the wei without it. `price_level_stream_override_skipped_sources_total` counts the quote addresses the merge skips.
* **Of two entries with one quote sequence, the later-built one wins**, by `timestamp` rather than by arrival order. When Titan sends one entry twice, the stream keeps the copy it already holds: the second copy carried, in the 2026-10-21 recording, a subset of the first one's state.
* **The pair's orientation does not matter,** and both sockets normalize a venue's address through the alias table of [Venue identity](#venue-identity), so Kipseli's and Metric's entries reach the states served under their public identity.

This is one venue entry of a quote stream message from the 2026-10-16 capture; the message lists one such entry per venue:

```json
{
  "venues": [
    {
      "maker": "Goliath",
      "quoteAddress": "0x4367985a63063fc970b009c6e599e9225181f551",
      "pricedAddresses": ["0x4367985a63063fc970b009c6e599e9225181f551"],
      "assetPairs": [
        {
          "token_1": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
          "token_2": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        }
      ],
      "pairSequences": [284836027668196596826243132],
      "slot": 15440992,
      "blockNumber": 26201954,
      "timestamp": 1792115920127487452,
      "stateOverride": {
        "0x48165e588ec68e724315d3e0fe7b0f3b9b6c7a6d": {
          "balance": "0x1b6008ea9711c3f3",
          "nonce": "0xe40"
        },
        "0xb261ef0a97bc7cf3af2025ab6b3792e6062d3d4a": {
          "balance": "0x0",
          "nonce": "0x1",
          "stateDiff": {
            "0x0000000000000000000000000000000000000000000000000000000000000000": "0x000000006ad184d800003c06d28e200000000000000000000000000090db0604"
          }
        }
      }
    }
  ]
}
```

Quote sequences run past `u64::MAX`, so the stream reads them as `u128`.

### Using the overrides in `eth_call`

`PammSimulationOverrides`, in `tycho_simulation::price_level_stream::overrides`, carries plain data:

| field             | what it holds                                                                                                                                                       |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `accounts`        | the overridden accounts, keyed by address. Each carries `balance` (Titan's hex quantity as minimal big-endian bytes), `nonce`, and `state_diff` — 32-byte storage slot to 32-byte word. |
| `block_number`    | the block Titan was building when it took this state. `None` when the entry named none.                                                                             |
| `block_timestamp` | the Unix second of that block, from the entry's beacon slot (`1606824023 + 12 · slot`). `None` when the entry named no slot.                                         |
| `built_at_ns`     | the nanosecond Titan took this state. `expires_at()` returns the Unix second one slot later, for information only: nothing in Tycho acts on it.                     |

An absent field means Titan's simulation read the chain's value, never that the value is zero: `state_diff` maps onto `stateDiff`, which changes only the slots it names, never onto `state`, which replaces the whole storage. The stream refuses a whole entry that carries `code` or a `state`, rather than dropping the field.

The fields map onto the State Override Set — the third `eth_call` parameter, or `stateOverrides` in `eth_simulateV1` — but the type is not its JSON-RPC shape, so convert each field into your client's type: with alloy, `balance` becomes a `U256` read from its big-endian bytes, `nonce` a quantity, and `state_diff` the account's `stateDiff`. Merge `accounts` into the overrides you already build: they describe the venue, not your sender's balance or allowance. Call on the latest mined block, and pass `block_number` and `block_timestamp` as block overrides, the fourth parameter. Venues compare the block's timestamp with the price they stored, so a call that keeps the mined block's context reverts even with the right storage.

For a route across several pAMM legs, merge every leg's overrides into one State Override Set.

### What a successful simulation tells you

With those block overrides, the venue's staleness check (`StaleUpdate()`) passes however old the overrides are. A successful call therefore tells you the quote can execute **if** the venue's state is current at that price when your transaction lands, not that it will. Between quoting and inclusion the builder usually refreshes a pAMM's state, so a quote for the previous block often still fills. It fails on chain in two cases — the price moved past your slippage tolerance, or the builder did not refresh the venue or does not support pAMMs — and the fallback router covers both (see [Component families and execution](#component-families-and-execution)).

For the same reason the stream never invalidates a quote because of its overrides: a state quotes for as long as its price levels are valid, as [The one-slot quote guard](#the-one-slot-quote-guard) describes. If you want to know how old the overrides are, compare `block_number` with the block you simulate for.

### When a state carries no overrides

`price_level_stream_override_match_total{venue, outcome}` counts every served pair under one outcome:

| `outcome`           | What happened                                                                                                                         |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `same_sequence`     | The state carries the venue's entry at the book's own quote sequence.                                                                 |
| `earlier_sequence`  | The state carries the venue's newest entry for the pair at an earlier quote sequence of the same slot.                                |
| `none_in_slot`      | The venue had no entry for the pair in the book's slot at or below its quote sequence, so the state carries no overrides.            |
| `no_quote_sequence` | The book named no quote sequence, or one the parser cannot read, so the stream looked nothing up.                                     |

A state without overrides does not mean its swap needs none: it means you cannot simulate that leg on Titan's account state, and simulating it on mined state can still revert. Three other counters say why the stream held no usable entry:

* `price_level_stream_override_entries_refused_total{reason}` — the stream refused a venue entry whole, so the pairs it carried got nothing from it.
* `price_level_stream_override_buffer_total{event}` — what the stream did with each entry it received, including the ones it did not store.
* `price_level_stream_unreadable_quote_sequence_total` — books whose `calculated_from` the parser cannot read.

The stream only stores entries for venues it serves, and it starts serving an auto-detected venue with the first message that carries its books, so that venue's states from that message get no overrides.

### Limits

* **Per entry.** The stream refuses an entry with more than 32 accounts or 512 storage words (`over_cap`), and one whose storage slots or words are not 32 bytes (`malformed_storage`).
* **Across entries.** The stream keeps the entries from the beacon slot of the latest price level message and from one slot either side, and drops the rest whenever that slot moves on. Before the first price level message, the newest slot the entries name places that window. It reads no clock to decide this, so while the price level socket is down the window stays put and newer entries count as `arrived_outside_slot_window`. It holds at most 1,000,000 accounts and storage words in all, and counts an entry once for each venue and pair it holds the entry under. That keeps the buffer to a few hundred megabytes: past that it stores nothing new, not even a newer copy of an entry it holds, until the slot moves, and counts `buffer_full`.
* **Decoding.** The stream decodes each venue entry on its own, so an entry it cannot read costs that entry alone (`undecodable`), not the rest of the message.

### What the 2026-10-21 recording showed

A 13 minute recording of both EU sockets — 795 price level messages, 8,022 quote stream messages, 29,772 books and 77,377 venue entries — showed:

* 56.9% of books found their venue's entry at their own quote sequence, and 99.7% at their own or an earlier one of the same slot.
* Every entry arrived before the book that named it: 0.83 s earlier at the median, 3.8 s at most.
* No quote sequence appeared in two blocks, and every quote sequence carried its message's slot.
* Titan sent 268 entries a second time with fewer accounts and slots and the same values.
* 1.2% of entries named no pair and no account, which the stream refuses as `empty`: about one a second.
* All 795 price level messages named their beacon slot.
* A replay of the recording through these rules, serving every venue Titan streams, held at most 11,655 copies of entries at once, one per venue and pair, carrying 148,444 accounts and storage words.

On mainnet, the live test simulates one leg per venue the way a client does: on the parent block, with the quoted block's number and timestamp. In four runs, at blocks 26244499, 26244560, 26244907 and 26248421, all 36 legs of the 9 venues reproduced their quoted amount within 1 bps, and 32 of them to the wei; Metric's overrides held its own entry with `0x30a88bf8…`'s merged in. The last run overrode only the block's number and timestamp, and reproduced all 9 of its legs to the wei; the three before it also overrode the block's coinbase and base fee. A separate run whose overrides came from an earlier quote sequence reproduced 10 of 10 legs within 1 bps. Without overrides, 35 of the 36 legs reverted; Metric's USDC/USDT leg succeeded once. These simulations applied the storage part of the overrides only. Read all of this as a few hours of observations, not as guarantees.

### Turning it off, and pointing it elsewhere

`without_simulation_overrides()` connects no quote stream socket: every state then arrives without overrides, the stream stores nothing, and the series that [Metrics](#metrics) lists as existing only while overrides are on do not exist. Use it for a consumer that never simulates a swap with `eth_call`.

`quote_stream_endpoint(url)` sets the quote stream socket's endpoint, as `endpoint` sets the price level one; without it the stream honors the `TITAN_PAMM_QUOTE_STREAM_URL` environment variable, else the EU default. Set both endpoints when you move region: a region's quote stream matches that region's price levels, and nobody has tested whether it matches another region's. The stream logs a WARN when you customize only one of the two.

## Metrics

The stream emits `price_level_stream_*` metrics through the `metrics` facade; a consumer that installs a `metrics` recorder receives them with no further setup, and without a recorder every call does nothing. The `venue` label carries the registered venue name, or the address of an auto-detected venue — the one exception to the rule that no label ever carries a value from the stream. A streamed token address or symbol, a component id and a quote sequence never become labels; the DEBUG logs that accompany the counters name them instead. Per-venue series start at zero for every registered venue, and every `reason` of the two book counters starts at zero, so a reason that never fires reads as a visible zero rather than a missing series.

* `price_level_stream_frames_accepted_total` — one increment per message the tracker accepts. Titan pushes one message per second, so a rate much below that points at delivery gaps, or at the rejections the next counter breaks down.
* `price_level_stream_frames_rejected_total{socket, reason}` — one increment per message the stream drops, under `parse_error`, `too_old`, `in_future`, `out_of_order`, `block_regression` or `block_jump`. A steady rate on any reason means the upstream, the local clock, or your `stale_after` disagrees with the freshness contract. Only `parse_error` ever counts on `socket="quote_stream"`; the other reasons check price level messages.
* `price_level_stream_frame_age_seconds` — a histogram of every accepted message's age at acceptance: the local wall clock minus the message's `ts`, negative for a message whose `ts` is ahead of the local clock. It measures Titan's lag plus delivery delay plus clock skew; above one slot, that message's states refuse to quote.
* `price_level_stream_last_seen_timestamp_seconds{venue}` — the `ts` of the newest accepted message that carried the venue, in seconds since the Unix epoch; 0 until the first such message. A value that stops advancing means the venue went quiet.
* `price_level_stream_served_components{venue}` — how many components the stream currently serves for the venue. A drop to zero means every pair of that venue turned stale.
* `price_level_stream_stale_removals_total{venue}` — one increment per component the stream removes at its stale deadline. A burst across every venue at once points at the connection rather than at the venues.
* `price_level_stream_serving_state` — 0 while the stream serves nothing, 1 while it serves at least one component.
* `price_level_stream_reconnects_total{socket, reason}` — one increment per Titan connection the stream gives up on, or fails to establish, before it backs off: `idle_timeout`, `ended`, `closed`, `read_error`, `connect_failed` or `connect_timeout`, on `socket="price_levels"` or `socket="quote_stream"`. A quote stream that keeps dropping costs states their overrides; a price level socket that keeps dropping costs quotes.
* `price_level_stream_unregistered_pamm_entries_total` — one increment per venue and message that the stream passes over because neither a registration nor auto-detection covers that venue, however many books the message carried for it. A steady rate means Titan streams liquidity your configuration leaves on the table.
* `price_level_stream_skipped_books_total{reason}` — one increment per book the stream leaves unpriced, under `unadmitted_token`, `malformed_address` or `one_asset_pair`. A steady rate means an asset a venue quotes goes unserved; admit it to serve it.
* `price_level_stream_rejected_sides_total{reason}` — one increment per book side the stream drops whole, under `unsupported_level`, `non_finite_amount`, `zero_amount` or `conflicting_knots`. Each increment leaves one trade direction unquotable while the opposite one keeps serving.
* `price_level_stream_quotes_refused_total{reason}` — one increment per call to `spot_price`, `get_amount_out` or `get_limits` that a state refused: `price_levels_expired`, `not_monotonic`, `unknown_pair`, `no_liquidity` or `below_smallest_quote`. It counts calls, so its rate follows how often your router asks. Watch `price_levels_expired`: a consumer that quotes promptly should almost never see it.
* `price_level_stream_duplicate_books_total` — one increment per book that replaces an earlier book of the same venue and pair in one message; the stream serves the last one. None of the 795 messages in the 2026-10-21 recording carried one; it exists because the replacement changes a served price without a trace.
* `price_level_stream_messages_without_slot_total` — one increment per accepted price level message that names no beacon `slot`. The stream still serves it, but cannot check its books' quote sequences against a slot, and the overrides' slot window then follows the newest slot those quote sequences carry. All 795 messages of the 2026-10-21 recording named one, so a non-zero rate is a change on Titan's side.

These series exist only while simulation overrides are on, except `price_level_stream_unreadable_quote_sequence_total` and the `price_levels` series of `price_level_stream_sequence_slot_mismatch_total`, which the stream reads off the price level socket either way:

* `price_level_stream_override_match_total{venue, outcome}` — one increment per served pair, under `same_sequence`, `earlier_sequence`, `none_in_slot` or `no_quote_sequence` (see [When a state carries no overrides](#when-a-state-carries-no-overrides)). The sum over `outcome` equals the pairs served.
* `price_level_stream_override_buffer_total{event}` — what the buffer did with each entry: once per served venue and pair it tries to store the entry under (`added`, `replaced_by_newer`, `older_than_held`, `resent_unchanged`, `resent_with_different_state` or `buffer_full`), once per pair it skips (`same_token_pair`, `arrived_outside_slot_window`), and once per held entry it drops as the slot moves on (`left_slot_window`). `left_slot_window` moves all the time, as the slot advances; `arrived_outside_slot_window` and `buffer_full` should sit near zero. Never divide these by the match total: they count storage, not matches.
* `price_level_stream_override_entries_refused_total{reason}` — one increment per venue entry the stream refused whole: `undecodable`, `not_a_venue`, `not_an_account`, `malformed_pair`, `malformed_storage`, `malformed_quantity`, `no_timestamp`, `pairs_and_sequences_differ`, `empty`, `unsupported_override` or `over_cap`. **`empty` runs at about one a second permanently**, because Titan sends entries that name no pair and no account; exclude it from alerts, and alert separately on a large change in its rate.
* `price_level_stream_unreadable_quote_sequence_total` — one increment per served book whose `calculated_from` the parser cannot read. A book naming none does not count, so a change in Titan's format stands out. It counts books, where the match counter counts pairs.
* `price_level_stream_sequence_slot_mismatch_total{socket}` — one increment per quote sequence whose high 64 bits are not the slot it should carry: a book's, against its price level message's slot, on `price_levels`; an entry's pair, against the entry's own slot, on `quote_stream`. Matching relies on that encoding, so this should stay at zero.
* `price_level_stream_override_block_mismatch_total` — one increment per state whose overrides name a block other than its message's, or name no block. This should stay at zero too; it checks the block you simulate at, where `price_level_stream_sequence_slot_mismatch_total` checks the encoding. The stream still attaches those overrides.
* `price_level_stream_override_merged_sources_total{venue}` — the number of other quote addresses' entries merged into the venue's states' overrides. Only Metric moves it today.
* `price_level_stream_override_skipped_sources_total{venue}` — for each state that gets overrides, the number of other quote addresses that held entries for its pair in the book's slot but none at or below the book's quote sequence, so the merge took nothing from them. A quote address that numbers its quote sequences on another counter than the venue's books moves it whenever it holds an entry for the pair. A quote address on the venue's counter moves it too, when all its entries in the slot are newer than the book. In a replay of the 2026-10-21 recording, `0x6dca2500…` moved it on 661 of Metric's 6,198 states with overrides, and `0x30a88bf8…` on 7.
* `price_level_stream_override_entry_missing_field_total{field}` — one increment per accepted entry missing `block_number` or `slot`, each of which you need to simulate a quote. Titan sent both on every entry the 2026-10-21 recording holds.
* `price_level_stream_unserved_venue_entries_total` — one increment per accepted entry streamed under a quote address the stream does not serve. The stream still stores it under any served venue its `pricedAddresses` name, as it does Metric's second quote address's entries.
* `price_level_stream_override_entry_accounts` and `price_level_stream_override_entry_storage_slots` — histograms of the accounts and storage words each accepted entry overrides, so growth toward the per-entry caps of 32 and 512 shows early. The 2026-10-21 recording peaked at 9 accounts and 16 storage words.

**The `socket` label changes two series.** Before October 2026, `price_level_stream_frames_rejected_total` and `price_level_stream_reconnects_total` carried `reason` alone. When you deploy a release that has the label, each old series goes stale and a labelled one starts from zero. That is harmless for `rate` and `increase`, but a recording rule, alert or `sum without(...)` keyed on the exact label set sees both series for the length of your staleness window.
