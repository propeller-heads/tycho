# Tracking Contract Storage

{% hint style="info" %}
This implementation pattern is, by default, used in both the <a href="https://github.com/propeller-heads/tycho-indexer/tree/main/protocols/substreams/ethereum-template-factory" target="_blank" rel="noopener noreferrer">ethereum-template-factory</a> and the <a href="https://github.com/propeller-heads/tycho-indexer/tree/main/protocols/substreams/ethereum-template-singleton" target="_blank" rel="noopener noreferrer">ethereum-template-singleton</a> templates.
{% endhint %}

In VM implementations, accurately identifying and extracting relevant contract changes is essential.&#x20;

The `tycho_substreams::contract::extract_contract_changes` helper function simplifies this process significantly.

{% hint style="warning" %}
Note: These contract helper functions require the extended block model from substreams for your target chain.
{% endhint %}

### Factory protocols

In factory-based protocols, each contract typically corresponds to a unique component, allowing its hex-encoded address to serve as the component ID, provided there is a one-to-one relationship between contracts and components.

The example below shows how to use a component store to define a predicate. This predicate filters for contract addresses of interest:

```rust
use tycho_substreams::contract::extract_contract_changes;

// all changes on this block, aggregated by transaction
let mut transaction_changes: HashMap<_, TransactionChanges> = HashMap::new();

extract_contract_changes(
    &block,
    |addr| {
        components_store
            .get_last(format!("pool:{0}", hex::encode(addr)))
            .is_some()
    },
    &mut transaction_changes,
);
```

### Other protocols

For protocols where contracts aren't necessarily pools themselves, you'll need to identify specific contracts to track. These addresses can be:

1. Hard-coded (for single-chain implementations)
2. Configured via parameters in your <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/protocols/substreams/ethereum-template-singleton/substreams.yaml" target="_blank" rel="noopener noreferrer">substreams.yaml</a> file (for chain-agnostic implementations)
3. Read from the storage of a known contract (hardcoded or configured)

Here's how to extract changes for specific addresses using configuration parameters:

```yaml
// substreams.yaml
...

networks:
  mainnet:
    params:
      map_protocol_changes: "vault_address=0000,swap_helper_address=0000"

...
```

<pre class="language-rust"><code class="lang-rust"><strong>// map_protocol_changes
</strong><strong>use tycho_substreams::contract::extract_contract_changes;
</strong>
// all changes on this block, aggregated by transaction
let mut transaction_changes: HashMap&#x3C;_, TransactionChanges> = HashMap::new();

// *params* is a module input var
let config: DeploymentConfig = serde_qs::from_str(params.as_str())?;
extract_contract_changes_builder(
    &#x26;block,
    |addr| {
        addr == config.vault_address
        || addr == config.swap_helper_address
    },
    &#x26;mut transaction_changes,
);
</code></pre>
