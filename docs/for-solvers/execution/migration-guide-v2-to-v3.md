# Migration Guide: V2 to V3

This guide covers the breaking changes between V2 and V3 from the perspective of users who consume the Rust encoding
library or interact with the TychoRouter contracts.

{% hint style="info" %}
To keep using Router V2, please encode your swap with `tycho-execution<=0.165.1` . All higher versions support only
Router V3.
{% endhint %}

### Encoding Changes

#### Solution Struct

**Renamed fields:**

<table><thead><tr><th width="210">V2</th><th width="210">V3</th><th width="280">Notes</th></tr></thead><tbody><tr><td><code>given_token</code></td><td><code>token_in</code></td><td>The input token</td></tr><tr><td><code>given_amount</code></td><td><code>amount_in</code></td><td>Amount of the input token</td></tr><tr><td><code>checked_token</code></td><td><code>token_out</code></td><td>The output token</td></tr><tr><td><code>checked_amount</code></td><td><code>min_amount_out</code></td><td>Minimum acceptable output amount</td></tr></tbody></table>

**Removed fields:**

<table><thead><tr><th width="280">Field</th><th width="420">Replacement</th></tr></thead><tbody><tr><td><code>native_action: Option&#x3C;NativeAction></code></td><td>The encoder now inserts WETH wrap/unwrap swaps automatically (see <a href="encoding/#native-tokens">Native Tokens</a>).</td></tr><tr><td><code>exact_out: bool</code></td><td>Only exact-in was ever supported. Removed for simplicity.</td></tr></tbody></table>

**New fields:**

<table><thead><tr><th width="210">Field</th><th width="210">Type</th><th width="280">Description</th></tr></thead><tbody><tr><td><code>user_transfer_type</code></td><td><code>UserTransferType</code></td><td>How user funds enter the router. Moved here from the encoder builder.</td></tr></tbody></table>

**Private fields with getters/setters:**

`Solution` fields are now private — use the constructor and builder methods instead of direct field access:

```rust
// V2
let solution = Solution {
sender: addr,
receiver: addr,
given_token: token_a,
given_amount: amount,
checked_token: token_b,
checked_amount: min_out,
swaps: vec![swap],
exact_out: false,
native_action: Some(NativeAction::Wrap),
};

// V3
let solution = Solution::new(
addr,        // sender
addr,        // receiver
token_a,     // token_in
token_b,     // token_out
amount,      // amount_in
min_out,     // min_amount_out
vec![swap],  // swaps
)
.with_user_transfer_type(UserTransferType::TransferFrom);
```

#### UserTransferType Moved to Solution

`UserTransferType` has moved from the encoder builder to each `Solution`, so solutions in the same batch can use different funding methods.

```rust
// V2
let encoder = TychoRouterEncoderBuilder::new()
.chain(chain)
.user_transfer_type(UserTransferType::TransferFrom)  // set here
.swap_encoder_registry(registry)
.build() ?;

// V3
let encoder = TychoRouterEncoderBuilder::new()
.chain(chain)
.swap_encoder_registry(registry)
.build() ?;

let solution = Solution::new(/* ... */)
.with_user_transfer_type(UserTransferType::TransferFrom);  // set here
```

The `UserTransferType::None` variant has been renamed to `UserTransferType::UseVaultsFunds`, reflecting the new
vault-based architecture.

#### Swap Struct

**Builder methods renamed** (added `with_` prefix for consistency):

| V2                             | V3                                  |
|--------------------------------|-------------------------------------|
| `.split(0.5)`                  | `.with_split(0.5)`                  |
| `.user_data(data)`             | `.with_user_data(data)`             |
| `.protocol_state(state)`       | `.with_protocol_state(state)`       |
| `.estimated_amount_in(amount)` | `.with_estimated_amount_in(amount)` |

**Getter methods renamed** (dropped `get_` prefix):

| V2                           | V3                       |
|------------------------------|--------------------------|
| `.get_split()`               | `.split()`               |
| `.get_user_data()`           | `.user_data()`           |
| `.get_protocol_state()`      | `.protocol_state()`      |
| `.get_estimated_amount_in()` | `.estimated_amount_in()` |

**`token_in` / `token_out` are now `Token`, not `Bytes`:**

In V2 these fields were `Bytes` (raw addresses). In V3 they are `tycho_common::models::token::Token`, carrying decimals,
symbol, and tax/gas metadata alongside the address. Wrap a raw address with the `default_token(addr)` test helper
(available under `#[cfg(any(test, feature = "test-utils"))]`) when full token metadata isn't needed.

```rust
// V2
let swap = Swap::new(component, token_in_bytes, token_out_bytes);

// V3
let swap = Swap::new(component, token_in_token, token_out_token, estimated_gas);
```

**New required parameter on `Swap::new`:**

The constructor now takes a per-swap simulation gas estimate as its 4th argument. The new field is exposed
via `.estimated_gas() -> &BigUint`.

#### EncodedSolution Struct

Fields are now private with getter methods, matching the pattern used elsewhere:

```rust
// V2
let swaps = encoded_solution.swaps;
let sig = encoded_solution.function_signature;

// V3
let swaps = encoded_solution.swaps();
let sig = encoded_solution.function_signature();
```

The `function_signature` field now reflects both the swap strategy and the funding mode. For
example, `splitSwapUsingVault(...)` for a split swap using vault funds.

**Removed `permit` field:**

