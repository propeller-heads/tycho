# Executing

Once you have calldata from [Encoding](encoding/), you can execute your trade via the Tycho Router.

## Tycho Router

Send the encoded calldata to the TychoRouterV3 <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/contracts/src/TychoRouterV3.sol" target="_blank" rel="noopener noreferrer">contract</a> (see contract addresses [here](contract-addresses.md)). Preparation depends on the `user_transfer_type` in your `Solution`:

* `TransferFrom`: Call `approve()` on your input token to allow the TychoRouterV3 to spend it.
* `TransferFromPermit2`: Approve the Permit2 contract - use the `Permit2` utility from the encoding crate to build and sign the `PermitSingle`. You must handle the permit; the encoder does not.
* `UseVaultsFunds`: No approval needed — the router draws from your vault balance. Deposit sufficient funds into the vault before swapping.

For an example of how to execute trades using the Tycho Router, refer to the [Quickstart](../../#id-5.-simulate-or-execute-the-best-swap).

### Fee Taking

The TychoRouterV3 supports a dual fee system:

* **Client fees**: Construct a `ClientFeeParams` with your `client_fee_bps`, `client_fee_receiver`, and signature, and pass it when calling the router. Fees are credited to the receiver's vault balance.
* **Router fees**: Configured on-chain by Propeller Heads. These are mandatory and cannot be bypassed through encoding. The router can charge a fee on the output amount and/or a percentage of the client fee. Currently set to 0.1 bps (0.001%) on the swap output and 20% share of the client fee (the integrator keeps 80%).

All fee rates — yours and the router's — use 8-decimal fee units where `100_000_000` = 100%. See [Fee units](encoding/#fee-units).

Fees apply to the output your swap actually produced, never to your quote. When the router captures positive slippage, it subtracts that surplus first and charges fees on what remains. If the fees would exceed the output altogether, the swap reverts with `TychoRouter__FeesExceedOutput`.

#### Custom router fee rates

Propeller Heads can configure a custom router fee rate for specific client addresses. If your address has a negotiated rate, the router applies it automatically — no extra configuration required on your end.

When you call the router without a `clientFeeReceiver` (i.e., passing all-zero `ClientFeeParams`), the router looks up custom fee rates using `tx.origin`. This means your negotiated rate applies to any transaction you originate, even when no client signature is present. When a `clientFeeReceiver` is provided, the signed address takes precedence over `tx.origin` for the fee lookup.

### Client Contribution (Slippage Subsidy)

If the swap output falls below `minAmountOut`, the router covers the shortfall from the client's vault balance, up to `max_client_contribution`. Beyond that, the transaction reverts with `TychoRouter__NegativeSlippage`. This lets clients absorb minor slippage without a separate transaction — but set `max_client_contribution` conservatively, as a high value can expose you to MEV attacks.

A contribution requires a signed `ClientFeeParams`: the router rejects a non-zero `maxClientContribution` when `clientFeeReceiver` is the zero address.

The contribution comes out of the client's own vault balance, so `clientFeeReceiver` has to be an address that can both hold a balance and sign. Either an EOA or a contract qualifies: the router recovers an ECDSA signature first and falls back to an <a href="https://eips.ethereum.org/EIPS/eip-1271" target="_blank" rel="noopener noreferrer">ERC-1271</a> check, so a Safe can act as the client. Deposit the contribution into that address's vault balance before the swap — see [Client Fee Signature](encoding/#client-fee-signature).
