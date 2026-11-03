# Reserved Attributes

Certain attribute names are reserved exclusively for specific purposes. Please use them only for their intended applications. Attribute names are unique: if the same attribute is set twice, the value will be overwritten.

## Static Attributes

The following attributes names are reserved and must be given using `ProtocolComponent.static_att`. These attributes MUST be immutable.

### 1. **`manual_updates`**

#### Description

Determines whether the component updates should be manually triggered using the `update_marker` state attribute. By default, updates occur automatically whenever there is a change indexed for any of the required contracts. For contracts with frequent changes, automatic updates may not be desirable. For instance, a change in Balancer Vault storage should only trigger updates for the specific pools affected by the change, rather than for all pools indiscriminately. The `manual_updates` field helps to control and prevent unnecessary updates in such cases.

If it's enable, updates on this component are only triggered by emitting an `update_marker` state attribute (described [below](reserved-attributes.md#id-3.-update_marker)).

#### Type

Set to `[1u8]`to enable manual updates.

#### Example Usage

```rust
Attribute {
    name: "manual_updates".to_string(),
    value: [1u8],
    change: ChangeType::Creation.into(),
}
```

### 2. **`pool_id`**

#### Description

The `pool_id` static attribute is used to specify the identifier of the pool when it differs from the `ProtocolComponent.id`. For example, Balancer pools have a component ID that corresponds to their contract address, and a separate pool ID used for registration on the Balancer Vault contract (needed for swaps and simulations).

**Notice**: In most of the cases, using `ProtocolComponent.id` is preferred over `pool_id` and `pool_id` should only be used if a special identifier is strictly necessary.

#### Type

This attribute value must be provided as a UTF-8 encoded string in bytes.

#### Example Usage

```rust
Attribute {
    name: "pool_id".to_string(),
    value: format!("0x{}", hex::encode(pool_registered.pool_id)).as_bytes(),
    change: ChangeType::Creation.into(),
}
```

### 3. **`is_exclusive`**

#### Description

Marks a component that gates swaps behind off-chain authorization. Fynd uses this attribute to keep such pools out of public routing.

Presence of the attribute is the signal: consumers should test whether the key exists rather than parse its value. Do not set it on non-exclusive components.

#### Type

Set to `[1u8]` to mark the component as exclusive.

#### Example Usage

```rust
Attribute {
    name: "is_exclusive".to_string(),
    value: vec![1u8],
    change: ChangeType::Creation.into(),
}
```

##

## State Attributes

The following attributes names are reserved and must be given using `EntityChanges`. Unlike static attributes, state attributes are updatable.

### 1. **`stateless_contract_addr`**

#### Description

The `stateless_contract_addr_{index}` field specifies the address of a stateless contract required by the component. Stateless contracts are those where storage is not accessed for the calls made to it during swaps or simulations.&#x20;

This is particularly useful in scenarios involving `DELEGATECALL`. If the contract's bytecode can be retrieved in Substreams, provide it using the `stateless_contract_code` attribute (see [below](reserved-attributes.md#id-2.-stateless_contract_code)).&#x20;

**Note:** If no contract code is given, the consumer of the indexed protocol has to access a chain node to fetch the code. This is considered non-ideal and should be avoided where possible.

An index is used if multiple stateless contracts are needed. This index should start at 0 and increment by 1 for each additional `stateless_contract_addr`.

The value for `stateless_contract_addr_{index}` can be provided in two ways:

1. **Direct Contract Address**: A static contract address can be specified directly.
2. **Dynamic Address Resolution**: Alternatively, you can define a function or method that dynamically resolves and retrieves the stateless contract address at runtime. This can be particularly useful in complex contract architectures, such as those using a dynamic proxy pattern. It is important to note that the called contract must be indexed by the Substreams module.

#### Type

This attribute value must be provided as a UTF-8 encoded string in bytes.

#### Example Usage

**1. Direct Contract Address**

To specify a direct contract address:

```rust
Attribute {
    name: "stateless_contract_addr_0".into(),
    value: format!("0x{}", hex::encode(address)).into_bytes(),
    change: ChangeType::Creation.into(),
}
Attribute {
    name: "stateless_contract_addr_1".into(),
    value: format!("0x{}", hex::encode(other_address)).into_bytes(),
    change: ChangeType::Creation.into(),
}
```

**2. Dynamic Address Resolution**

To specify a function that dynamically resolves the address:

```rust
Attribute {
    name: "stateless_contract_addr_0".into(),
    // Call views_implementation() on TRICRYPTO_FACTORY
    value: format!("call:0x{}:views_implementation()", hex::encode(TRICRYPTO_FACTORY)).into_bytes(),
    change: ChangeType::Creation.into(),
}
```

### 2. **`stateless_contract_code`**

#### Description

The `stateless_contract_code_{index}` field is used to specify the bytecode for a given `stateless_contract_addr`. The index used here must match with the index of the related address.

#### Type

This attribute value must be provided as bytes.

#### Example Usage

```rust
Attribute {
    name: "stateless_contract_code_0".to_string(),
    value: code.to_vec(),
    change: ChangeType::Creation.into(),
}
```

### 3. **`update_marker`**

#### Description

The `update_marker` field is used to indicate that a pool has changed, thereby triggering an update on the protocol component. This is particularly useful for when [`manual_updates`](reserved-attributes.md#id-1.-manual_updates) is enabled.

#### Type

Set to `[1u8]`to trigger an update.

#### Example Usage

```rust
Attribute {
    name: "update_marker".to_string(),
    value: vec![1u8],
    change: ChangeType::Update.into(),
};
```

### 4. **`balance_owner`**\[deprecated]

#### Description

The `balance_owner` field specifies the address of the account that owns the protocol component tokens, when tokens are not owned by the protocol component itself or the multiple contracts are involved. This is particularly useful for protocols that use a vault, for example Balancer.

{% hint style="info" %}
The use of the `balance_owner` reserved attribute has been deprecated in favour of tracking contract balances directly. See [Tracking Contract Balances](common-problems-and-patterns/tracking-contract-balances.md).
{% endhint %}

#### Type

This attribute value must be provided as bytes.

#### Example Usage

```rust
Attribute {
    name: "balance_owner".to_string(),
    value: VAULT_ADDRESS.to_vec(),
    change: ChangeType::Creation.into(),
}
```
