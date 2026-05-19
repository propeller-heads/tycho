---
description: How to integrate Tycho in different execution venues.
---

# Execution Venues

## Cow Protocol

To solve orders on <a href="https://docs.cow.fi/cow-protocol/tutorials/solvers" target="_blank" rel="noopener noreferrer">CoW Protocol</a>, you'll need to prepare your solution following specific formatting requirements.

First, initialize the encoder:

```rust
let swap_encoder_registry = SwapEncoderRegistry::new_with_defaults(Chain::Ethereum)
    .expect("Failed to get default SwapEncoderRegistry");

let encoder = TychoRouterEncoderBuilder::new()
    .chain(Chain::Ethereum)
    .swap_encoder_registry(swap_encoder_registry)
    .build()
    .expect("Failed to build encoder");
```

When solving for CoW Protocol, you need to return a <a href="https://docs.cow.fi/cow-protocol/reference/core/auctions/schema#solutions-output" target="_blank" rel="noopener noreferrer">Solution object</a> that contains a list of interactions to be executed in sequence.

To solve with the Tycho Router you only need one custom interaction where:

1. `callData` is the full encoded method calldata using the encoded solution returned from `encoder.encode_solutions(...)`
2. `allowances` is a list with one entry where the allowance for the token in and amount in is set for spender to be the Tycho Router. This is necessary for the `transferFrom` to work.

## Uniswap X

To help you fill Uniswap X orders using Tycho, we provide an example <a href="https://github.com/propeller-heads/tycho-execution/blob/main/foundry/src/uniswap_x/UniswapXFiller.sol" target="_blank" rel="noopener noreferrer">`UniswapXFiller`</a> contract. This contract is a starting point — you should adapt it to fit your use case.

The example contract:

* Inherits from `IReactorCallback` and implements `execute` and `reactorCallback`
* Calls the `TychoRouter` from `reactorCallback` to execute swaps
* Uses standard token approvals to allow `TychoRouter` to pull funds; you can replace this with Permit2 easily (you need to change the encoding accordingly though).
* Approves the UniswapX Reactor contract to transfer tokens out after execution
* Only supports solving one order at a time; you can extend it to support batching by implementing `executeBatch` and updating `reactorCallback`
* Can safely hold tokens. The Uniswap X Reactor only transfers out the required amount. If your solution is more efficient, any surplus stays in the filler contract
* Is not audited — use at your own risk

See how to encode the `callbackData` for `TychoRouter` <a href="https://github.com/propeller-heads/tycho-execution/tree/main/examples/uniswapx-encoding-example" target="_blank" rel="noopener noreferrer">here</a>.

<details>

<summary>How to deploy the Uniswap X Filler</summary>

The current <a href="https://github.com/propeller-heads/tycho-execution/blob/main/foundry/scripts/deploy-uniswap-x-filler.js" target="_blank" rel="noopener noreferrer">script</a> deploys a Uniswap X filler and verifies it in the corresponding blockchain explorer.

Make sure to run `unset HISTFILE` in your terminal before setting the private key. This will prevent the private key from being stored in the shell history.

1. Set the following environment variables:

```
export RPC_URL=<chain-rpc-url>
export PRIVATE_KEY=<deploy-wallet-private-key>
export BLOCKCHAIN_EXPLORER_API_KEY=<blockchain-explorer-api-key>
```

2. Confirm that the variables `tychoRouter`, `uniswapXReactor` and `nativeToken` are correctly set in the script. Make sure that the Uniswap X Reactor address matches the reactor you are targeting.
3. Run `npx hardhat run scripts/deploy-uniswap-x-filler.js --network NETWORK`.

</details>

For more on filling Uniswap X orders, see their <a href="https://docs.uniswap.org/contracts/uniswapx/guides/mainnet/createfiller" target="_blank" rel="noopener noreferrer">docs</a> and <a href="https://github.com/marktoda/uniswapx-artemis" target="_blank" rel="noopener noreferrer">examples</a>.

## Other competition venues

For other venues, like 1inch Fusion, please contact us.
