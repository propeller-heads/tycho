# Tycho Client

Tycho Client is the main consumer-facing component of the Tycho indexing system. This guide covers the CLI application, although all functionality is also available as a Rust library.

## Installation

You can download the correct binary from the [latest release](https://github.com/propeller-heads/tycho-indexer/releases) on GitHub.

Once you have downloaded the binary, follow these steps to install it:

1. Unpack the tarball:
```bash
tar -xvzf tycho-client-aarch64-apple-darwin-{VERSION}.tar.gz
```
2. Bypass macOS quarantine (macOS users only):
```bash
xattr -d com.apple.quarantine tycho-client || true
```
3. Move the binary to a directory in your PATH:
```bash
mv tycho-client /usr/local/bin/
```

### Verify Installation

After installing, you can verify that the client is available from your command line:

```bash
tycho-client -V
```

This should display the version of the Tycho client installed.

## Quickstart

### Client CLI Binary

To get started with the Tycho Client, you'll first need access to a Tycho Indexer. This can be achieved by either running your own Tycho instance or connecting to a [hosted endpoint](#hosted-endpoints). Once connected, you can create a data stream using the following command:

```bash
tycho-client \
    --exchange uniswap_v2 \
    --exchange uniswap_v3 \
    --exchange vm:ambient \
    --exchange vm:balancer \
    --min-tvl 100 \
    --tycho-url {TYCHO_INDEXER_URL}
```

Note: If not specified, `TYCHO_INDEXER_URL` defaults to *localhost:4242*.

#### Authentication

If your Tycho Indexer requires authentication, you can provide your token in one of two ways:

Environment Variable:

```bash
export TYCHO_AUTH_TOKEN={your_token}
```

Command-Line Flag:

```bash
tycho-client --auth-key {your_token}
```

For setups that do not require secure connections (e.g., self-hosted Tycho instances), you can skip setting an auth key and use the --no-tls flag to disable TLS encryption.

#### Help and More Information

To explore more options and details about the CLI, run:

```bash
tycho-client --help
```

## Rust Builder

You can also integrate Tycho Client directly in your Rust projects using the rust stream builder:

```rust
use tycho_core::dto::Chain;
use tycho_client::{stream::TychoStreamBuilder, feed::component_tracker::ComponentFilter};

let receiver = TychoStreamBuilder::new("localhost:4242", Chain::Ethereum)
    .auth_key(Some("my_api_key".into()))
    .exchange("uniswap_v2", ComponentFilter::with_tvl_range(10.0, 15.0))
    .exchange(
        "uniswap_v3",
        ComponentFilter::Ids(vec![
            "0xCBCdF9626bC03E24f779434178A73a0B4bad62eD".to_string(),
            "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640".to_string(),
        ]),
    )
    .build()
    .await
    .expect("Failed to build tycho stream");
```

## Usage

The main use case of the Tycho Client is to provide a stream of protocol components,
snapshots, their state changes and associated tokens.

If you choose to stream from multiple extractors, the client will try to align the
messages by their block. You can use the `--block-time` and `--timeout` parameters to fine tune this behaviour. This is the maximum time we will wait for another extractor before emitting a message. If any other extractor has not replied within this time it is considered as delayed. If an extractor is marked as delayed for too long, it is considered stale and the client will exit with an error message.

Note: *We do currently not provide support to stream from different chains.*

### State tracking

Tycho Client provides automatic state tracking. This is done using two core models: snapshots and deltas.

The client will first query Tycho Indexer for all components that match the filter
criteria. Once received, a full snapshot of the state of these components is requested, which the client will forward to the user. Thereafter, the client will collect and forward state changes (deltas) for all tracked components.

The delta messages will also include any new tokens that the client consumer has not
seen yet as well as a map of components that should be removed because the client
stopped tracking them.

#### Component Filtering

You can request individual pools, or use a minimum TVL threshold to filter the components. If you choose minimum TVL tracking, tycho-client will automatically add snapshots for any components that exceed the TVL threshold, e.g. because more liquidity was provided. It will also notify you and remove any components that fall below the TVL threshold. Note that the TVL values are estimates intended solely for filtering the most relevant components.

With state tracking enabled, components whose `paused` state attribute has reason `0x01`
(Substreams) are excluded. DCI reasons `0x02` (tracing) and `0x03` (metadata), and unknown
reasons, neither trigger removal nor clear an already observed Substreams pause. A pause removes a tracked component via `removed_components`
and cancels pending snapshots. An explicit zero value or deletion of `paused` makes the
component eligible for a fresh snapshot request. It is admitted only after checking the
configured filters, including the TVL returned with that snapshot. Omitting `paused`
from a sparse delta leaves its status unchanged, while a missing or zero-valued attribute
in a full snapshot means the component is not paused.

##### To track a single pool:

```bash
tycho-client --exchange uniswap_v3-0x....
```

This will stream all relevant messages for this particular uniswap v3 pool.

##### To filter by TVL:

If you wish to track all pools with a minimum TVL (denominated in the chain's native token), you have 2 options:

  1) Set an exact tvl boundary:
```bash
tycho-client --min-tvl 100 --exchange uniswap_v3 --exchange uniswap_v2
```
This will stream updates for all components whose TVL exceeds the minimum threshold set. Note: if a pool fluctuates in tvl close to this boundary the client will emit a message to add/remove that pool every time it crosses that boundary. To mitigate this please use the ranged tvl boundary decribed below.

  2) Set a ranged TVL boundary:
