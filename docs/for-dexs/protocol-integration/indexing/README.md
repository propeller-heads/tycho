# Indexing

Our indexing integrations require a Substreams SPKG to transform raw blockchain data into structured data streams. These packages enable our indexing integrations to track protocol state changes with low latency.

### What is Substreams?

Substreams is a new indexing technology that uses Rust modules to process blockchain data. An SPKG file contains the Rust modules, protobuf definitions, and a manifest, and runs on the Substreams server.

Learn more:

* <a href="https://thegraph.com/docs/en/substreams/introduction/" target="_blank" rel="noopener noreferrer">Quick explanation</a>
* <a href="https://docs.substreams.dev/reference-material/substreams-components/packages" target="_blank" rel="noopener noreferrer">SPKGs</a>
* <a href="https://docs.substreams.dev/" target="_blank" rel="noopener noreferrer">Full documentation</a>

## **Integration Modes**

### VM Integration

VM integrations primarily track contract storage associated with the protocol’s behavior. Most integrations will likely use the VM method due to its relative simplicity, so this guide focuses on VM-based integrations.

It's important to know that simulations run in an empty VM, which is only loaded with the indexed contracts and storage. If your protocol calls external contracts during any simulation (swaps, price calculations, etc.), those contracts also have to be indexed. There are 2 approaches that can be used to index external contracts:

* Direct indexing on the substream package. This is where you index the external contract the same way you would index your own protocol's contract. A key limitation in Substreams to keep in mind is that you must witness a contract’s creation to access its full storage and index it.&#x20;
* Using the DCI (Dynamic Contract Indexer). To be used if your protocol calls external contracts whose creation event cannot be witnessed within the Substreams package - for example: oracles deployed long before the protocol's initial block, or when which contract is called can be changed during the protocol's lifetime. Use of the DCI introduces indexing latency and should only be used if necessary.

### Native Integration

Native integrations follow a similar approach, with one main difference: Instead of emitting changes in contract storage slots, they should emit values for all created and updated attributes relevant to the protocol’s behavior.

## Understanding the Data Model

The Tycho Indexer ingests all data versioned by block and transaction. This approach maintains a low-latency feed. And it correctly handles chains that undergo reorgs. Here are the key requirements for the data emitted:

1. Each state change must include the transaction that caused it.
2. Each transaction must be paired with its corresponding block.
3. All changes must be absolute values (final state), not deltas.

Details of the data model that encodes these changes, transactions, and blocks in messages are available <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/proto/tycho/evm/v1/common.proto" target="_blank" rel="noopener noreferrer">here</a>. These models facilitate communication between Substreams and the Tycho Indexer, and within Substreams modules. Tycho Indexer expects to receive a `BlockChanges` output from your Substreams package.

You must aggregate changes at the transaction level. Emitting `BlockChanges` with duplicate transactions in the `changes` attributes is an error.

### Data Encoding

To ensure compatibility across blockchains, many data types are encoded as variable-length bytes. This flexible approach requires an informal interface so that consuming applications can interpret these bytes consistently:

* **Integers:** When encoding integers, particularly those representing balances, always use unsigned big-endian format. Multiple points within the system reference balances, so they must be consistently decoded along their entire journey.
* **Strings**: Use UTF-8 encoding for any string data stored as bytes.
* **Attributes:** Attribute encoding is variable and depends on specific use cases. But whenever possible, follow the encoding standards above for integers and strings.

### Reserved Attributes

We reserve some attribute names for specific functions in our simulation process. Use these names only for their intended purposes. [See list of reserved attributes](./#reserved-attributes).

## Changes of interest

Tycho Protocol Integrations should communicate the following changes:

1. **New Protocol Components**: Signify any newly added protocol components. For example, pools, pairs, or markets – anything that indicates you can execute a new operation using the protocol.
2. **ERC20 Balances**: For any contracts involved with the protocol, you should report balance changes in terms of absolute balances.
3. **Protocol State Changes**: For VM integrations, this typically involves reporting contract storage changes for all contracts whose state is accessible during a swap operation (except token contracts).

For a hands-on integration guide, see the following pages:

{% content-ref url="1.-setup.md" %}
[1.-setup.md](1.-setup.md)
{% endcontent-ref %}

{% content-ref url="2.-implementation.md" %}
[2.-implementation.md](2.-implementation.md)
{% endcontent-ref %}

{% content-ref url="../3.-testing.md" %}
[3.-testing.md](../3.-testing.md)
{% endcontent-ref %}

