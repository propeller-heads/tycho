# PriceLevelStream freshness: stale components are removed, fresh ones re-added

Date: 2026-09-04, revised 2026-09-05 after a live characterization of the Titan stream,
revised 2026-09-07 after review
Status: design approved in interview, review adjustments applied, pending final approval
Owner: tycho-simulation, `crates/tycho-simulation/src/price_level_stream/`

## 1. Goal

Make the Titan pAMM price level stream fail closed on stale data. Whenever the stream can no
longer vouch for the freshness of a component's ladders, it emits that component in
`Update.removed_pairs`. When a fresh frame carries that component again, it re-emits it in
`Update.new_pairs`. Any consumer that already handles `removed_pairs` (Fynd does, end to end)
drops the component from its routing graph for the whole stale phase with no code change.

Invariant, adapted from the investigation reports to the component-scoped contract this design
implements:

> After the configured freshness threshold, zero returned routes may contain a component whose
> data the stream has not refreshed within that threshold, regardless of reconnect state,
> worker lag, fallback availability, process restart, or provider replay. A component recovers
> only when an accepted, fresh frame carries its unordered token pair again; reconnection alone
> never reactivates anything.

Recovery is per component, not per venue or per source. Titan frames are best-effort (section
2.2), so there is no source-level completeness marker to gate on. A frame that carries only one
direction of a pair re-adds the component with the other direction unquotable.

## 2. Background

### 2.1 The incident and the investigation

On 2026-09-01 the `vm:fermiswap` integration served an 11 hour old WETH/USDT price because its
substreams package was pinned to a retired engine and nothing invalidated the frozen state. The
protocol was removed from production and PropAMMs are now served only through this stream. The
follow-up investigation (`investigations/pricelevelstream-stability/reports/`, two independent
sessions that agree on every scenario) found the stream has no freshness boundary at any layer:

- The only removal signal is a set difference against a *newly arrived, parseable, accepted*
  frame. Silence, a dropped connection, a reconnect that never completes, keepalive-only or
  malformed traffic, a replayed frame, and a single absurd block number all leave the last ladders
  routable indefinitely.
- Titan stamps every frame with a nanosecond wall-clock `timestamp` and a beacon `slot`. The
  parser keeps only `blockNumber` and `pamms`.
- The PropAMMRouter whitelist read is once-only and fail-open. It failed four times in seven days
  in production, once leaving a pod on the unfloored `pricelevelstream:` family for about 37 hours.
- The removal and re-add plumbing below the stream (tracker diff, Fynd `MarketState`, worker
  graphs, lag rebuild) is proven correct at runtime. The entire gap is the trigger.

The stream is an in-process library inside `tycho-simulation`. It never passes through
tycho-indexer or tycho-client. The module is byte-identical from tycho 0.366.0 through current
`main`, so every finding applies to what is deployed and to what is being changed here.

### 2.2 What the wire actually does (live capture, 2026-09-05)

A 15 minute capture on the EU endpoint (900 frames, one connection, no drops), plus 90 s each on
the US and AP endpoints. Evidence under
`investigations/pricelevelstream-stability/reports/live-characterization-2026-09-05/`.

| Property | Observed |
|---|---|
| Cadence | 1 Hz, arrival gap min 0.986 s, max 1.014 s; 5 to 16 frames per block, median 12 |
| Frame age at arrival | p50 0.21 s, p95 0.59 s, max 2.8 s (EU); p50 0.6 s on AP |
| Timestamp order | never decreases; 5 of 899 consecutive pairs **equal**, with different content |
| Block and slot | never regress, advance in lockstep, delta only 0 or 1; Titan is at chain head +1 (82%) or +2 (18%) |
| **Frame completeness** | **only 92.3% of frames carry every venue**; 38 frames are strict subsets of the previous one; one venue was absent for 25 consecutive seconds while the other six streamed normally; absences do not cluster at block boundaries |
| Pair sets within a venue | vary independently: FermiSwap oscillated between 10 and 16 directed pairs, 52 transitions in 15 min, 43 mid-block |
| Ladder content | a quiet USDC/USDT venue sat byte-identical for up to 188 s; one FermiSwap direction for 473 s while the venue changed on 98% of frames |
| Keepalive | zero server pings, pongs, or close frames in 900 s |
| Schema | an undocumented per-venue `maker` integer; no `Interpolated` levels; zero empty ladders; some venues quote a direction without its reverse |
| Regions | same venues and cadence, but independently produced with unaligned timestamps |

Three conclusions shape this design:

1. **Absence from a frame is not retirement.** Frames are best-effort. Diffing consecutive frames
   would have produced about 75 remove and re-add events in 15 minutes.
2. **Content never signals staleness.** A "ladder unchanged for N seconds" rule would flag the
   quiet venue two thirds of the time. Freshness must come from when a component was last *seen*.
3. **The wire `timestamp` is the build round's clock, not a frame identity.** Equal timestamps
   are legitimate; strict monotonicity rejects real updates.

