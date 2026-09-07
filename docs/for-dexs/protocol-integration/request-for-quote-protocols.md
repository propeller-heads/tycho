# Request for Quote Protocols

To add support for a new RFQ provider in Tycho, you’ll need to implement a feed, a client, a state, and the logic to encode and execute trades.&#x20;

The state, encoding, and execution logic for RFQs follow the same structure as on-chain protocol integrations. See our [simulation](simulation/) and [execution](execution/) guides for details.

We recommend using the existing Bebop integration as a reference.

### SnapshotFeed

Each RFQ provider needs a feed type (e.g. `BebopFeed`) implementing the generic `SnapshotFeed` trait (`tycho_simulation::snapshot_feed`):

```rust
pub trait SnapshotFeed {
    type Snapshot;
    type Output;

    fn subscribe(self) -> (watch::Receiver<Self::Snapshot>, impl Future<Output = Self::Output> + Send + 'static);
}
```

`subscribe` consumes the feed and returns a watch receiver holding the provider's latest complete book, plus the feed future that keeps it fresh — the caller spawns it. The RFQ feeds all use `type Snapshot = Option<BookSnapshot<ReceivedAt>>` (a `BookSnapshot` is the provider's complete set of `Book`s — one ready-to-simulate component and state per pair, with the provider's own `updated_at` where it reports one; `None` until the first snapshot arrives, or after the last one was withdrawn: after `max_missed_polls` failed polls for an HTTP feed, after `max_book_age` without a book for a WebSocket feed, and whenever the feed ends) and `type Output = Result<(), FeedError>` (the future resolves `Ok` when every receiver is dropped, `Err` when the feed gives up). The shared feed loops in `book/feed_loops.rs` (`run_ws_feed`, `run_http_poll_feed`) implement the connection handling, failure counting, and backoff; your feed supplies the fetch/decode logic that turns provider messages into the book.

Expose the provider identifier stamped on emitted components (e.g. `rfq:bebop`) as a `PROTOCOL_SYSTEM` constant in the provider's module (`rfq::protocols::bebop::PROTOCOL_SYSTEM`), so consumers can label the feed.

Binding quotes are not part of the trait: move quoting into a dedicated client struct that the emitted states share via `Arc`, and expose it through `IndicativelyPriced` (see the Bebop client).

Construct the feed through a builder in the same file that takes the shared `CommonConfig` by value plus provider credentials, similar to `BebopFeedBuilder`; provider-specific options are builder setters, the common ones come only from the config.

### State

Each provider must define a state object that represents a full snapshot of their indicative prices.

This state must implement:

* `ProtocolSim` for simulation
* `IndicativelyPriced` to request binding quotes at encoding time via the embedded client

Details on how to implement `ProtocolSim` can be found [here](simulation/#native-integration).

### Encoder + Executor

To support execution, implement:

* **Encoder**: Encodes the calldata to execute a swap on the RFQ via the Tycho Router. Be sure to request the binding quote here.&#x20;
* **Executor**: Executes the swap

For more see [here](execution/).

This allows the RFQ to be used in hybrid routes and benefit from Tycho’s execution optimizations.
