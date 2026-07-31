# Tycho Client

Tycho Client helps you consume data from Tycho Indexer. It's the recommended way to connect to the Indexer data stream, whether you're using our [hosted endpoint](../../hosted-endpoints.md) or running your own instance.

In this guide, you'll learn more about the Tycho Client and the streamed data models.

{% hint style="success" %}
If you are developing in Rust and is using Tycho to simulate DeFi Protocol's behavior, we recommend checking out our [simulation.md](../../simulation.md "mention") package - this tool extends Tycho Client's data streaming functionality with powerful simulation capabilities.
{% endhint %}

{% hint style="info" %}
**✨ New: Sub-Second Latency with Partial Blocks**

Tycho now provides early support for partial blocks on Base, enabling sub-second latency by streaming pre-confirmation updates. Enable via `--partial-blocks` (CLI), `partial_blocks=True` (Python), or `.enable_partial_blocks()` (Rust). See [Streaming Options](./#streaming-options) below for details.
{% endhint %}

### Key Features

* **Real-Time Streaming**: Get low-latency updates to stay in sync with the latest protocol changes. Discover new pools as they’re created.
* **TVL Filtering**: Receive updates only for pools exceeding a specified TVL threshold (denominated in the Chain's Native Token).
* **Support for multiple protocols and chains**

### Available Clients

The client is written in Rust and available as:

* [rust-client.md](rust-client.md "mention")
* [binary-cli.md](binary-cli.md "mention")
* [python-client.md](python-client.md "mention")

Follow one of the guides above to learn how to set up the client appropriate for you.

We welcome community contributions to expand language support. See our contribution guidelines [how-to-contribute](../../../how-to-contribute/ "mention").

***

## Authentication

Interacting with the hosted Tycho Indexer requires a personalized API Key.

1. **Get a base key**: Message [@fynd_portal_bot](https://t.me/fynd_portal_bot) on Telegram to receive a Tycho Fynd key. This gives you access to [Tycho Fynd endpoints](../../hosted-endpoints.md#tycho-fynd).
2. **Need full Tycho Indexer access?** Once you have a base key, DM `@tanay_j` on Telegram to request an upgrade.

***

## Usage

Tycho Client provides a stream of protocol components, snapshots, their state changes, and associated tokens. For simplicity, we will use Tycho Client Binary as a reference, but the parameters described below are also available for our Rust and Python versions.

{% hint style="info" %}
Note: _While Tycho takes **chain** as a parameter, it is designed to support streaming from a single chain. If you want to consume data from multiple chains you will need to use more than one client connection._
{% endhint %}

### Component Filtering

You can request individual pools or use a minimum TVL threshold to filter the components. If you choose minimum TVL tracking, Tycho-client will automatically add snapshots for any components that exceed the TVL threshold, e.g., because more liquidity was provided. It will also notify you and remove any components that fall below the TVL threshold. Note that the TVL values are estimates intended solely for filtering the most relevant components.

**TVL Filtering:**

{% hint style="info" %}
Tycho indexes all the components in a Protocol. TVL filtering is highly encouraged to speed up data transfer and processing times by reducing the number of returned components.
{% endhint %}

**TVL is measured in the chain's native currency.** Since native tokens have different USD values, the CLI default (`--min-tvl`) is chain-dependent:

| Chain | Native Token | Default `--min-tvl` (~$20K USD) |
| ----- | ------------ | ------------------------------- |
| Ethereum, Arbitrum, Base, Unichain, Starknet, ZkSync | ETH | 10 |
| Polygon | POL | 200,000 |
| BSC | BNB | 32 |

You can filter by TVL in 2 ways:

1. **Set an exact TVL boundary**:

```bash
tycho-client --min-tvl 100 --exchange uniswap_v2
```

This will stream updates for all components whose TVL exceeds the minimum threshold set. Note: if a pool fluctuates in TVL close to this boundary, the client will emit a message to add/remove that pool every time it crosses that boundary. To mitigate this, please use the ranged tv boundary described below.

2. **Set a ranged TVL boundary (recommended)**:

```bash
tycho-client --remove-tvl-threshold 95 --add-tvl-threshold 100 --exchange uniswap_v3
```

This will stream state updates for all components whose TVL exceeds the `add-tvl-threshold`. It will continue to track already added components if they drop below the `add-tvl-threshold`, only emitting a message to remove them if they drop below `remove-tvl-threshold`.

#### Streaming Options

Tycho Client supports several options to customize the data stream. These are available as CLI flags, Rust builder methods, and Python parameters. Refer to each client's documentation for usage details.

| Option                  | Description                                        | When to use                     |
| ----------------------- | -------------------------------------------------- | ------------------------------- |
| **Partial blocks**      | Stream pre-confirmation blocks                     | For lower stream latency        |
| **No state**            | Stream only component metadata and tokens          | For lower stream latency        |
| **Include TVL**         | Attach approximate TVL estimates to each component | For getting components TVL      |
| **No TLS**              | Use unencrypted transports (http/ws)               | For local self-hosted indexers  |
| **Disable compression** | Turn off stream message compression                | _Debugging Only_                |

{% hint style="info" %}
**Note:** `disable compression` and `no tls` are not intended to be used with the [hosted](../../hosted-endpoints.md) Tycho Indexer endpoints.
{% endhint %}

<details>

<summary><strong>Details:</strong> <strong>Partial Blocks</strong></summary>

Some chains, such as <a href="https://docs.base.org/building-with-base/differences/flashblocks" target="_blank" rel="noopener noreferrer">Base</a>, support _flash blocks_ - pre-confirmation updates that contain parts of a future block before its construction is finished. When `partial blocks` is enabled, Tycho streams these incremental updates as they arrive, giving you sub-block latency. On chains without flash block support, enabling this flag is unsupported.

{% hint style="warning" %}
Block hashes in partial block messages are **unstable** — they change between partial updates and will differ from the final block hash. Do not use them as persistent identifiers or cache keys. See the <a href="https://docs.substreams.dev/reference-material/chain-support/flashblocks#developing-for-partial-blocks" target="_blank" rel="noopener noreferrer">Substreams documentation</a> for details.
{% endhint %}

</details>

<details>

<summary><strong>Details: No State</strong></summary>

By default, the first sync message includes full component snapshots, and every subsequent block includes state deltas (reserves, balances, contract storage). If you only need to discover which components exist and which tokens they involve, such as to build a pool registry or monitor new deployments, you can disable state monitoring with `no state`. This significantly reduces message sizes, startup time, and processing overhead, as both snapshots and per-block state updates are omitted entirely.

</details>

<details>

<summary><strong>Details: Include TVL</strong></summary>

When enabled, each message includes an approximate TVL estimate for every tracked component. This is useful for building dashboards or for ranking pools by liquidity. Note that enabling this option increases startup latency: for each snapshot request, the client makes additional RPC calls to fetch token prices and compute TVL for all tracked components. This overhead scales with the number of components you're tracking.

</details>

***

## Understanding Tycho Client Messages

Tycho emits data in an easy-to-read JSON format. Get granular updates on each block:

* **Snapshots** for complete component (or pool) states,
* **Deltas** for specific updates, and
* **Removal notices** for components that no longer match your filtration criteria.
* **Extractor status** for keeping track of the sync status of each extractor.

<details>

<summary>Block message example</summary>

```json
{
  "state_msgs": {
    "uniswap_v2": {
      "header": {
        "hash": "0x063a4837d7689df84c3b106be6ee1a31a65afb7122f9847bf566a3f97fdd6dd7",
        "number": 21926578,
        "parent_hash": "0xef792af9f9cc6036a4b7d8fb66879162e5b6edd30a6d4f1eec817be91bc950b1",
        "revert": false
      },
      "snapshots": {
        "states": {
          "0x21b8065d10f73ee2e260e5b47d3344d3ced7596e": {
            "state": {
              "component_id": "0x21b8065d10f73ee2e260e5b47d3344d3ced7596e",
              "attributes": {
                "reserve0": "0x019cd10cabe7a7916b2963a5",
                "reserve1": "0x064e2eb1ad62df7d3620"
              },
              "balances": {
                "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2": "0x064e2eb1ad62df7d3620",
                "0x66a0f676479cee1d7373f3dc2e2952778bff5bd6": "0x019cd10cabe7a7916b2963a5"
              }
            },
            "component": {
              "id": "0x21b8065d10f73ee2e260e5b47d3344d3ced7596e",
              "protocol_system": "uniswap_v2",
              "protocol_type_name": "uniswap_v2_pool",
              "chain": "ethereum",
              "tokens": [
                "0x66a0f676479cee1d7373f3dc2e2952778bff5bd6",
                "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
              ],
              "contract_ids": [],
              "static_attributes": {
                "pool_address": "0x21b8065d10f73ee2e260e5b47d3344d3ced7596e",
                "fee": "0x1e"
              },
              "change": "Creation",
              "creation_tx": "0xdd4b8bb7d2965ff7aa72e1c588fa0b57a69c83cad511fff0ae8356617c5e6fa3",
              "created_at": "2020-12-22T17:13:12"
            }
          },
          "0xa43fe16908251ee70ef74718545e4fe6c5ccec9f": {
            "state": {
              "component_id": "0xa43fe16908251ee70ef74718545e4fe6c5ccec9f",
              "attributes": {
                "reserve1": "0x01a43a590836b94fa2ba",
                "reserve0": "0x1d9b4fe1831a31d214d18686b4"
              },
              "balances": {
                "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2": "0x01a43a590836b94fa2ba",
                "0x6982508145454ce325ddbe47a25d4ec3d2311933": "0x1d9b4fe1831a31d214d18686b4"
              }
            },
            "component": {
              "id": "0xa43fe16908251ee70ef74718545e4fe6c5ccec9f",
              "protocol_system": "uniswap_v2",
              "protocol_type_name": "uniswap_v2_pool",
              "chain": "ethereum",
              "tokens": [
                "0x6982508145454ce325ddbe47a25d4ec3d2311933",
                "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
              ],
              "contract_ids": [],
              "static_attributes": {
                "pool_address": "0xa43fe16908251ee70ef74718545e4fe6c5ccec9f",
                "fee": "0x1e"
              },
              "change": "Creation",
              "creation_tx": "0x273894b35d8c30d32e1ffa22ee6aa320cc9f55f2adbba0583594ed47c031f6f6",
              "created_at": "2023-04-14T17:21:11"
            }
          }
        },
        "vm_storage": {}
      },
      "deltas": {
        "extractor": "uniswap_v2",
        "chain": "ethereum",
        "block": {
          "number": 21926578,
          "hash": "0x063a4837d7689df84c3b106be6ee1a31a65afb7122f9847bf566a3f97fdd6dd7",
          "parent_hash": "0xef792af9f9cc6036a4b7d8fb66879162e5b6edd30a6d4f1eec817be91bc950b1",
          "chain": "ethereum",
          "ts": "2025-02-25T23:18:59"
        },
        "finalized_block_height": 21926513,
        "revert": false,
        "new_tokens": {},
        "account_updates": {},
        "state_updates": {},
        "new_protocol_components": {},
        "deleted_protocol_components": {},
        "component_balances": {},
        "component_tvl": {}
      },
      "removed_components": {}
    }
  },
  "sync_states": {
    "uniswap_v2": {
      "status": "ready",
      "hash": "0x063a4837d7689df84c3b106be6ee1a31a65afb7122f9847bf566a3f97fdd6dd7",
      "number": 21926578,
      "parent_hash": "0xef792af9f9cc6036a4b7d8fb66879162e5b6edd30a6d4f1eec817be91bc950b1",
      "revert": false
    }
  }
}
```

</details>

Each message includes block details to help you stay on track with the latest block data.

**FeedMessage**

The main outer message type. It contains both the individual SynchronizerState (one per extractor) and the StateSyncMessage (also one per extractor). Each extractor is supposed to emit one message per block (even if no changes happened in that block) and metadata about the extractor's block synchronization state. The latter allows consumers to handle delayed extractors gracefully.

**SynchronizerState** (`sync_states`**`)`**

This struct contains metadata about the extractor's block synchronization state. It allows consumers to handle delayed extractors gracefully. Extractors can have any of the following states:

* `Ready`: the extractor is in sync with the expected block
* `Advanced`: the extractor is ahead of the expected block
* `Delayed`: the extractor has fallen behind on recent blocks but is still active and trying to catch up
* `Stale`: the extractor has made no progress for a significant amount of time and is flagged to be deactivated
* `Ended`: the synchronizer has ended, usually due to a termination or an error

**StateSyncMessage** (`state_msgs` )

This struct, as the name states, serves to synchronize the state of any consumer to be up-to-date with the blockchain.

The attributes of this struct include the header (block information), snapshots, deltas, and removed components.

* _Snapshots_ are provided for any components that have NOT been observed yet by the client. A snapshot contains the entire state at the header.
* _Deltas_ contain state updates observed after or at the snapshot. Any components mentioned in the snapshots and deltas within the same StateSynchronization message must have the deltas applied to their snapshot to arrive at a correct state for the current header.
* _Removed components_ is a map of components that should be removed by consumers. Any components mentioned here will not appear in any further messages/updates.

**Snapshots**

Snapshots are simple messages that contain the complete state of a component (ComponentWithState) along with the related contract data (ResponseAccount). Contract data is only emitted for protocols that require vm simulations, it is omitted for protocols implemented natively (like UniswapV2 - see the list of [supported-protocols.md](../../supported-protocols.md "mention")and how they're implemented).

Snapshots are only emitted **once** per protocol, upon the client's startup. All the state is updated later via deltas from the next block onwards.

{% hint style="info" %}
**Note**: for related tokens, only their addresses are emitted with the component snapshots. If you require more token information, you can request using [tycho-rpc.md](../tycho-rpc.md "mention")'s [#v1-tokens](../tycho-rpc.md#v1-tokens "mention")endpoint
{% endhint %}

**ComponentWithState**

Tycho differentiates between _component_ and _component state_.

The _component_ itself is static: it describes, for example, which tokens are involved or how much fees are charged (if this value is static).

The _component state_ is dynamic: it contains attributes that can change at any block, such as reserves, balances, etc.

**ResponseAccount**

This contains all contract data needed to perform simulations. This includes the contract address, code, storage slots, native balance, account balances, etc.

**Deltas**

Deltas contain only targeted changes to the component state. They are designed to be lightweight and always contain absolute new values. They will never contain delta values so that clients have an easy time updating their internal state.

Deltas include the following few special attributes:

* `state_updates`: Includes attribute changes, given as a component to state key-value mapping, with keys being strings and values being bytes. The attributes provided are protocol-specific. Tycho occasionally makes use of reserved attributes, see <a href="https://docs.propellerheads.xyz/integrations/indexing/reserved-attributes" target="_blank" rel="noopener noreferrer">here</a> for more details.
* `account_updates`: Includes contract storage changes given as a contract storage key-value mapping for each involved contract address. Here, both keys and values are bytes.
* `new_protocol_components`: Components that were created on this block. Must not necessarily pass the tvl filter to appear here.
* `deleted_protocol_components`: Any components mentioned here have been removed from the protocol and are not available anymore.
* `new_tokens`: Token metadata of all newly created components.
* `component_balances`: Balances changes are emitted for every tracked protocol component.
* `component_tvl`: If there was a balance change in a tracked component, the new tvl for the component is emitted.
* `account_balances`: For protocols that need the balance (both native and ERC-20) of accounts tracked for the simulation package (like BalancerV3 which needs the Vault balances), the updated balances are emitted.

Note: exact byte encoding might differ depending on the protocol, but as a general guideline integers are big-endian encoded.