## 3. Scope

In scope:

- `crates/tycho-simulation/src/price_level_stream/{titan,stream,tracker,telemetry,state,
  fallback_router,mod}.rs`, `crates/tycho-simulation/Cargo.toml`, the workspace `Cargo.lock`,
  `crates/tycho-simulation/CLAUDE.md`.
- The freshness contract, the whitelist read policy, metrics, tests, module documentation.

Out of scope, tracked separately:

- Fynd changes: a `MarketState` age sweep, per-source health in `/v1/health`, an injectable
  stream seam for end-to-end tests, flooring quote-only and direct-family legs, and the
  consumer-contract test named in section 9.
- Serving PropAMM components over the Tycho WebSocket or RPC API to third parties.
- Detecting a maker whose ladders are frozen while Titan keeps including them in fresh frames
  (see section 11).
- Multi-region failover.
- Alerts and helm changes listed in section 10, which need no release.

## 4. Decisions taken

| Topic | Decision |
|---|---|
| Freshness clock | Titan's wire `timestamp` validated against the local wall clock; every deadline and every enforcement on the local monotonic clock |
| Freshness granularity | **Per component** (venue plus unordered token pair), anchored on the wire timestamp of the last frame that carried it |
| Removal triggers | Exactly two: a component's freshness deadline, and a whitelist family change. **No diff-based removal** |
| Removal threshold | `stale_after`, default 24 s (two block times). A frame whose data is `stale_after` old or older is rejected, so no emitted state is ever born due |
| Quote-time guard | A monotonic `quotable_until` on each state, one block time after the frame was built, enforced in the `ProtocolSim` methods; live-only, never serialized |
| Block frontier | Block regression and jump checks apply while something is served; the frontier resets whenever the source has nothing served, so one absurd block can never freeze the tracker for longer than `stale_after` |
| Signalling | `removed_pairs` only; no `sync_states`, no new `Update` field. Stale and retired are indistinguishable downstream; both mean do not route |
| Whitelist read | Each read bounded by a timeout, retried with backoff, nothing emitted until known, refreshed every 10 min, `RPC_URL` unset is a configuration error |
| Structure | One `async_stream` loop selecting frames, an earliest-deadline timer, and the whitelist reader; a synchronous tracker with injected time |
| Transport liveness | Idle timeout measured from the last parsed frame, default lowered to 10 s |
| Observability | `metrics` 0.24 facade in the crate, same major as Fynd and tycho-indexer, so consumers' recorders pick it up; every per-venue series pre-initialised for registered venues; no untrusted label values |
| Frozen maker inside fresh frames | Not detectable client-side; content-based rules ruled out by the capture. Question for Titan |
| Empty ladders | Unchanged: present and unquotable |

## 5. Design

### 5.1 Architecture

`PriceLevelStreamBuilder::build()` keeps its signature, `impl Stream<Item = Update> + Send`, and
its semantics: connect lazily on first poll, never terminate, close the socket and cancel the
whitelist reader on drop. Internally the bare `filter_map` over frames becomes an `async_stream`
loop that selects over:

1. **Frames** from `titan::messages`, forwarded to the tracker the instant they arrive. Nothing
   buffers or delays a frame.
2. **The stale deadline**, a `tokio::time::Sleep` armed at the earliest deadline among served
   components. Present only while something is served; the select arm is disabled otherwise.
3. **The whitelist reader**, a stream of read outcomes. Present only when the fallback router is
   enabled and a node URL is known.

The loop feeds typed events into a synchronous `SnapshotTracker`:

```rust
struct Now { wall_nanos: u64, instant: Instant }

impl SnapshotTracker {
    fn on_frame(&mut self, frame: TitanPriceLevelMessage, now: Now) -> Option<Update>;
    fn on_stale_deadline(&mut self, now: Now) -> Option<Update>;
    fn on_router_venues(&mut self, read: RouterVenuesRead) -> Option<Update>;
    fn stale_deadline(&self) -> Option<Instant>;
}
```

`wall_nanos` is used only to judge Titan's `timestamp`. `instant` drives every deadline, so a
wall-clock jump (NTP) can neither mass-expire nor freeze the tracker. Because time is injected,
the tracker's rules are unit-tested with plain values and no runtime.

### 5.2 Source state machine

```rust
enum Source {
    /// Fallback router enabled, whitelist not yet read. Frames are validated and counted,
    /// nothing is emitted: the family of every component is unknown.
    AwaitingWhitelist,
    /// Whitelist known (or router disabled), nothing served. At start and whenever the last
    /// served component expired. The block frontier is reset on entry.
    Unserved,
    /// At least one component is served. `deadline` is the earliest component deadline.
    Serving { deadline: Instant },
}
```

Transitions:

- `AwaitingWhitelist` → `Unserved` on the first successful whitelist read.
- `Unserved` → `Serving` on an accepted frame that yields at least one served pair.
- `Serving` → `Serving` on every accepted frame: components in the frame are refreshed, new ones
  added, `deadline` recomputed.
