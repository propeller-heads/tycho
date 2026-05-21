# Execution

To integrate a new protocol into Tycho, you need to implement two key components:

1. **SwapEncoder** (Rust struct) – Translates swap parameters into protocol-specific calldata.
2. **Executor** (Solidity contract) – Processes that calldata to perform the swap on-chain.

See more about our code architecture [here](code-architecture.md).

## Encoder Interface

Each protocol needs a dedicated encoder that implements the `SwapEncoder` trait:

```rust
fn encode_swap(
    &self,
    swap: Swap,
    encoding_context: EncodingContext,
) -> Result<Vec<u8>, EncodingError>;
```

This function encodes a swap and its relevant context information into calldata that is compatible with the `Executor` contract. The output of the `SwapEncoder` is the input of the `Executor` (see next section). We recommend using packed encoding to save gas. See current implementations <a href="https://github.com/propeller-heads/tycho-indexer/tree/main/crates/tycho-execution/src/encoding/evm/swap_encoder" target="_blank" rel="noopener noreferrer">here</a>.

For protocol-specific constant addresses, add them to <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/config/protocol_specific_addresses.json" target="_blank" rel="noopener noreferrer">config/protocol\_specific\_addresses.json</a>.

Once your `SwapEncoder` is implemented:

* Add a placeholder address to <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/config/executor_addresses.json" target="_blank" rel="noopener noreferrer">config/executor\_addresses.json</a> and <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/config/test_executor_addresses.json" target="_blank" rel="noopener noreferrer">config/test\_executor\_addresses.json</a>
* Register your encoder in <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/src/encoding/evm/swap_encoder/swap_encoder_registry.rs" target="_blank" rel="noopener noreferrer">`SwapEncoderRegister`</a> if you want it included as a default protocol

<details>

<summary>Protocols Supporting Consecutive Swap Optimizations</summary>