The `permit: Option<PermitSingle>` field has been removed from `EncodedSolution`. The encoder no longer creates or
returns Permit2 data. If you use `TransferFromPermit2`, you must handle permit creation and signing yourself.

The `Permit2` utility struct has been made public, so you can use it directly.

**New `estimated_gas` field:**

`EncodedSolution` now exposes a `estimated_gas: BigUint` (via `.estimated_gas()`), derived from each
swap's `estimated_gas` and some overheads (from the router and token transfers). Users can use this as minimum estimated
gas for this solution.

#### Wrapping and Unwrapping

V2 used a `NativeAction` enum on the `Solution` with `Wrap` and `Unwrap` variants. The router had dedicated wrap/unwrap
flags.

**V3 removes this entirely.** Instead, a WETH executor handles wrapping and unwrapping as regular swap steps. The
encoder automatically inserts these swaps when it detects ETH↔WETH gaps in the swap path.

```rust
// V2
let solution = Solution {
given_token: eth_address,
checked_token: dai_address,
native_action: Some(NativeAction::Wrap),
swaps: vec![weth_to_dai_swap],
..
};

// V3 — just set token_in to ETH; the encoder adds a WETH wrap swap automatically
let solution = Solution::new(
sender,
receiver,
eth_address,   // token_in is ETH
dai_address,   // token_out is DAI
amount,
min_out,
vec![weth_to_dai_swap],  // first swap expects WETH — encoder bridges the gap
);
```

This also works for mid-path bridging (e.g., if one swap outputs ETH and the next expects WETH) and at the end of a
path. See more in [Native Tokens](encoding/#native-tokens).

#### Encoder Builder

**Removed options:**

| V2 option                  | Notes                             |
|----------------------------|-----------------------------------|
| `.user_transfer_type(...)` | Moved to `Solution`.              |
| `.swapper_pk(...)`         | Removed. Sign Permit2 externally. |
| `.historical_trade()`      | Removed. No longer needed.        |

The V3 builder only requires `chain` and `swap_encoder_registry`:

```rust
// V3
let encoder = TychoRouterEncoderBuilder::new()
.chain(Chain::Ethereum)
.swap_encoder_registry(registry)
.build() ?;
```

#### Transaction and encode\_full\_calldata Removed

The `Transaction` struct and `encode_full_calldata` method have been removed entirely. In V2, `encode_full_calldata` was
already deprecated. V3 only supports `encode_solutions`, which returns `EncodedSolution` objects.

You are responsible for constructing the full method call, including execution-critical parameters
like `min_amount_out`, `receiver`, and fee configuration.

#### SwapEncoderRegistry

`SwapEncoderRegistry::new` now requires a `Chain` parameter:

```rust
// V2
let registry = SwapEncoderRegistry::new()
.add_default_encoders(executors_addresses)?;

// V3
let registry = SwapEncoderRegistry::new_with_defaults(Chain::Ethereum)?;
```

### Execution Changes

#### Router Function Signatures

The TychoRouter V3 methods now include a `ClientFeeParams` struct in their signatures:

```solidity
struct ClientFeeParams {
    uint16 clientFeeBps;
    address clientFeeReceiver;
    uint256 maxClientContribution;
    uint256 deadline;
    bytes clientSignature;
}
```

When constructing calldata yourself (recommended), encode this struct as part of the function arguments. Even if you are
not charging fees, you must pass this parameter with zero values.

A `ClientFeeParams` Rust struct matching this Solidity struct is available in `tycho-execution`. Clients are
responsible for constructing and signing it — the encoder does not use it internally. Call `.into_abi_params()` to
convert it to the ABI-encodable tuple:

```rust
// No fee (zero values)
let client_fee_params = ClientFeeParams::default().into_abi_params();

// With a fee
let client_fee_params = ClientFeeParams {
    client_fee_bps: 50,
    client_fee_receiver: fee_receiver_bytes,
    ..ClientFeeParams::default()
}.into_abi_params();
```

#### Vault Integration

The TychoRouter now includes an ERC6909 vault. Key changes:

* **`UseVaultsFunds`** replaces the old `None` transfer type. Tokens deposited in the vault are tracked per-user and can
  be used for swaps or withdrawn.
* Deposit tokens via `router.deposit(token, amount)` before swapping with vault funds.
* Fees (both client and router fees) are credited to the receiver's vault balance rather than transferred immediately.

For more see [Vault](vault.md).

#### No More Wrap/Unwrap Flags

The router no longer accepts `wrap` or `unwrap` boolean flags. If your calldata construction includes these parameters,
remove them. The WETH executor handles wrapping and unwrapping as part of the swap path.
See [Native Tokens](encoding/#native-tokens "mention").

#### Native ETH Address

When constructing the outer function arguments (`tokenIn` / `tokenOut`), native ETH must be represented as `0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE` — not `address(0)`. The router reverts on `address(0)`.

The `ROUTER_ETH_ADDRESS` constant is exported from the `tycho-execution` crate for this purpose.

#### Method Variants

Each swap strategy (single, sequential, split) gains a third variant — `UsingVault` — alongside the existing standard and Permit2 variants:

| V2                       | V3                          |
|--------------------------|-----------------------------|
| `singleSwap(...)`        | `singleSwap(...)`           |
| `singleSwapPermit2(...)` | `singleSwapPermit2(...)`    |
| —                        | `singleSwapUsingVault(...)` |

`sequentialSwap` and `splitSwap` follow the same pattern. Use `EncodedSolution.function_signature` to determine which variant to call.