- `Serving` → `Serving` or `Unserved` when the deadline fires: due components are removed; if
  any remain the deadline is re-armed, otherwise the source is unserved.

Entering `Unserved` resets `newest_block` and `last_accepted` (never `newest_timestamp`), so the
next accepted frame is judged as a first frame for the block checks. This is what bounds a
poisoned first block to `stale_after` (section 5.3).

Re-add is the existing path: a component that is not currently served is built and put in
`new_pairs` by the next accepted frame that carries it. There is no hysteresis. Expiry requires
`stale_after` without that component being seen, so the worst-case flap period is about
`stale_after` plus one frame.

A transport disconnect on its own removes nothing. The 7 s Titan resets observed in production
stay invisible; a longer outage is caught by the deadlines. One mechanism, one threshold.

### 5.3 Frame acceptance

`TitanPriceLevelMessage` gains a required `timestamp: u64` (nanoseconds since the Unix epoch). A
frame missing it is a parse error. `slot` and the per-venue `maker` field stay unparsed; nothing
needs them.

Every parsed frame passes through these checks, in order. The first failure drops the frame and
increments `price_level_stream_frames_rejected_total{reason}`. A rejected frame changes nothing
in the tracker.

| # | Check | Rule | `reason` |
|---|---|---|---|
| 1 | Age | `frame_age = wall_now - timestamp` (saturating) must be `< stale_after` | `too_old` |
| 2 | Future | `timestamp <= wall_now + MAX_FUTURE_SKEW` (12 s) | `in_future` |
| 3 | Order | `timestamp >= newest_timestamp` (equal allowed) | `out_of_order` |
| 4 | Block regression | `block >= newest_block` | `block_regression` |
| 5 | Block jump | `block <= newest_block + slots_elapsed_since_last_accepted + 2` | `block_jump` |

The boundary is defined once: data exactly `stale_after` old is stale. Check 1 therefore rejects
at equality, and a state emitted from an accepted frame always has a strictly positive lifetime.

Checks 4 and 5 are skipped when `last_accepted` is `None`: for the first accepted frame of the
process and for the first frame after the source returns to `Unserved`. `newest_timestamp` only
ever grows and survives everything. Check 5's bound grows with the monotonic time since the last
accepted frame, so a legitimate long gap is accepted while a poisoned block number is rejected;
the constant `+2` covers Titan building at chain head plus two, which the capture saw on 18% of
frames.

**Poisoned first block.** A first frame with a fresh timestamp and an absurd block passes the
checks and becomes the frontier; every sane later frame then fails check 4. Its components are
still served with correct ladders, expire at their deadline, the source becomes `Unserved`, the
frontier resets, and the next sane frame is accepted. Recovery is automatic and bounded by
`stale_after`. The block number is metadata for `Update.block_number_or_timestamp`; the ladders
of such a frame are not wrong, so serving them for one window is acceptable. A two-frame
bootstrap that delays first service was considered and rejected as added state for a case the
capture never showed.

Equal timestamps are accepted because Titan re-emits within a build round with the same
timestamp and different content. Accepting a replayed frame is harmless: freshness is anchored on
the wire timestamp (section 5.4), so a replay cannot extend any deadline, and check 1 rejects it
once it is `stale_after` old.

A provider whose clock is far behind ours has every frame rejected as `too_old` and its
components expire. That is fail-closed and loud, by design.

Rejections are logged at WARN for the first frame of a rejection streak and at DEBUG for the
rest of the streak; an accepted frame ends the streak. The counter carries the rate.

### 5.4 Per-component freshness and the sweep

The tracker's served set is:

```rust
struct Served { component: ProtocolComponent, venue: String, address: Bytes, deadline: Instant }
served: HashMap<String, Served>   // keyed by component id
```

On an accepted frame with `frame_age`, for every served pair the frame carries:

- `deadline = now.instant + (stale_after - frame_age)`, i.e. the instant the *data* turns
  `stale_after` old, not the instant the frame is that old since receipt. Components carried by
  the same frame share a deadline, so a venue that drops out is removed in one Update.
- `quotable_until = now.instant + QUOTE_TTL.saturating_sub(frame_age)`, the instant the data
  turns one block time old (section 5.7). A frame accepted with `frame_age >= QUOTE_TTL` yields a
  state that is already unquotable but still served until its deadline.
- If the id is not in `served`, the component is built and placed in `new_pairs`.
- Its state is placed in `states`.

Components the frame does not carry are untouched: no state is emitted for them and their
deadline stands. Consumers keep the previous state; its quote guard makes it unquotable at one
block time.

When the deadline fires, every component with `deadline <= now.instant` is emitted in one
Update:

```rust
Update::new(newest_block, HashMap::new(), HashMap::new())
    .set_is_partial(true)
    .set_removed_pairs(due)
```