Some protocols — like Uniswap V4 — use flash accounting to batch consecutive swaps without intermediate token transfers. As described in the [Swap Group](../../../for-solvers/execution/encoding/#swap-group) section, each `SwapEncoder` still encodes only a **single swap**; the `StrategyEncoder` concatenates them into a single executor call.

The first swap in a group may carry extra data that subsequent swaps omit (see the diagram below).

<figure><img src="../../../.gitbook/assets/both (1).svg" alt=""><figcaption><p>Diagram representing swap groups</p></figcaption></figure>

<figure><img src="../../../.gitbook/assets/encoding data opt.svg" alt=""><figcaption><p>Output of a SwapEncoder for a group swap</p></figcaption></figure>

</details>

## Swap Interface

Every integrated protocol requires its own swap executor contract. This contract must implement the <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/contracts/interfaces/IExecutor.sol" target="_blank" rel="noopener noreferrer">`IExecutor`</a> interface. See currently implemented executors <a href="https://github.com/propeller-heads/tycho-indexer/tree/main/crates/tycho-execution/contracts/src/executors" target="_blank" rel="noopener noreferrer">here</a>. Please also look through our [Contributing Guidelines for Solidity](../contributing-guidelines.md#changing-solidity-code).

The `IExecutor` interface requires three methods:

#### swap

```solidity
function swap(uint256 amountIn, bytes calldata data, address receiver)
    external
    payable;
```

Called by the Dispatcher via `delegatecall`. This function:

* Accepts the input amount (`amountIn`). The input amount is calculated at execution time, not during encoding, to account for possible slippage.
* Processes the swap using the provided calldata (`data`), which is the output of the `SwapEncoder`.
* Sends output tokens to `receiver`.
* Does not return any `amountOut` - for security purposes, this information is automatically detected using balance checks in the `Dispatcher`

**Important:** Executors must not transfer **any** ERC20 tokens. All input and output token transfers are handled by the Dispatcher (via the `TransferManager`). The only exception is native ETH — executors that interact with protocols requiring ETH as `msg.value` (e.g., Fluid, Rocketpool) handle this themselves and declare `TransferNativeInExecutor` as their transfer type.

#### getTransferData

```solidity
function getTransferData(bytes calldata data)
    external
    payable
    returns (
        TransferManager.TransferType transferType,
        address receiver,
        address tokenIn,
        address tokenOut,
        bool outputToRouter
    );
```

Called by the Dispatcher via `staticcall` before each swap to determine how input tokens should be transferred. The executor returns:

* `transferType`: How the protocol expects to receive tokens (see [Token Transfers](./#token-transfers)).
* `receiver`: Where tokens should be sent (typically the pool address or the router).
* `tokenIn`: The input token address. For native ETH, this must be `0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE` (the `ETH_ADDRESS` constant), not `address(0)`.
* `tokenOut`: The output token address. Same rule applies for native ETH.
* `outputToRouter`: Whether the protocol automatically sends the output token back to the TychoRouter. The Dispatcher uses this to decide whether it needs to transfer the token to the intended receiver.

`transferType`, `receiver` and `outputToRouter` must be **hardcoded** per-executor based on the protocol's requirements — they are not encodable in calldata.

#### fundsExpectedAddress

```solidity
function fundsExpectedAddress(bytes calldata data)
    external
    returns (address receiver);
```

Used during [sequential swaps](../../../concepts.md#sequential) to determine where the **previous** swap should send its output tokens. For example, in a route WBTC → USDC → DAI, before executing the first swap, the Dispatcher calls `fundsExpectedAddress` on the second executor to decide where to send USDC.

* Return the pool address if the protocol accepts direct transfers (e.g., Uniswap V2 pools).
* Return `msg.sender` (the router) if the protocol expects tokens in the router (e.g., callback-based protocols).

### Callbacks

Some protocols require a callback during swap execution (e.g., Uniswap V3, Uniswap V4, Balancer V3). In these cases, the executor contract must also implement <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/contracts/interfaces/ICallback.sol" target="_blank" rel="noopener noreferrer">`ICallback`</a>.

**Required Methods**

```solidity
function handleCallback(
    bytes calldata data
) external returns (bytes memory result);

function verifyCallback(bytes calldata data) external view;

function getCallbackTransferData(bytes calldata data)
    external
    payable
    returns (
        TransferManager.TransferType transferType,
        address receiver,
        address tokenIn,
        uint256 amountIn
    );
```

* `handleCallback`: The main entry point for handling callbacks.
* `verifyCallback`: Should be called within `handleCallback` to ensure that the `msg.sender` is a valid pool from the expected protocol.
* `getCallbackTransferData`: Called by the Dispatcher during the callback to determine how tokens should be transferred. Like `getTransferData`, the transfer type must be hardcoded — the Dispatcher handles the actual transfer based on the returned values.

**Callback Flow**

When a protocol initiates a callback during swap execution, it flows through the `TychoRouter`'s `fallback()` method, which acts as the entry point for all callback requests. The router's fallback function delegates the call to the Dispatcher, which:

1. Calls `getCallbackTransferData` on the executor to determine transfer requirements.
2. Performs the token transfer via the `TransferManager` (the executor does not transfer tokens itself).
3. Calls `handleCallback` on the executor to complete the swap interaction.

The callback data passed through this flow should include the function selector and all necessary information for the executor to complete the swap operation, such as token addresses, amounts, and any protocol-specific parameters required by the pool contract.

## Token Transfers

**Executors do not handle any token transfers**. All ERC20 token transfers are orchestrated by the Dispatcher via the <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/contracts/src/TransferManager.sol" target="_blank" rel="noopener noreferrer">`TransferManager`</a>. The Dispatcher calls `getTransferData` (or `getCallbackTransferData` during callbacks) on the executor to learn _how_ the protocol expects to receive tokens, and then performs the transfer itself.

This design reduces the attack surface — a malicious or buggy executor cannot misroute user funds because it never touches the input token directly.

#### TransferType

Each executor must return a hardcoded `TransferManager.TransferType` from `getTransferData` (and `getCallbackTransferData` for callback executors). The available types are:

<table><thead><tr><th width="210">Transfer Type</th><th width="490">Description</th></tr></thead><tbody><tr><td><code>Transfer</code></td><td>The Dispatcher transfers tokens to the pool (or router) before calling <code>swap</code>. Used by protocols that expect tokens to be present in the pool before the swap call (e.g., Uniswap V2).</td></tr><tr><td><code>ProtocolWillDebit</code></td><td>The protocol pulls tokens from the router via an approval. The Dispatcher approves the protocol to spend the required amount. Used by protocols like Curve and Balancer V2.</td></tr><tr><td><code>TransferNativeInExecutor</code></td><td>The executor sends native ETH as <code>msg.value</code> during the swap. The Dispatcher only performs accounting — no ERC-20 transfer occurs. Used by protocols like Fluid and Rocketpool.</td></tr><tr><td><code>None</code></td><td>No transfer is needed at this point. Typically returned by <code>getTransferData</code> for callback-based protocols where the transfer happens inside the callback instead.</td></tr></tbody></table>

**The only case where an executor handles a token transfer is native ETH** (`TransferNativeInExecutor`). For all ERC-20 tokens, the Dispatcher is solely responsible for transfers.

#### How the Dispatcher Resolves Transfers

Before each swap, the Dispatcher:

1. Calls `getTransferData` on the executor to get the `TransferType`, `receiver`, token addresses, and whether the protocol sends out tokens back to the router automatically.
2. Determines the transfer strategy based on the swap context (first swap vs. subsequent, split swap, vault-funded, etc.).
3. Performs the input token transfer via the `TransferManager`.

After each swap, the Dispatcher:

1. Performs a balance check to determine the token output amount of the swap
2. If `outputToRouter` is true, forwards output tokens to swap receiver

For sequential swaps, the Dispatcher also calls `fundsExpectedAddress` on the _next_ executor to decide where the current swap should send its output tokens — either directly to the next pool or back to the router.

The transfer behavior is fully determined by the values your executor returns from `getTransferData` and `fundsExpectedAddress`.

### Native Token Address Handling

Tycho uses `address(0)` to represent native tokens during indexing and simulation, but uses `0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE` (`ETH_ADDRESS`) consistently within the router and executor calldata. The boundary between these two representations lives in the **swap encoder**.

#### SwapEncoder: translate `address(0)` → `ETH_ADDRESS`

In your `SwapEncoder`, if you encode token addresses into the calldata, convert `address(0)` to `ETH_ADDRESS` (`0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE`). A helper is available:

```rust
use crate::encoding::evm::utils::native_to_router_eth;

let token_in = native_to_router_eth(bytes_to_address(&swap.token_in().address)?);
```

#### Executor `swap`: translate `ETH_ADDRESS` → protocol address

If your protocol uses `address(0)` (e.g., UniswapV4, Ekubo) or another sentinel for native ETH in its pool keys or function calls, translate `ETH_ADDRESS` back to the protocol's expected address inside `swap()` (or `_decodeData()` / `_locked()`), right before interacting with the protocol.

## Fee Tokens

Balance checks before and after token transfers mean fee-on-transfer tokens and rebasing tokens work on most protocols. The exception is Uniswap V3-like protocols, which require declaring the exact input amount upfront but only transfer it inside a callback.

## Security Requirements

TychoRouter calls executors via `delegatecall`, so executor code runs within TychoRouter's context — it can freely transfer the router's assets and write to the router's storage, including users' vault balances. Follow this checklist when building or reviewing an executor.

{% hint style="danger" %}
**Executors run with TychoRouter's full privileges.** A bug or malicious executor can steal all router assets.
{% endhint %}

* **Never call `ERC20.transfer`, `ERC20.transferFrom`, or `Permit2.transferFrom` directly.** Communicate transfer intent through `getTransferData` and `getCallbackTransferData` instead — TychoRouter performs the actual transfers with its own safeguards. The only exception is for native ETH transfers: this transfers must be handled inside the Executor and have a `transferType` of `TransferNativeInExecutor`.
* **Never do ERC20 token approvals**. These are all handled by the TychoRouter.
* **Never assign to a state variable** or perform any operation that writes to TychoRouter's storage.
* **Do not execute `delegatecall`.** If a protocol makes it unavoidable, ensure the caller cannot control the `delegatecall` target — attacker-controlled targets enable arbitrary code execution within TychoRouter's context.
* **Avoid trusting data sent via callback.** If necessary, call `verifyCallback` within `handleCallback` to confirm `msg.sender` is a valid pool from the expected protocol.
* **`handleCallback`'s `data` argument** contains raw ABI-encoded calldata that the executor must decode manually.
* **`handleCallback`'s return value** must be raw ABI-encoded return data that the executor encodes manually.

## Testing

Each new integration must be thoroughly tested in both Rust and Solidity. This includes:

* Unit tests for the `SwapEncoder` in Rust
* Unit tests for the `Executor` in Solidity
* Two key **integration tests** to verify the full swap flow: `SwapEncoder` to `Executor` integration test and a full TychoRouter integration test

#### 1. SwapEncoder ↔ Executor Integration Test

Verify that the calldata generated by the `SwapEncoder` is accepted by the corresponding `Executor`.

Use the helper functions:

* `write_calldata_to_file()` in the encoding module (Rust)
* `loadCallDataFromFile()` in the execution module (Solidity)

These helpers save and load the calldata to/from `calldata.txt`.

#### 2. Full TychoRouter Integration Test

* In `tests/protocol_integration_tests.rs`, write a Rust test that encodes a single swap and saves the calldata using `write_calldata_to_file()`.
* In `TychoRouterTestSetup`, deploy your new executor and add it to executors list in `deployExecutors`.
* Run the setup to retrieve your executor’s deployed address and add it to <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/config/test_executor_addresses.json" target="_blank" rel="noopener noreferrer">config/test\_executor\_addresses.json</a>.
* Create a new Solidity test contract that inherits from `TychoRouterTestSetup`. For example:

```solidity
 contract TychoRouterForYouProtocolTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 22644371; // Use a block that fits your test scenario
    }

    function testSingleYourProtocolIntegration() public {
        ...
    }
}
```

These tests ensure your integration works end-to-end within Tycho’s architecture.

## Deploying and Whitelisting

Once your implementation is approved:

1. **Deploy the executor contract** on the appropriate network (more <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/contracts/scripts/README.md" target="_blank" rel="noopener noreferrer">here</a>).
2. **Contact us** to whitelist the new executor address on our main router contract.
3. **Update the configuration** by adding the new executor address to `executor_addresses.json` and register the `SwapEncoder` within the `SwapEncoderBuilder` .

By following these steps, your protocol will be fully integrated with Tycho.
