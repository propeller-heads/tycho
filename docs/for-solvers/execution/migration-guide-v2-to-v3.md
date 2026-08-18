# Migration Guide

This guide covers the breaking changes between Router versions from the perspective of users who consume the Rust
encoding library or interact with the TychoRouterV3 contracts. Migrating from V2 means working through both sections in
order.

## V2 to V3

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
checked_amount: min_amount_out,
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
amount,           // amount_in
min_amount_out,   // min_amount_out
vec![swap],       // swaps
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
min_amount_out,
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

The TychoRouterV3 methods now include a `ClientFeeParams` struct in their signatures:

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

The TychoRouterV3 now includes an ERC6909 vault. Key changes:

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

## V3 to V3.1

{% hint style="info" %}
Router V3.0 stays available on the `tycho-execution` releases that precede this change. Pin your
dependency to the last of those releases to keep using it. All later versions target V3.1.
{% endhint %}

### Encoding Changes

#### Solution Struct

**New field:** `min_amount_out` keeps its meaning, and the quote behind it joins the struct.

<table>
<thead><tr><th width="210">Field</th><th width="210">Type</th><th width="280">Description</th></tr></thead>
<tbody>
<tr><td><code>expected_amount_out</code></td><td><code>BigUint</code></td><td>The output amount your simulation quoted. Becomes the router's <code>expectedAmountOut</code>. Must be non-zero — <code>validate_solution</code> now rejects zero</td></tr>
</tbody>
</table>

`Solution::new` takes both amounts, quote first, growing from 7 arguments to 8:

```rust
// V3.0
Solution::new(sender, receiver, token_in, token_out, amount_in, min_amount_out, vec![swap]);

// V3.1
Solution::new(
    sender, receiver, token_in, token_out, amount_in, expected_amount_out, min_amount_out,
    vec![swap],
);
```

The new field brings a getter, `.expected_amount_out() -> &BigUint`, and `.min_amount_out()` is
unchanged. The builders for both amounts are gone: `Solution::new` requires them, so pass them
there instead of `.with_min_amount_out(amount)`. The same applies to the other required fields —
`.with_sender`, `.with_receiver`, `.with_token_in`, `.with_amount_in`, and `.with_token_out` are
removed. `.with_swaps` and `.with_user_transfer_type` remain.

The `tycho-encode` CLI's JSON input gains the matching key: keep `min_amount_out` and add
`expected_amount_out`.

#### ClientFeeParams Struct

`client_fee_bps` widens from `u16` to `u32` and switches from basis points to the 8-decimal fee unit the
router's own rates already used. `ClientFeeParams::new` and `into_abi_params` follow suit.

| Rate             | Fee units     |
|------------------|---------------|
| 100%             | `100_000_000` |
| 1%               | `1_000_000`   |
| 1 BPS (0.01%)    | `10_000`      |
| 0.1 BPS (0.001%) | `1_000`       |

Multiply your existing basis-point rates by `10_000`.

### Execution Changes

#### Contract Name

The Solidity contract is now `TychoRouterV3`, in `contracts/src/TychoRouterV3.sol`. Update any artifact
path, `forge inspect` target, or import that referred to `TychoRouter`. The EIP-712 domain name stays
`"TychoRouter"` — do not change it when rebuilding the domain separator, or your client fee signatures
will fail verification.

#### Router Function Signatures

Every swap method gains an `expectedAmountOut` parameter directly before `minAmountOut`, so all nine
selectors change:

```solidity
// V3.0
singleSwap(amountIn, tokenIn, tokenOut, minAmountOut, receiver, clientFeeParams, swapData)

// V3.1
singleSwap(amountIn, tokenIn, tokenOut, expectedAmountOut, minAmountOut, receiver, clientFeeParams, swapData)
```

`sequentialSwap`, `splitSwap`, and their `Permit2` and `UsingVault` variants take it in the same
position. `splitSwap` keeps `nTokens` between `minAmountOut` and `receiver`.

#### Slippage Bounds

V3.0 accepted any non-zero `minAmountOut`, including `1`. V3.1 requires it to sit inside a window
anchored on `expectedAmountOut`:

```
expectedAmountOut * (10_000 - MAX_SLIPPAGE_TOLERANCE_BPS) / 10_000  <=  minAmountOut  <=  expectedAmountOut
```