with one WARN naming the venues, the removed component ids, and the count (a sweep removes at
most the served set, a few dozen ids, so listing them is bounded), and
`price_level_stream_stale_removals_total{pamm}` incremented per component.
`block_number_or_timestamp` is the newest accepted block, so consumers that label state by
block see no regression and no jump.

**There is no diff-based removal.** A venue or pair absent from a frame is not removed by that
frame. This is the change the capture forced: absence is routine (7.7% of frames are partial,
pair sets oscillate mid-block) and does not mean retirement. A genuinely retired venue lingers at
most `stale_after`, unquotable after one block time.

### 5.5 Whitelist lifecycle

Applies when the fallback router is enabled, which is the default and what Fynd uses.

- The node URL is `fallback_router_rpc_url` if set, else `RPC_URL` from the environment or
  `.env`. Neither set is a configuration error: one ERROR log,
  `price_level_stream_source_state = 0`, and the stream stays in `AwaitingWhitelist` for its
  lifetime. It serves nothing rather than the wrong family.
- The whitelist reader calls `fetch_fallback_router_venues` under a `WHITELIST_READ_TIMEOUT`
  (15 s); a read that has not resolved by then is a failure like any other. On failure it retries
  forever with the existing `2^attempt` backoff capped at `max_backoff`. Each outcome is yielded
  to the loop as `RouterVenuesRead::Ok(set)` or `RouterVenuesRead::Failed(error)` and counted in
  `price_level_stream_whitelist_reads_total{outcome}`. The reader logs one WARN per failure; the
  tracker does not log failures again.
- After a success the reader sleeps `whitelist_refresh_interval` (default 10 min) and reads again.
- Frames and deadlines keep being processed while a read is pending; the reader is one arm of
  the loop, never awaited inline.
- `on_router_venues(Failed)`: keep the last known set (or keep waiting, before the first
  success). A stale whitelist is far safer than no pAMMs, and the counter makes it visible.
- `on_router_venues(Ok(set))` while `AwaitingWhitelist`: store the set, go `Unserved`.
- `on_router_venues(Ok(set))` while `Serving`, when a served venue's family changed: emit one
  Update whose `removed_pairs` holds that venue's served components, drop them from `served`,
  store the new whitelist. The next accepted frame that carries them re-adds them under the new
  family through the normal path, because `build_component` reads the current whitelist. If no
  served component remains, go `Unserved`.

Removal and re-add are always separate Updates. Fynd applies `remove_components` after
`upsert_components` within one Update, so an id present in both would vanish. This is an
invariant of the tracker: **an id never appears in both `removed_pairs` and `new_pairs` of the
same Update.**

`without_fallback_router()` is unchanged: no read, direct family, no waiting.

### 5.6 Transport liveness (`titan.rs`)

- The read-idle timeout is measured from the last *parsed* frame, not from any WebSocket
  message. Each `next()` waits at most `read_idle_timeout - time_since_last_parsed`; when that is
  zero the socket is reconnected with reason `idle_timeout`.
- The default `read_idle_timeout` drops from 30 s to **10 s**. Titan sends no keepalive traffic
  at all, so a half-open socket is indistinguishable from silence, and the longest gap seen in
  15 minutes was 1.014 s. At 10 s a dead socket reconnects and refreshes before any 24 s
  deadline fires, so a transient socket death costs no removal cycle.
- The reconnect backoff attempt counter resets only after a successful parse.
- Pings, pongs, binary frames, and unparsable text are logged as today but no longer extend
  liveness. Unparsable text increments `frames_rejected_total{reason="parse_error"}`.
- Every reconnect increments `price_level_stream_reconnects_total{reason}` with reason one of
  `idle_timeout`, `closed`, `read_error`, `ended`, `connect_failed`, `connect_timeout`.

The tracker's deadlines, not the idle timeout, remove components. The idle timeout only keeps a
half-open socket from wedging the transport.

### 5.7 Quote-time guard (`state.rs`)

`PriceLevelStreamState` gains `quotable_until: Option<Instant>`, marked `#[serde(skip)]`, so it
is a live, in-process property: it is never written to a recording and always deserializes as
`None`, which never expires. The tracker sets it as in section 5.4, from the frame's wire
timestamp translated to the monotonic clock at acceptance; `QUOTE_TTL` is one block time
(12 s), the same rule as `OverrideSnapshot::expires_at` in the override stream. It is **never**
derived from whether the ladder's content changed; the capture showed legitimate ladders
unchanged for minutes. Because it is monotonic, a future-skewed frame cannot stretch it (the
frame age saturates at zero, so the guard is at most `QUOTE_TTL` from acceptance) and a
wall-clock correction cannot move it.

A new `with_quotable_until(self, until: Instant) -> Self` sets it; `new` is unchanged.

`get_amount_out`, `spot_price`, and `get_limits` first call a private
`ensure_quotable(now: Instant)` with `Instant::now()` and return
`SimulationError::RecoverableError("price levels expired")` once `now >= quotable_until`.
`consumed()` propagates the field. `eq` includes it. This is defence in depth below the removal:
a component that a partial frame did not refresh, or a removal a consumer has not yet applied,
cannot quote a ladder older than one block.

