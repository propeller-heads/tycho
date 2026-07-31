# tycho-client

Consumer library implementing the snapshot + deltas pattern for real-time protocol state.

## Module Map

```
rpc.rs              HTTP snapshot client — fetches protocol state at a block height
deltas.rs           WebSocket client — streams real-time state deltas
stream.rs           Builder entry point — wires RPC + WS clients into a TychoStream
feed/
  mod.rs            BlockSynchronizer — aligns N synchronizers by block, emits FeedMessage
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

1. `WsDeltasClient` subscribes; first message determines snapshot block
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
