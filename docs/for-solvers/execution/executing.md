# Executing

Once you have calldata from [Encoding](encoding/), you can execute your trade via the Tycho Router.

## Tycho Router

Send the encoded calldata to the TychoRouterV3 <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/contracts/src/TychoRouterV3.sol" target="_blank" rel="noopener noreferrer">contract</a> (see contract addresses [here](contract-addresses.md)). Preparation depends on the `user_transfer_type` in your `Solution`:

* `TransferFrom`: Call `approve()` on your input token to allow the TychoRouterV3 to spend it.
* `TransferFromPermit2`: Approve the Permit2 contract - use the `Permit2` utility from the encoding crate to build and sign the `PermitSingle`. You must handle the permit; the encoder does not.

For an example of how to execute trades using the Tycho Router, refer to the [Quickstart](../../#id-5.-simulate-or-execute-the-best-swap).

### Fee Taking

The TychoRouterV3 supports a dual fee system:

* **Client fees**: Construct a `ClientFeeParams` with your `client_fee_bps`, `client_fee_receiver`, and signature, and pass it when calling the router. The router transfers the fee to the receiver in the same transaction.
* **Router fees**: Configured on-chain by Propeller Heads. These are mandatory and cannot be bypassed through encoding. The router can charge a fee on the output amount and/or a percentage of the client fee. Currently set to 0.1 bps (0.001%) on the swap output and 20% share of the client fee (the integrator keeps 80%).

All fee rates — yours and the router's — use 8-decimal fee units where `100_000_000` = 100%. See [Fee units](encoding/#fee-units).

Fees apply to the output your swap actually produced, never to your quote. When the router captures positive slippage, it subtracts that surplus first and charges fees on what remains. If the fees would exceed the output altogether, the swap reverts with `TychoRouter__FeesExceedOutput`.

Positive slippage capture is a single on-chain switch on the FeeCalculator, and Propeller Heads can exempt individual client addresses from it. An exempt client's swaps keep the whole surplus above `expectedAmountOut`. Propeller Heads can also give a client a share of the captured surplus. The router transfers that share to the client together with the client fee, and keeps the rest. Propeller Heads can also cap the surplus it captures from a client at a percentage of the swap's actual output. Your swap keeps any surplus above the cap, and fees apply to it. The router resolves which client applies the same way it resolves fee rates: from `clientFeeReceiver` when you pass a signed one, otherwise from `tx.origin`.

#### Custom router fee rates

Propeller Heads can configure a custom router fee rate for specific client addresses. If your address has a negotiated rate, the router applies it automatically — no extra configuration required on your end.

When you call the router without a `clientFeeReceiver` (i.e., passing all-zero `ClientFeeParams`), the router looks up custom fee rates using `tx.origin`. This means your negotiated rate applies to any transaction you originate, even when no client signature is present. When a `clientFeeReceiver` is provided, the signed address takes precedence over `tx.origin` for the fee lookup.
