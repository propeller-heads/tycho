# Off-Chain Priced Venues

Integrate here if your venue prices off the chain: instead of pool state Tycho reads from the chain, you publish a complete **book** — the price levels you will trade at — over a WebSocket or an API. Two kinds share that shape: RFQ market makers, who price indicatively and sign a binding quote at execution (Bebop, Hashflow, Liquorice, Native), and off-chain-priced pAMMs, whose book prices a pool a taker executes against directly, with nothing to sign (Metric). Tycho stamps your components `rfq:<venue>`.

You implement a feed, a client, a state, and the logic to encode and execute trades.

The state, encoding, and execution logic follow the same structure as on-chain protocol integrations. See our [simulation](simulation/) and [execution](execution/) guides for details.

We recommend using the existing Bebop integration as a reference for an RFQ venue, and Metric for a pAMM.

### SnapshotFeed

Each venue needs a feed type (e.g. `BebopFeed`) implementing the generic `SnapshotFeed` trait (`tycho_simulation::snapshot_feed`):

```rust
pub trait SnapshotFeed {
    type Snapshot: Send + Sync + 'static;
    type Error: Send + 'static;

    fn run(self, publisher: Publisher<Self::Snapshot>) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;
}
```

`run` consumes the feed and publishes every complete book set through the `Publisher` the consumer hands it, until it gives up (`Err`) or has nothing left to publish, or nobody left to publish to (`Ok`). One method does it: `publisher.publishing(max_snapshot_age, books)` takes your stream of book sets and the age at which an unrefreshed one stops being servable, publishes each set, withdraws a stale one — consumers then stop using the venue's data until it publishes again — resolves with the error you gave up on, and stops asking your stream for book sets once the last reader is gone.

You cannot construct a `Publisher`, and that is the point: `snapshot_feed::SnapshotFeedStream::spawn(provider, feed)` makes one, spawns your feed in its own task and reads it as a stream of events (`book::BookFeedStreams` does that for several feeds at once, and `book::BookFeedWatch` for a consumer that prices on demand). The feed task therefore polls your socket at its own pace rather than the consumer's — a consumer that falls behind skips snapshots instead of stalling your connection — and you never have to notice that nobody is reading: `publishing` stops on its own, the runtime drops your task either way, and dropping the publisher withdraws whatever you were serving.

**Your impl is one line of body.** Set `Snapshot = BookSnapshot<ReceivedAt>` — a `BookSnapshot` is the provider's complete set of `Book`s, one ready-to-simulate component and state per book the venue publishes, which is per pair unless it names the maker behind a quote, and then per maker and pair — and `Error = FeedError`, then have `run` call one of the two shared loops (`snapshot_feed::ws::run_ws_feed`, `snapshot_feed::http::run_http_poll_feed`) with your feed's config, the publisher and your source. A `snapshot_feed` span records everything your feed logs and names the provider the consumer reads it under, so a warning always says which venue produced it — you declare that name nowhere.

**What you supply is the venue half, split in two.** `client.rs` is everything that talks to the venue: its endpoints, its credentials, one pooled `reqwest::Client`, and every request made to it — the book poll or WebSocket handshake, and the binding quote if your venue signs one. `source.rs` is what to make of the answers: decoding, orientation, TVL measurement, building components and states. A source never holds a credential or assembles a request; it asks its client.

The loops own connection handling, failure counting, backoff and stale-book withdrawal; you implement `WsSource` (hand back your client's handshake, decode a frame into a book set) or `HttpSource` (fetch one complete book set), returning `BookSnapshot::received_now(books)`, and the loop drives it. Nothing in the loops names a venue.

Classify failures with `FeedError`, and reserve `Fatal` for what no attempt can answer differently — an unsupported chain, a request that cannot be built — because it ends the feed whatever the failure budget says. A credential the venue refuses is not fatal: you fix it at the venue while the feed keeps trying, and the attempt that follows serves the book again. Everything else is `Connection` or `Parsing`, and the variant your source picks is the one the feed reports if it eventually gives up.

Expose the provider identifier stamped on emitted components (e.g. `rfq:bebop`) as a `PROTOCOL_SYSTEM` constant in the provider's module (`rfq::protocols::bebop::PROTOCOL_SYSTEM`), so consumers can label the feed.

Binding quotes are not part of the trait. If your venue signs them, the request is another method on the same client, the emitted states share that client via `Arc`, and they expose it through `IndicativelyPriced` (see the Bebop client). A pAMM whose book needs no quote implements neither, and its client is held by the feed alone (see the Metric client).

Construct the feed through a builder in the same file that takes the shared `BookFeedConfig` by value plus provider credentials, similar to `BebopFeedBuilder`; provider-specific options are builder setters, the common ones come only from the config.

### State

Each venue must define a state object that represents one pair's book.

This state must implement:

* `ProtocolSim` for simulation
* `IndicativelyPriced`, if the venue signs quotes, to request one at encoding time through the embedded client

[This page](simulation/#native-integration) explains how to implement `ProtocolSim`.

A venue that signs a limited number of quotes for one route can also publish a single component covering every pair it prices (`all_pairs()` on its feed builder). What that takes is a `PairState` impl on your per-pair state — the two tokens it prices and the directions it has levels on — and a `<Venue>AllPairsState` wrapper over `book::all_pairs::AllPairs`, which the `all_pairs_state!` macro declares for you along with its `ProtocolSim` and `IndicativelyPriced` impls. Give the macro your per-pair state and how your venue counts a route's turns: `SignedByMakers` where it names the market maker behind each book, `SignedByVenue` where it signs its own quotes — which is also what decides whether its builder takes a `QuoteRule`. The books behind it are the states your source already builds, one per pair and, where your venue names its makers, per maker; `book::all_pairs` picks among them, advertises the directions they report and holds a route to your venue's `QuoteRule`. It leaves out a state that reports no direction, so a book with nothing on it puts no untradeable token on the component either.

### Encoder + Executor

To support execution, implement:

* **Encoder**: Encodes the calldata to execute a swap on your venue via the Tycho Router. If the venue signs quotes, request the binding one here.&#x20;
* **Executor**: Executes the swap

For more see [here](execution/).

This allows the venue to be used in hybrid routes and benefit from Tycho’s execution optimizations.
