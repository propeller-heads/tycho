# Simulation

To enable simulations for a newly added protocol, it must first be integrated into the Tycho monorepo. Please submit a pull request to the <a href="https://github.com/propeller-heads/tycho-indexer" target="_blank" rel="noopener noreferrer">repository</a> targeting `crates/tycho-simulation`.

## Native Integration

In order to add a new native protocol, you will need to complete the following high-level steps:

1. Create a protocol state struct that contains the state of the protocol, and implements the `ProtocolSim` trait (see <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-simulation/src/protocol/state.rs" target="_blank" rel="noopener noreferrer">here</a>).
2. Create a tycho decoder for the protocol state: i.e. implement `TryFromWithBlock` for `ComponentWithState` to your new protocol state.

Each native protocol should have its own module under `crates/tycho-simulation/src/evm/protocol`.

## VM Integration

To create a VM integration, provide a manifest file and an implementation of the corresponding adapter interface. The <a href="https://github.com/propeller-heads/tycho-indexer" target="_blank" rel="noopener noreferrer">Tycho monorepo</a> contains the tools to integrate DEXs and other onchain liquidity protocols into Tycho.

### Example Implementations <a href="#example-implementations" id="example-implementations"></a>

The following exchanges are integrated with the VM approach:

* Balancer V2 (see code <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/protocols/adapter-integration/evm/src/balancer-v2/BalancerV2SwapAdapter.sol" target="_blank" rel="noopener noreferrer">here</a>)

### Install prerequisites <a href="#install-prerequisites" id="install-prerequisites"></a>

1.  Install <a href="https://book.getfoundry.sh/getting-started/installation#using-foundryup" target="_blank" rel="noopener noreferrer">Foundry</a>, start by downloading and installing the Foundry installer:

    ```bash
    curl -L https://foundry.paradigm.xyz | bash
    ```

    then start a new terminal session and run

    ```bash
    foundryup
    ```
2.  Clone the Tycho monorepo:

    ```bash
    git clone https://github.com/propeller-heads/tycho-indexer
    ```
3.  Install dependencies:

    ```bash
    cd ./tycho-indexer/protocols/adapter-integration/evm/
    forge install
    ```

### Understanding the ISwapAdapter <a href="#understanding-the-iswapadapter" id="understanding-the-iswapadapter"></a>

Read the documentation of the [Ethereum Solidity](ethereum-solidity.md) interface. It describes the functions that need to be implemented and the manifest file.

Additionally, read through the docstring of the <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/protocols/adapter-integration/evm/interfaces/ISwapAdapter.sol" target="_blank" rel="noopener noreferrer">ISwapAdapter.sol</a> interface and the <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/protocols/adapter-integration/evm/interfaces/ISwapAdapterTypes.sol" target="_blank" rel="noopener noreferrer">ISwapAdapterTypes.sol</a> interface, which defines the data types and errors the adapter interface uses. You can also generate the documentation locally and look at the generated documentation in the `./docs` folder:

```bash
cd ./evm/
forge doc
```

### Implementing the ISwapAdapter interface <a href="#implementing-the-iswapadapter-interface" id="implementing-the-iswapadapter-interface"></a>

Your integration should be in a separate directory in the `protocols/adapter-integration/evm/src` folder. Start by copying the template directory:

```bash
cp ./protocols/adapter-integration/evm/src/template ./protocols/adapter-integration/evm/src/<your-adapter-name>
```

Implement the `ISwapAdapter` interface in the `./protocols/adapter-integration/evm/src/<your-adapter-name>.sol` file. See Balancer V2 implementation for reference.

### Testing your implementation <a href="#testing-your-implementation" id="testing-your-implementation"></a>

1. Set up test files:
   * Copy `evm/test/TemplateSwapAdapter.t.sol`
   * Rename to `<your-adapter-name>.t.sol`
2. Write comprehensive tests:
   * Test all implemented functions.
   * Use fuzz testing (see <a href="https://book.getfoundry.sh/forge/tests" target="_blank" rel="noopener noreferrer">Foundry test guide</a>, especially the chapter for <a href="https://book.getfoundry.sh/forge/fuzz-testing" target="_blank" rel="noopener noreferrer">Fuzz testing</a>)
   * Reference existing test files: `BalancerV2SwapAdapter.t.sol`
3. Configure fork testing (run a local mainnet fork against actual contracts and data):
   * Set `ETH_RPC_URL` environment variable
   * Use your own Ethereum node or services like <a href="https://infura.io/" target="_blank" rel="noopener noreferrer">Infura</a>
4.  Run the tests with

    ```bash
    cd ./evm
    forge test
    ```

### Add implementation to Tycho simulation

Once you have the swap adapter implemented for the new protocol, you will need to:

1.  Generate the adapter runtime file by running the <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/protocols/adapter-integration/evm/scripts/buildRuntime.sh" target="_blank" rel="noopener noreferrer">`protocols/adapter-integration/evm/scripts/buildRuntime.sh`</a> script with the proper input parameters.\
    \
    For example, in order to build the `Balancer V2` runtime, the following command can be run:\


    ```
    >>> cd protocols/adapter-integration/evm
    >>> ./scripts/buildRuntime.sh -c “BalancerV2SwapAdapter” -s “constructor(address)” -a “0xBA12222222228d8Ba445958a75a0704d566BF2C8”
    ```


2. Add the associated adapter runtime file to `crates/tycho-simulation/src/protocol/vm/assets`. Make sure to name the file according to the protocol name used by Tycho Indexer in the following format: `<Protocol><Version>Adapter.evm.runtime`. For example: `vm:balancer_v2` will be `BalancerV2Adapter.evm.runtime`. Following this naming format is important as we use an automated name resolution for these files.

### Filtering

If your implementation does not support all pools indexed for a protocol, you can create a filter function to handle this. This filter can then be used when registering an exchange in the `ProtocolStreamBuilder`. See <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-simulation/src/evm/protocol/filters.rs" target="_blank" rel="noopener noreferrer">here</a> for example implementations.

## Gas Estimation

The <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/src/encoding/evm/gas_estimator.rs" target="_blank" rel="noopener noreferrer">gas estimator</a> builds total execution gas from two components: the simulation-reported swap gas and token-transfer overhead that simulation does not capture. Which overhead applies depends on your protocol's behavior.

### Native Integrations

Your `get_amount_out` implementation must return a gas estimate. Measure only what happens inside the protocol's `swap()` call, including any token transfers that occur during a callback.

After implementing, add your protocol to the relevant constant slices in `gas_estimator.rs`:

* **`PROTOCOLS_CALLBACK`**: The pool pulls tokens inside a callback fired during `swap()`. The simulation captures that transfer, so the estimator adds no separate input-transfer cost.
* **`PROTOCOLS_OPTIMIZABLE_TRANSFER_IN`**: The router might send tokens directly to the pool, skipping the router-to-pool hop. The estimator adds no router-to-pool transfer cost for non-split strategies.
* **`PROTOCOLS_NEEDING_APPROVAL`**: The pool pulls tokens from the router via `approve` + `transferFrom` (ProtocolWillDebit). The simulation includes the `transferFrom` gas, but not the `approve` (25,000 gas); the estimator adds it separately.
* **`PROTOCOLS_OUTPUT_TO_ROUTER`**: The pool sends output to `msg.sender` (the router) instead of to an explicit receiver. The estimator adds an extra forward transfer from the router to the final receiver.

A protocol can appear in more than one list.

### VM Integrations

In your adapter contract's `swap` function, measure gas with `gasleft()` and return it in the `Trade` struct. The measurement must cover only the pool call itself.

Do not include gas for:
* Token transfers (input or output): the Dispatcher handles these via `TransferManager`
* `approve()` calls: also handled by the Dispatcher

Including them would double-count with what the gas estimator adds based on your protocol's category.