Fynd maps a `RecoverableError` from `get_amount_out` to a refusal for that amount
(`fynd-core/src/algorithm/swap_cache.rs`, `Refusal::of`), so an expired state is skipped by the
solver with no Fynd change. Fynd's `Solver::from_recording` replays serialized `Update`s; since
the field is skipped, recordings made after this change replay exactly as before.

### 5.8 Behaviour kept, changed, and ignored

Kept:

- A pair with empty ladders on both sides stays present and unquotable.
- A pair present in only one direction gets an empty ladder for the other direction in the
  emitted state (fail closed in that direction). Some venues quote one direction structurally.
- Pairs whose tokens are absent from the boot token map are skipped.
- Auto-detect, deny-list, and registration precedence are unchanged.
- Component identity `pamm ++ token0 ++ token1` and `PAMM_ADDRESS_ATTRIBUTE` are unchanged.

Changed:

- An unregistered venue address is logged at INFO the first time it is seen, for at most 64
  distinct addresses per process (the set is bounded; beyond the cap nothing more is logged),
  and counted in the label-free `price_level_stream_unregistered_pamm_frames_total`. Addresses
  come from an external source and never become metric labels. A venue address migration
  therefore shows at production log level.
- Venue and pair omission no longer remove anything (section 5.4).

Ignored on purpose:

- The undocumented per-venue `maker` field and the level `variant` field.
- Regional endpoints other than the configured one. Regions are independent producers with
  unaligned timestamps; merging two into one tracker would reject most of the lagging region's
  frames. Failover between regions is out of scope.

### 5.9 Builder API

Three new knobs, following the existing setter pattern:

```rust
/// How long after a frame was built the data it carried may still be served (default: 24s,
/// two block times). A component not carried by any accepted frame within this window is
/// emitted in `removed_pairs`; the next accepted frame carrying it re-adds it. Frames whose
/// data is this old or older are rejected.
pub fn stale_after(mut self, duration: Duration) -> Self;

/// How often the PropAMMRouter whitelist is re-read (default: 10 minutes).
pub fn whitelist_refresh_interval(mut self, interval: Duration) -> Self;

/// The node URL the PropAMMRouter whitelist is read from, taking precedence over `RPC_URL`.
pub fn fallback_router_rpc_url(mut self, url: impl Into<String>) -> Self;
```

`fallback_router_rpc_url` exists for consumers that already hold a node URL as configuration
(Fynd passes `--rpc-url`) and should not depend on process environment for an execution-affecting
decision; it also gives tests a deterministic seam. `read_idle_timeout` keeps its setter; its
default changes to 10 s and its docstring explains why. Internal constants: `MAX_FUTURE_SKEW =
12 s`, `QUOTE_TTL = 12 s`, `WHITELIST_READ_TIMEOUT = 15 s`. The docstring of `stale_after` states
that the quote-time guard is fixed at one block time regardless of the setting.

### 5.10 Update shape and consumer contract

- First sight or re-add of a component: it is in `new_pairs` and `states`.
- Steady state: `states` holds every served component the frame carried, `new_pairs` the ones
  it did not serve before, `removed_pairs` empty. Components the frame did not carry are absent
  from the Update and keep their previous state downstream.
- Expiry: `states` and `new_pairs` empty, `removed_pairs` holds the due components.
- Family change: `states` and `new_pairs` empty, `removed_pairs` holds the venue's components.
- Always `is_partial = true`, `sync_states` empty, `block_number_or_timestamp` the newest
  accepted block.

Consumers cannot tell an expiry from a retired venue. Both mean the component must not be
routed until it reappears in `new_pairs`. The module docs state this explicitly.

Removals are emitted immediately, not queued to a block boundary. Verified against Fynd
(`fynd-core/src/feed/tycho_feed.rs`, `handle_tycho_message`): Fynd never reads `is_partial`, applies
every Update from the price-level arm identically, and its removal path is the one already
exercised by today's diff-based removals, which are also marked partial. Price-level Updates carry
no `Ready` sync header, so they cannot touch Fynd's block clock or health. Inside tycho-simulation
nothing filters on `is_partial`; the indexer's WebSocket partial-block gating applies only to the
indexer feed, which this stream never passes through. A consumer that dropped partial Updates
would already be dropping this entire stream, so immediate emission adds no new hazard.

### 5.11 Metrics

`metrics` 0.24 facade added to `tycho-simulation` under the `price-level-stream` feature, no
exporter. Consumers with a recorder installed (Fynd, tycho-integration-test) see them for free.
Every per-venue series is initialised to 0 for every registered venue when the tracker is
built, so a venue that never appears is visible as a zero, not as a missing series. Label
values are venue names from the registry or fixed enumerations; never an address from the wire,
except the venue address itself when a pAMM is served under auto-detection.