`MAX_SLIPPAGE_TOLERANCE_BPS` is `2_000`, which puts the floor 20% below the quote. Two things follow
from this. Calldata that used to pass `minAmountOut = 1` now reverts, so compute a real floor from your
slippage tolerance. And because `expectedAmountOut` sets both ends of the window, inflating it raises
your floor rather than relaxing it — pass the amount your simulation actually returned.

#### Client Fee Signature

The `ClientFee` typehash gains `expectedAmountOut` and widens `clientFeeBps`, so every V3.0 signature
fails verification against V3.1:

```solidity
// V3.0
ClientFee(uint16 clientFeeBps, address clientFeeReceiver, uint256 maxClientContribution,
          uint256 deadline, uint256 amountIn, address tokenIn, address tokenOut,
          uint256 minAmountOut, address receiver, bytes swaps)

// V3.1
ClientFee(uint32 clientFeeBps, address clientFeeReceiver, uint256 maxClientContribution,
          uint256 deadline, uint256 amountIn, address tokenIn, address tokenOut,
          uint256 expectedAmountOut, uint256 minAmountOut, address receiver, bytes swaps)
```

The EIP-712 domain is unchanged. As in V3.0, the signature binds the whole swap, so encode first, then
sign. See [Client Fee Signature](encoding/#client-fee-signature) for a full example.

V3.1 also widens who may sign. V3.0 called `ECDSA.recover` and compared the result to
`clientFeeReceiver`, so only an EOA could act as the client. V3.1 tries ECDSA first and falls back to an
<a href="https://eips.ethereum.org/EIPS/eip-1271" target="_blank" rel="noopener noreferrer">ERC-1271</a>
`isValidSignature` staticcall on the receiver, so a contract — a Safe, for example — can now be the
client, and its `clientSignature` carries no length constraint. Because ECDSA runs first, an EOA holding
delegated code (EIP-7702) keeps signing with its own key. Nothing changes for existing EOA clients
beyond the typehash above.

#### Fee Calculation

Fees are now charged on the swap output minus any positive slippage the router captured, rather than on
the full output. To check whether the router is capturing surplus at all, call
`getPositiveSlippageEnabled()` on the FeeCalculator.

The FeeCalculator interface changes:

| V3.0                                                                        | V3.1                                                                     |
|-----------------------------------------------------------------------------|--------------------------------------------------------------------------|
| `calculateFee(amountIn, client, clientFeeBps)` → `(amountOut, FeeRecipient[])` | `calculateFee(FeeInput)` → `FeeRecipient[]`, always `[router, client]` |
| `getEffectiveRouterFeeOnOutput(client)`                                     | Removed — read `getAllClientFees(start, count)`                          |
| `getEffectiveRouterFeeOnOutputScaled(client)`                               | Removed                                                                  |
| `getEffectiveRouterFeeOnClientFee(client)`                                  | Removed                                                                  |
| `MAX_FEE_BPS`, `MAX_FEE_BPS_SQUARED`                                        | `MAX_BPS`, `MAX_BPS_SQUARED` — same values                               |
| —                                                                           | `mustOutputThroughRouter(clientFeeBps, client)` → `bool`                 |
| —                                                                           | `getPositiveSlippageEnabled()` → `bool`                                  |

`calculateFee` no longer returns the post-fee amount — subtract the recipients' amounts yourself. It
takes a struct instead of loose arguments:

```solidity
struct FeeInput {
    uint256 actualAmountOut;
    uint256 expectedAmountOut;
    uint256 amountIn;
    address tokenIn;
    address tokenOut;
    uint32 clientFeeBps;
    address client;
}
```

#### Revert Reasons

| Error                                                                | Cause                                                     |
|----------------------------------------------------------------------|-----------------------------------------------------------|
| `TychoRouter__InvalidMinAmountOut(minAmountOut, expectedAmountOut)`   | `minAmountOut` outside the slippage window, or zero       |
| `TychoRouter__AmountOutZero()`                                       | `expectedAmountOut` is zero                               |
| `TychoRouter__FeesExceedOutput(totalFees, actualAmountOut)`          | Calculated fees exceed the swap output                    |

`TychoRouter__UndefinedMinAmountOut` is removed — the first two errors replace it.
