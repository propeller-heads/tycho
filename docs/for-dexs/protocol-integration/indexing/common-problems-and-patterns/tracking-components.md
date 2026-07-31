# Tracking Components

{% hint style="info" %}
Note: this implementation pattern is, by default, used in the <a href="https://github.com/propeller-heads/tycho-indexer/tree/main/protocols/substreams/ethereum-template-factory" target="_blank" rel="noopener noreferrer">ethereum-template-factory</a> template.
{% endhint %}

If protocols use factories to deploy components, a common pattern used during indexing is to detect the creation of these new components and store their contract addresses to track them downstream. Later, you might need to emit balance and state changes based on the current set of tracked components.

### Implementation Steps

1. Implement logic to identify newly created components. A recommended approach is to create a `factory.rs` module to facilitate the detection of newly deployed components.
2. Use the logic/helper module from step 1 in a map handler that consumes `substreams_ethereum::pb::eth::v2::Block` models and outputs a message containing all available information about the component at the time of creation, along with the transaction that deployed it. The recommended output model for this initial handler is <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/proto/tycho/evm/v1/utils.proto" target="_blank" rel="noopener noreferrer">BlockTransactionProtocolComponents</a>.\
   Note that a single transaction may create multiple components. In such cases, `TransactionProtocolComponents.components` should list all newly created `ProtocolComponents`.
3. After emitting, store the protocol components in a `Store`. This you will use later in the module to detect relevant balance changes and to determine whether a contract is relevant for tracking.

{% hint style="danger" %}
Emitting state or balance changes for components not previously registered/stored is considered an error.
{% endhint %}