| Name | Type | Labels | Emitted by |
|---|---|---|---|
| `price_level_stream_frames_accepted_total` | counter | | tracker |
| `price_level_stream_frames_rejected_total` | counter | `reason`: parse_error, too_old, in_future, out_of_order, block_regression, block_jump | transport (parse_error), tracker (rest) |
| `price_level_stream_last_seen_timestamp_seconds` | gauge | `pamm` (registered venue name) | tracker, on every accepted frame carrying the venue; 0 at start |
| `price_level_stream_served_components` | gauge | `pamm` | tracker, after every change to the served set; 0 at start |
| `price_level_stream_stale_removals_total` | counter | `pamm` | tracker, per component removed by expiry |
| `price_level_stream_source_state` | gauge | 0 awaiting_whitelist, 1 unserved, 2 serving | tracker |
| `price_level_stream_reconnects_total` | counter | `reason` (section 5.6) | transport |
| `price_level_stream_whitelist_reads_total` | counter | `outcome`: ok, error | whitelist reader |
| `price_level_stream_whitelisted_venues` | gauge | | tracker |
| `price_level_stream_unregistered_pamm_frames_total` | counter | | tracker |

Alerts for operators:

- A registered venue not seen for two block times. The gauge starts at 0, so the expression is
  true from the first scrape and the `for` clause is what grants a new pod its grace period:

  ```yaml
  expr: time() - price_level_stream_last_seen_timestamp_seconds > 24
  for: 24s
  ```
- Partial, persistent coverage loss (one pair of a venue gone while the rest is served), as a
  drop against the venue's own recent baseline, since per-venue counts are stable in normal
  operation (the capture showed 2 to 12 per venue and oscillations recover within seconds):

  ```promql
  price_level_stream_served_components
    < 0.75 * max_over_time(price_level_stream_served_components[6h])
  ```

  held `for: 5m`, labelled by `pamm`. The sweep WARN lists the removed component ids, so the
  alert leads straight to the affected pair in the logs.
- Stream not serving, with the startup grace (whitelist read plus first frame) as an executable
  clause rather than prose:

  ```yaml
  expr: price_level_stream_source_state != 2
  for: 1m
  ```
- `stale_removals_total` is expected to tick occasionally in normal operation (the capture
  would have produced one removal in 15 minutes for the flakiest venue); a sustained rate is
  the signal.

Component-level series (`{pamm,pair}`) were considered and rejected: the per-venue count already
exposes a lost pair, and pair labels would multiply series for no additional alerting power.
Family is not a label here because Fynd's `market_pools_per_protocol` already carries the full
protocol system per component.

### 5.12 Logging

- INFO: connected, whitelist read with venue count, first frame served with component count,
  first sight of an unregistered venue (bounded, section 5.8), family change for a venue.
- WARN: the first rejected frame of a rejection streak with reason and the offending values,
  every reconnect with reason, every expiry with venues, removed component ids, and count,
  every failed whitelist read (from the reader only).
- DEBUG: subsequent rejections within a streak.
- ERROR: no node URL for the whitelist with the fallback router enabled.

## 6. Error handling summary

| Situation | Behaviour |
|---|---|
| A component not carried by any accepted frame for `stale_after` | Removed by expiry; re-added by the next accepted frame carrying it |
| Whole stream silent, disconnected, or keepalive-only | Every served component expires at its own deadline, all within `stale_after` of the last accepted frame |
| Partial frame omitting a venue or pair | Nothing removed; the omitted components keep their deadline and their quote guard |
| Frame `stale_after` old or older, in the future, strictly older than the newest, or with an implausible block | Rejected, counted, logged; tracker untouched |
| Equal-timestamp re-emission | Accepted; cannot extend any deadline |
| First frame with an absurd block | Served for at most `stale_after`; later sane frames rejected until expiry resets the frontier, then accepted |
| Unparsable text, binary, ping-only traffic | Does not count as liveness; idle timeout reconnects at 10 s |
| Whitelist read pending past 15 s | Counted as a failed read; frames and deadlines keep flowing meanwhile |
| Whitelist unreadable at start | Serve nothing, retry with backoff |
| Whitelist unreadable on refresh | Keep last known set, counter |
| No node URL with router enabled | ERROR, serve nothing |
| Venue moves between families | Removal now, re-add under new family on next frame carrying it |
| Consumer quotes a state past `quotable_until` | `RecoverableError("price levels expired")`; Fynd treats it as a refusal |

## 7. Testing

Three tiers, all under `cargo test -p tycho-simulation --lib price_level_stream`. No external
network: loop tests use loopback WebSocket servers.

**Tracker unit tests** (synchronous, injected `Now`, monotonic test clock):

- One test per rejection reason in section 5.3, including an older-timestamp frame; the
  boundary test accepts data `stale_after - 1 s` old and rejects data exactly `stale_after` old;
  every reason, including `out_of_order`, is asserted on the counter.
- An equal-timestamp frame with different content is accepted and its components refreshed.
- Block jump bound: a poisoned block after a sane first frame is rejected and later frames still
  process; a frame after a long gap with a proportionally larger block is accepted.
