# tycho-client

Consumer library implementing the snapshot + deltas pattern for real-time protocol state.

## Module Map

```
rpc.rs              HTTP snapshot client — fetches protocol state at a block height;
                    get_delta_catch_up fetches the messages a resubscribing client missed
                    (Unsupported on a 404 from servers without the endpoint)
deltas.rs           WebSocket client — streams real-time state deltas; subscribe_with_fingerprint
                    also returns the extractor fingerprint from NewSubscription (None from older
                    servers and from DeltasClient impls that keep the trait default)
stream.rs           Builder entry point — wires RPC + WS clients into a TychoStream.
                    build() eagerly loads and validates the chain registry before any network I/O
client_metadata.rs  X-Tycho-Client-Metadata header (CLIENT_METADATA_HEADER, size caps);
                    TychoStreamBuilder::add_client_metadata. Server half: indexer services/client_metadata.rs
feed/
  mod.rs            BlockSynchronizer — aligns N synchronizers by block, emits FeedMessage
  dto.rs            Consumer-facing feed types: ComponentWithState, Snapshot, StateSyncMessage, FeedMessage
  synchronizer.rs   ProtocolStateSynchronizer — manages snapshot + delta sync for one extractor
  component_tracker.rs  Filters components by TVL threshold or explicit ID list
  block_history.rs  Validates block chain continuity; classifies incoming blocks
cli.rs / main.rs    CLI binary for manual testing
```

## Connections

```
TychoStreamBuilder (stream.rs)
  └─ creates ProtocolStateSynchronizer per extractor (feed/synchronizer.rs)
       ├─ ComponentTracker (feed/component_tracker.rs) → HttpRPCClient (rpc.rs)
       ├─ WsDeltasClient (deltas.rs) for live deltas
       └─ StateSyncMessage → BlockSynchronizer (feed/mod.rs)
            ├─ BlockHistory (feed/block_history.rs) for chain validation
            └─ FeedMessage → consumer channel
```

## Sync Lifecycle

1. `WsDeltasClient` subscribes; first message determines snapshot block.
   On a restart, a first message that follows the last synced block (`is_next_expected`) resumes
   without a snapshot, but only if the subscription reports the fingerprint the client synced
   under (`may_resume`: `synced_fingerprint`, set with `last_synced_block` when a subscription's
   first message is emitted). An upgraded extractor may have emitted other deltas for blocks the
   client already applied, so a changed fingerprint, or one that only one of the two
   subscriptions reported, forces a snapshot. Two `None`s (servers that predate fingerprints)
   resume as before. Blocks at or below the last synced one are still skipped, and a snapshot the
   fingerprint forces waits for the snapshot delay below. A revert never resumes: it carries only
   the keys the reverted blocks changed, not its target block's own changes. Every message, the
   first included, goes through `handle_block`: TVL tracking, background snapshots for new
   components, DCI entry points, then filtering to the tracked components and contracts. With a
   snapshot, the first message is handled after the fetch succeeds and merged into the snapshot
   message.
   When the first candidate message `T` does not follow the last synced block, the synchronizer
   tries delta catch-up before the snapshot delay starts, if `may_resume` holds, the subscription
   reported a fingerprint and partial-block mode is off. `catch_up_path` asks
   `get_delta_catch_up` for the messages from the last synced block to `T`'s parent (to `T`'s own
   block when `T` is a revert), with the subscription's fingerprint and `include_state =
   include_snapshots`, bounded by the synchronizer's `timeout` but at least `MIN_CATCH_UP_TIMEOUT`
   (10 s; the RPC client waits 120 s before retrying a 502/503/504). A close signal ends the
   synchronizer while the request is in flight. `check_catch_up_path` verifies the whole answer
   before anything runs through `handle_block`: every message is of the subscription's extractor
   and chain; each block's parent is the block reached so far and its number is one higher; a
   revert moves back to a lower target block, which must be the block the walk passed at that
   height when it passed one (the last synced block included); and the walk ends at the target
   (so an empty path only when the last synced block is the target, and a path may end with a
   revert to it). `apply_catch_up` then runs each message, then `T`, through `handle_block` and
   merges the results into one message with `T`'s header, which leaves like any first message
   (setting `last_synced_block` and `synced_fingerprint`). Any failure is logged once with a
   `CatchUpSkipped` reason, at warn for an RPC error or an answer that is not a path to the
   target and at info otherwise, and takes the snapshot path, delay included.
   `feed/synchronizer/test/catch_up_properties.rs` checks resubscriptions over random histories
   against `catch_up_spec` and the model in `tycho-common`'s `testing::delta_history`.
   On a restart after a synced block that requests a snapshot (snapshots on and at least one
   tracked component) and cannot catch up, the synchronizer discards deltas for a random delay in
   `[0, snapshot_jitter]` (default `DEFAULT_SNAPSHOT_JITTER`, 60 s; at least
   `MIN_SNAPSHOT_JITTER`, 5 s) to spread snapshot requests across clients; the first block after
   the delay sets the snapshot.
2. `HttpRPCClient` fetches initial snapshot at that block synchronously; all subsequent new
   components are fetched via background tasks (`spawn_snapshot_task`) so the delta loop never
   blocks on RPC. Each component moves through `SnapshotStatus` (`Deferred` → `InFlight`
   → removed on success, or `RetryNext` / `Blacklisted` on failure). When `partial_blocks` is
   enabled, brand-new components are held in `Deferred` state until the first message of the next
   block, then promoted to `InFlight`.
3. `BlockSynchronizer` waits for all synchronizers, then emits a `FeedMessage` per block
4. Synchronizers classified as `Started | Ready | Delayed | Stale | Advanced | Ended`; stale ones are kept but skipped

## CLI

The `tycho-client` binary accepts `--blocklist-config <PATH>` pointing to a TOML file of the
form `ids = ["0x...", ...]`. Components in that list are excluded from tracking.
