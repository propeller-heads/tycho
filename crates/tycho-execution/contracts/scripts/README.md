# How to deploy

- `cd ./crates/tycho-execution/contracts`
- Install dependencies `npm install`

## [optional test] Deploy on a Tenderly fork

1. Make a new [fork](https://dashboard.tenderly.co/) in tenderly dashboard for the
   chain that you wish to deploy on.
2. Set the following environment variables:
   ```bash
   export RPC_URL=<fork-rpc-from-tenderly>
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

On Robinhood Chain (`robinhood`), leave `BLOCKCHAIN_EXPLORER_API_KEY` unset.

If verification reports that Blockscout has not indexed the contract, re-run the deploy script
later. Re-running skips a deployment that already exists.

## Deploy Tycho Router

### Deploy FeeCalculator

The FeeCalculator must be deployed **before** the TychoRouterV3, as the router requires its address.

1. Define the `ROUTER_FEE_SETTER` address for your network in `scripts/roles.json`. The first
   address receives `ROUTER_FEE_SETTER_ROLE` to manage fee configuration.
2. Define `ROUTER_FEE_RECEIVER` for your network in the same file. The first address is passed to
   the constructor and owns the vault balance every router fee is credited to, so it must be an
   address that can call `withdraw()` on the router. It is a constructor argument because the
   deployment goes through the CREATE2 factory, which can never withdraw.
3. Deploy: `npx hardhat run scripts/deploy-fee-calculator.js --network NETWORK`
4. Note the deployed address — you will need it in the next step.

### Deploy executors

1. In `scripts/deploy-executors.js` define the executors to be deployed
2. Deploy executors: `npx hardhat run scripts/deploy-executors.js --network NETWORK`
3. Fill in the executor addresses in `config/executor_addresses.json`

### Deploy Router

1. Define the accounts to grant roles to in `scripts/roles.json`. For each role, the first address
   receives the role in the constructor.
2. Set environment variables:
   ```
   export FEE_CALCULATOR=<fee-calculator-address-from-previous-step>
   ```
3. Deploy router: `npx hardhat run scripts/deploy-router.js --network NETWORK`
4. Fill in the router address in `config/router_addresses.json`

Via the safe wallet UI:

5. Set the executors addresses
6. Set fee amounts in FeeCalculator
7. Set the pauser wallets

### Publish the deployment

1. Add the complete deployment snapshot under the chain's `scheduled_successor` field. Keep the current deployment active during the notice period.
2. Set the notice publication date, effective date, matching migration deadline, deployment commit, dependency snapshot, and `status: scheduled`.
3. Publish the successor at least 30 days before its effective date. For an urgent material security risk, add a `notice_exception` with `kind: material_security_risk` and a nonempty reason.
4. Treat any change to the router, FeeCalculator, executor set, or pinned dependency configuration as a successor change.
5. At the effective time, move the previous active deployment into the chain's `superseded` list, promote the scheduled successor, and set `scheduled_successor` to `null`. Do not remove history.
6. Update `docs/for-solvers/execution/contract-addresses.md` when scheduling and promoting the successor.

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