- Poisoned first block: served, sane frames rejected until expiry, then the frontier resets and
  the sane sequence is accepted; nothing from the poisoned frame survives.
- A frame omitting a venue removes nothing and emits no state for it; the venue's components
  expire at their own deadline, not earlier, and are re-added by the next frame carrying them.
- A frame omitting one pair of a venue removes nothing; that pair expires alone while the
  venue's other pairs stay served.
- A pair-set oscillation (16, 14, 16 directed pairs on consecutive frames) emits no removal.
- Components from different frames expire in separate sweeps at their own deadlines; components
  from one frame expire together in one Update.
- Deadline and `quotable_until` are shortened by the frame's age at acceptance; a replayed
  frame does not move either; a frame at maximum accepted future skew gets at most `QUOTE_TTL`.
- The sweep emits empty `states` and `new_pairs`, and the tracker reports the next earliest
  deadline or none.
- Recovery is component-scoped: after expiry, a frame carrying only a subset of a venue's prior
  pairs re-adds only that subset; a frame carrying one direction re-adds the pair with the other
  direction unquotable.
- No emission while `AwaitingWhitelist`; the first frame after the whitelist arrives emits the
  snapshot.
- A whitelist change moves a served venue: removal Update now, re-add under the new family on
  the next frame, never both in one Update.
- A failed refresh keeps the previous set.
- Per-venue gauges are initialised to 0 for registered venues and updated on serve and expiry.
- Every existing test that does not assert diff-based removal stays green
  (`repeated_snapshot_is_not_a_new_pair`, `unknown_tokens_are_skipped`, migration and
  coexistence tests). `vanished_pamm_has_its_pairs_removed` and `dropped_pair_is_removed` are
  rewritten as expiry tests. The two tests that encode the fail-open whitelist
  (`failed_whitelist_read_leaves_every_venue_on_the_direct_path`,
  `missing_rpc_url_leaves_every_venue_on_the_direct_path`) are deleted and replaced by the
  fail-closed tests below.

**Whitelist reader tests** (scripted fetch, no HTTP): failures retried with backoff, success
refreshed after the interval, a fetch that never resolves counted as a failed read after the
read timeout.