```bash
tycho-client --remove-tvl-threshold 95 --add-tvl-threshold 100 --exchange uniswap_v3
```

This will stream state updates for all components whose TVL exceeds the add-tvl-threshold. It will continue to track already added components if they drop below the add-tvl-threshold, only emitting a message to remove them if they drop below remove-tvl-threshold.

### Message types

For each block, the tycho-client will emit a FeedMessage. Each message is emitted as a single JSON line to stdout.

#### FeedMessage

The main outer message type. It contains both the individual SynchronizerState (one per extractor) and the StateSyncMessage (also one per 
extractor). Each extractor is supposed to emit one message per block (even if no changes happened in that block) and metadata about the extractors 
block synchronization state. The latter allows consumers to handle delayed extractors gracefully. 

[Link to structs](https://github.com/propeller-heads/tycho-indexer/blob/main/tycho-client/src/feed/mod.rs#L305)

#### SynchronizerState

This struct contains metadata about the extractors block synchronization state. It
allows consumers to handle delayed extractors gracefully. Extractors can have any of the following states:

- `Ready`: the extractor is in sync with the expected block
- `Advanced`: the extractor is ahead of the expected block
- `Delayed`: the extractor has fallen behind on recent blocks, but is still active and trying to catch up
- `Stale`: the extractor has made no progress for a significant amount of time and is flagged to be deactivated
- `Ended`: the synchronizer has ended, usually due to a termination or an error

[Link to structs](https://github.com/propeller-heads/tycho-indexer/blob/main/tycho-client/src/feed/mod.rs#L106)

#### StateSyncMessage

This struct, as the name states, serves to synchronize the state of any consumer to be up-to-date with the blockchain.

The attributes of this struct include the header (block information), snapshots, deltas and removed components. 

 - *Snapshots* are provided for newly tracked components, including components that resume tracking after removal. A snapshot contains the entire state at the header.
 - *Deltas* contain state updates, observed after or at the snapshot. Any components
mentioned in the snapshots and in deltas within the same StateSynchronization message,
must have the deltas applied to their snapshot to arrive at a correct state for the
current header.
- *Removed components* is a map of components that should be removed by consumers. These components stop receiving updates until they qualify for tracking again, at which point they return with a fresh snapshot.

[Link to structs](https://github.com/propeller-heads/tycho-indexer/blob/main/tycho-client/src/feed/synchronizer.rs#L80)

#### Snapshots

Snapshots are simple messages that contain the complete state of a component (ComponentWithState) along with the related contract data (ResponseAccount). Contract data is only emitted for protocols that require vm simulations, it is omitted for more simple protocols such as uniswap v2 etc.

Note: for related tokens, only their addresses are emitted with the component snapshots. If you require more token information you may utilise the tycho rpc.

[Link to structs](https://github.com/propeller-heads/tycho-indexer/blob/main/tycho-client/src/feed/synchronizer.rs#L63)

##### ComponentWithState

Tycho differentiates between *component* and *component state*.

The *component* itself is static: it describes, for example, which tokens are involved or how much fees are charged (if this value is static).

The *component state* is dynamic: it contains attributes that can change at any block, such as reserves, balances, etc.

[Link to structs](https://github.com/propeller-heads/tycho-indexer/blob/main/tycho-client/src/feed/synchronizer.rs#L57)

##### ResponseAccount

This contains all contract data needed to perform simulations. This includes the contract address, code, storage slots, native balance, etc.

[Link to structs](https://github.com/propeller-heads/tycho-indexer/blob/main/tycho-common/src/dto.rs#L569)

#### Deltas

Deltas contain only targeted changes to the component state. They are designed to be
lightweight and always contain absolute new values. They will never contain delta values so that clients have an easy time updating their internal state.

Deltas include the following few special attributes:

- `state_updates`: Includes attribute changes, given as a component to state key-value mapping, with keys being strings and values being bytes. The attributes provided are protocol-specific. Tycho occasionally makes use of reserved attributes, see [here](https://docs.propellerheads.xyz/integrations/indexing/reserved-attributes) for more details.
- `account_updates`: Includes contract storage changes given as a contract storage key-value mapping for each involved contract address. Here both keys and values are bytes.
- `new_protocol_components`: Components that were created on this block. Must not necessarily pass the tvl filter to appear here.
- `deleted_protocol_components`: Any components mentioned here have been removed from
  the protocol and are not available anymore.
- `new_tokens`: Token metadata of all newly created components.
- `component_balances`: Balances changes are emitted for every tracked protocol component.
- `component_tvl`: If there was a balance change in a tracked component, the new tvl for the component is emitted.

Note: exact byte encoding might differ depending on the protocol, but as a general guideline integers are big-endian encoded.

[Link to structs](https://github.com/propeller-heads/tycho-indexer/blob/main/tycho-common/src/dto.rs#L215)

## Debugging

Since all messages are sent directly to stdout in a single line, logs are saved to a
file: `./logs/dev_logs.log`. You can configure the directory with the `--log-dir` option.

### Tail Logs in Real Time:

```bash
tail -f ./logs/dev_logs.log
```

### Modify Log Verbosity:

```bash
RUST_LOG=tycho_client=trace tycho-client ...
```

### Improve Message Readability

To get a pretty printed representation of all messages emitted by tycho-client you can
stream the messages into a formatter tool such as `jq`:

```bash
tycho-client --exchange uniswap_v3 ... | jq
```

### Stream Messages to a File

For debugging, it is often useful to stream everything into files, both logs and
messages, then use your own tools to browse those files.

```bash
RUST_LOG=tycho_client=debug tycho-client ... --log-dir /logs/run01.log > messages.jsonl &
# To view the logs
tail -f logs/run01.log
# To view latest emitted messages pretty printed
tail -f -n0 message.jsonl | jq 
# To view and browse the 3rd message pretty printed
sed '3q;d' message.jsonl | jq | less
```

To only stream a preset amount of messages you can use the `-n` flag. This is useful to create a single snapshot for test fixtures:

```bash
tycho-client -n 1 --exchange uniswap_v3:0x.... > debug_usv3.json
```

If you wish to create an integration test, you can also stream multiple messages into a
[jsonl](https://jsonlines.org/) file:

```bash
tycho-client -n 100 --exchange uniswap_v3:0x....  > integration.jsonl
# or with compression
tycho-client -n 1000 --exchange uniswap_v3:0x....  | gzip -c - > 1kblocks.jsonl.gz
```

This file can then be used as mock input to an integration test or a benchmark script.

### Tokens and Historical state

Tycho also provides access to recently historical states as well as all tokens supported by the protocols it has integrated. Both of which are accessible throught the Client RPC interfaces. 

Here is sample code to fetch tokens:

```rust
use tycho_client::rpc::HttpRPCClient;
use tycho_core::dto::Chain;

let client = HttpRPCClient::new("insert_tycho_url", Some("my_auth_token"));

let tokens = client
    .get_all_tokens(
        Chain::Ethereum,
        Some(51_i32), // min token quality
        Some(30_u64), // number of days since last traded
        1000, // pagination chunk size
    )
    .await
    .unwrap();
```

## Light mode

For use cases that do not require snapshots or state updates, we provide a light mode.
In this mode tycho will not emit any snapshots or state updates, it will only emit
newly created component, associated tokens, tvl changes, balance changes and removed
components. This mode can be turned on via the `--no-state` flag. Pause filtering is
unavailable in this mode because it requires component state.

# Hosted Endpoints

If you wish to use tycho as a service instead of hosting it yourself, the following endpoints are available:

### PropellerHeads (Beta)

|                        |                                                         |
|------------------------|---------------------------------------------------------|
| **URL**                | *tycho-beta.propellerheads.xyz*                         |
| **RPC Docs**           | *[View Documentation](https://tycho-beta.propellerheads.xyz/docs/)* |
| **Auth Key**           | *Please contact `@tanay_j` on Telegram to request a beta auth key. <br> Note: Use the `Authorization` header for this key in your RPC requests.* |
| **Supported Protocols** | *uniswap_v2, uniswap_v3, sushiswap, pancakeswap_v2, vm:balancer_v2, vm:curve*                    |
