# Executing

Once you have calldata from [Encoding](encoding/), you can execute your trade via the Tycho Router.

## Tycho Router

Send the encoded calldata to the TychoRouter <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/contracts/src/TychoRouter.sol" target="_blank" rel="noopener noreferrer">contract</a> (see contract addresses [here](contract-addresses.md)). Preparation depends on the `user_transfer_type` in your `Solution`:

* `TransferFrom`: Call `approve()` on your input token to allow the TychoRouter to spend it.
* `TransferFromPermit2`: Approve the Permit2 contract - use the `Permit2` utility from the encoding crate to build and sign the `PermitSingle`. You must handle the permit; the encoder does not.
* `UseVaultsFunds`: No approval needed — the router draws from your vault balance. Deposit sufficient funds into the vault before swapping.

For an example of how to execute trades using the Tycho Router, refer to the [Quickstart](../../#id-5.-simulate-or-execute-the-best-swap).

### Fee Taking

The TychoRouter V3 supports a dual fee system:

* **Client fees**: Construct a `ClientFeeParams` with your `client_fee_bps`, `client_fee_receiver`, and signature, and pass it when calling the router. Fees are credited to the receiver's vault balance.
* **Router fees**: Configured on-chain by Propeller Heads. These are mandatory and cannot be bypassed through encoding. The router can charge a fee on the output amount and/or a percentage of the client fee. Currently set to 10 bps (0.1%) on the swap output and 20% share of the client fee (the integrator keeps 80%).

### Client Contribution (Slippage Subsidy)

If the swap output falls below `min_amount_out`, the router covers the shortfall from the client's vault balance, up to `max_client_contribution`. Beyond that, the transaction reverts. This lets clients absorb minor slippage without a separate transaction — but set `max_client_contribution` conservatively, as a high value can expose you to MEV attacks.