**Loop tests** (fake Titan WebSocket server on `127.0.0.1:0` via `tokio_tungstenite::accept_async`,
short real durations, the verifier's harness pattern):

- One frame then a silent open socket (later connections stay silent) yields the removal Update.
- One frame then repeated immediate server closes yields the removal Update.
- One frame then the listener shut down (true connection refused) yields the removal Update.
- One frame then replayed identical frames, ping-only traffic, and malformed text each yield a
  removal-only Update while the stream stays alive.
- A fresh frame after recovery re-adds everything as `new_pairs`.
- With the router enabled and no node URL (env lookup disabled through a test seam) the stream
  yields nothing and the state gauge is 0.
- With the router enabled and an unreachable node the stream yields nothing while frames flow.
- Frames are yielded without waiting on any timer.
- No connection is opened before the first poll; dropping the stream closes the socket.

**State tests**:

- `get_amount_out`, `spot_price`, `get_limits` succeed before `quotable_until` and return the
  expiry error from that instant; a state without it never expires; the state returned by a
  successful `get_amount_out` keeps it; serializing a state never writes it and deserializing
  yields `None`.

**Metrics tests** with `metrics-util`'s `DebuggingRecorder` (dev-dependency), asserting each
counter and gauge in section 5.11 moves on the corresponding event.

**Fixture**: the first raw frame of the 2026-09-05 capture is added next to the existing
captured frame under `test_responses/`, proving the parser reads `timestamp` and ignores the
undocumented `maker` field. Partial frames and equal-timestamp re-emissions are exercised with
synthetic frames in the tracker tests, since the capture stored only its first three frames
verbatim.

The verifier's five uncommitted probe tests are brought into the module; the three that assert
unsafe behaviour (`repeated_same_block_is_accepted_as_fresh_state`,
`reconnect_after_silence_emits_no_invalidation`,
`malformed_text_keeps_connection_live_without_new_updates`) are inverted.

Verification before merge: the tests above, then fmt and clippy on `nightly-2026-06-28`, the
exact toolchain CI pins (installed as the plan's first task and used for every lint command),
then `/run-ci` and `/sync-docs`.

## 8. Documentation

- Rewrite the `mod.rs` module docs and the `build()` docstring for the new contract: frames are
  best-effort snapshots, per-component freshness window, removal semantics, whitelist policy,
  the knobs, and the statement that an expiry and a retired venue look the same downstream.
- Update the `price_level_stream` paragraph in `crates/tycho-simulation/CLAUDE.md`.
- No GitBook page describes this stream today (`docs/for-solvers/` mentions RFQ price levels and
  `vm:bopamm` only). Writing one is part of the advertising decision and out of scope here.
- Commit messages use conventional prefixes (`feat(simulation):`, `fix(simulation):`) so the
  release changelog records the contract change.

## 9. Rollout

1. One PR to this repo implementing sections 5 through 8. Release. Bump Fynd's tycho dependency.
   Fynd needs no code change: the removal path is already exercised, expired states are refused
   by `Refusal::of`, and recordings never carry the quote guard. Its visible behaviour changes
   are that pAMM components appear only after the whitelist read succeeds, and that a component
   missing from the stream for 24 s is removed and later re-added.
2. Configure the alerts of section 5.11 against the canary, then canary on `ethereum-fynd-spot`
   for a day watching `price_level_stream_source_state`, `price_level_stream_served_components`,
   `price_level_stream_frames_rejected_total`, `price_level_stream_stale_removals_total`, and
   `propamm_fallback_quotes_total`. Then baseline.
3. Consumer-contract test in the Fynd repository, tracked as a follow-up and not a gate: add →
   update → removal → graph absence → re-add through `handle_tycho_message` and a worker graph,
   plus a quote attempt on an expired state being refused. Fynd's removal path is already
   tested (`test_handle_message_removes_components`) and its `Refusal::of` maps the expiry
   error to a refusal, both verified in source, so this test strengthens the cross-repo
   contract without blocking the baseline rollout.
4. Ask Titan the questions in section 11 in parallel; their answers gate advertising, not merging.

## 10. Containment that needs no release (ops, outside this repo)

From the reports' P0 list, unchanged by this design:

- Alert on `market_pools_per_protocol{protocol="pricelevelstream:fermiswap"} > 0` on any prod pod,
  and on `market_pools_per_protocol{protocol="propammfallback:fermiswap"} == 0` for over 5 min on a
  pod older than 2 min.
- Loki alerts on `No Titan message within idle timeout`, `Backing off before reconnecting to
  Titan`, `Skipping out-of-order price level frame`, `Failed to parse Titan price level message`,
  `Could not read the PropAMMRouter venue whitelist`, `RPC_URL is not set`.
- Add `"pricelevelstream:"` to `exclude_protocols` of `most_liquid_d3_no_pamm` in both prod
  Ethereum Fynd releases.
- Re-add `exclude:vm:fermiswap` once Fynd 0.100.1 or later is deployed.

## 11. Deferred and open

**Frozen maker inside fresh frames.** If Titan keeps carrying a venue whose ladders no longer
reflect the maker, nothing client-side can tell. The capture rules out content-based detection:
a quiet venue legitimately sits byte-identical for minutes, and a busy venue can be frozen in one
direction for eight minutes. Only Titan can signal this. Questions to put to Titan:

1. What is published for a venue whose maker has stalled: is it dropped from frames, kept with
   its last ladders, or marked?
2. Are partial frames (a venue absent for up to 25 s while others stream) expected behaviour,
   and what does absence mean?
3. Is there, or could there be, a keepalive or per-venue freshness field?

**Also deferred:** Fynd defence in depth (section 3), an indexer API for PropAMMs, parsing
`slot`, removing empty-ladder pairs, client-side WebSocket pings (unnecessary while Titan
streams at 1 Hz and the idle timeout is 10 s), multi-region failover, and simplifying the
integration-test processor's own silence watchdog, which becomes redundant once the stream
expires on its own.

## 12. Acceptance criteria

1. With one accepted frame and then any of: silence, disconnect, refused reconnects, ping-only
   traffic, malformed text, or replayed frames, the stream yields an Update removing every
   served component at the instant the last accepted frame's data turns `stale_after` old.
2. A component absent from accepted frames for `stale_after` is removed alone, while components
   the frames keep carrying stay served.
3. A frame omitting a venue or a pair emits no removal.
4. An accepted frame carrying a component that is not served re-adds it in `new_pairs` and
   `states`; a frame carrying only a subset re-adds only that subset; reconnection alone re-adds
   nothing.
5. No Update ever carries the same id in `removed_pairs` and `new_pairs`.
6. A frame whose data is `stale_after` old or older, in the future beyond 12 s, strictly older
   than the newest accepted one, or with a block that regresses or jumps beyond the bound is
   rejected without affecting served state, and is counted by reason. An equal-timestamp frame
   is accepted.
7. A poisoned block number, first frame or not, cannot prevent later sane frames from being
   accepted for longer than `stale_after`.
8. With the fallback router enabled the stream emits nothing until the whitelist has been read;
   a pending read never blocks frames or deadlines and is failed after the read timeout; a later
   whitelist change moves affected venues between families through a removal followed by a
   re-add.
9. Every `ProtocolSim` method on a state past `quotable_until` returns the expiry error; the
   guard never exceeds one block time from acceptance regardless of the frame's timestamp skew;
   the guard is never serialized.
10. Every metric in section 5.11 is emitted and covered by a test; no metric label carries a
    value from the wire.
11. All existing `price_level_stream` tests, rewritten or inverted where noted, and the workspace
    CI pass.
