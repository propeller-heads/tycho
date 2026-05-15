# How to deploy

- `cd ./crates/tycho-execution/contracts`
- Install dependencies `npm install`

## [optional test] Deploy on a Tenderly fork

1. Make a new [fork](https://dashboard.tenderly.co/) in tenderly dashboard for the
   chain that you wish to deploy on.
2. Set the following environment variables:
   ```bash
   export RPC_URL=<fork-rpc-from-tenderly>
   export DEPLOY_WALLET=<wallet-address>
   export PRIVATE_KEY=<private-key>
   ```

3. Fund wallet: `npx hardhat run scripts/fund-wallet-tenderly-fork.js --network tenderly`

## Deploy on an EVM chain

Make sure to run `unset HISTFILE` in your terminal before setting the private key. This will prevent the private key
from being stored in the shell history.

1. Set the following environment variables:
   ```bash
   export RPC_URL=<chain-rpc-url>
   export PRIVATE_KEY=<private-key>
   export BLOCKCHAIN_EXPLORER_API_KEY=<blockchain-explorer-api-key>
   ```

## Deploy Tycho Router

### Deploy FeeCalculator

The FeeCalculator must be deployed **before** the TychoRouter, as the router requires its address.

1. Set the fee setter address:
   ```
   export ROUTER_FEE_SETTER=<address-to-grant-fee-setter-role>
   ```
   `ROUTER_FEE_SETTER` receives `ROUTER_FEE_SETTER_ROLE` to manage fee configuration.
2. Deploy: `npx hardhat run scripts/deploy-fee-calculator.js --network NETWORK`
3. Note the deployed address — you will need it in the next step.

### Deploy executors

1. In `scripts/deploy-executors.js` define the executors to be deployed
2. Deploy executors: `npx hardhat run scripts/deploy-executors.js --network NETWORK`
3. Fill in the executor addresses in `config/executor_addresses.json`

### Deploy Router

1. Define the accounts to grant roles to in `scripts/roles.json`. For each role, the first address
   receives the role in the constructor; additional addresses are granted post-deployment via `set-roles.js`.
2. Set environment variables:
   ```
   export FEE_CALCULATOR=<fee-calculator-address-from-previous-step>
   ```
3. Deploy router: `npx hardhat run scripts/deploy-router.js --network NETWORK`
4. Fill in the router address in `config/router_addresses.json`

Via the safe wallet UI:

5. Set the executors addresses
6. Set fee amounts and router fee receiver in FeeCalculator
7. Set the pauser wallets

### Revoke roles

1. If you wish to revoke a role for a certain address, run: `npx hardhat run scripts/revoke-role.js --network NETWORK`
2. There will be a prompt for you to insert the role hash and the address you want to revoke it for.

### Safe wallet

1. If the wallet that has the role, is a Gnosis Safe, you need to set the `SAFE_ADDRESS` env var.
2. The scripts deploy-executors, remove-executor, set-roles and revoke-role all support this.
    1. If `SAFE_ADDRESS` is set, then it will propose a transaction to the safe wallet and later on it needs to be
       approved in their UI to execute on chain. Be sure to change the PRIVATE_KEY to that which has permissions on the
       safe wallet.
    2. If it's not set, it will submit the transaction directly to the chain.

## Export Runtime Bytecode

The `export-runtime-bytecode.js` script allows you to export the runtime bytecode of any executor contract for use in
SDK testing.

### Prerequisites

1. Ensure the contract is compiled: `forge build`
2. Start a local blockchain: `anvil` (or `anvil &` to run in background)

### Usage

```bash
node scripts/export-runtime-bytecode.js <ContractName> [constructorArg1] [constructorArg2] ...
```

### Example

```bash
# Export BalancerV2Executor (requires permit2 address)
node scripts/export-runtime-bytecode.js BalancerV2Executor 0x000000000022D473030F116dDEE9F6B43aC78BA3
```

### Output

The script will:

1. Deploy the contract with the provided constructor arguments to your local fork
2. Extract the runtime bytecode (including immutables)
3. Save it to `test/{ContractName}.runtime.json`

The generated JSON file contains the runtime bytecode in the format expected by the SDK and should be copied to the
appropriate SDK repository for testing. **Do not commit these files to this repository.**
