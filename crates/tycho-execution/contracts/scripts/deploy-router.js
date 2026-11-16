require('dotenv').config();
const {ethers} = require("hardhat");
const hre = require("hardhat");
const {resolveRolesNetwork, verifyOnExplorer} = require("./utils");
const executorAddresses = require("../../config/executor_addresses.json");
const deploymentRegistry = require("../../config/deployment_registry.json");

// The executors the router approves at deployment, without the timelock that
// applies to executors added later.
function resolveExecutors(network) {
    // Strip tenderly_ to match the executor_addresses.json keys.
    const base = network.replace(/^tenderly_/, "");
    const byProtocol = executorAddresses[base];
    if (!byProtocol) {
        throw new Error(
            `No executors defined for network "${base}" in ` +
            "config/executor_addresses.json"
        );
    }
    // Several protocols share one executor, and the router rejects duplicates.
    // Sorting keeps the constructor arguments, and so the CREATE2 address,
    // stable for the same set of executors.
    const unique = new Set(
        Object.values(byProtocol).map((address) => ethers.utils.getAddress(address))
    );
    return [...unique].sort();
}

async function assertDeployed(addresses) {
    const missing = [];
    for (const address of addresses) {
        if ((await ethers.provider.getCode(address)) === "0x") {
            missing.push(address);
        }
    }
    if (missing.length > 0) {
        throw new Error(
            `Executors without code on this network: ${missing.join(", ")}. ` +
            "Deploy them with deploy-executors.js first, or fix " +
            "config/executor_addresses.json."
        );
    }
}

async function main() {
    const network = hre.network.name;
    // Permit2 is deployed at the same address on all EVM chains
    const permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3";
    // FEE_CALCULATOR overrides the chain's active FeeCalculator in
    // config/deployment_registry.json.
    const registryChain = deploymentRegistry.chains[network.replace(/^tenderly_/, "")];
    const feeCalculator =
        process.env.FEE_CALCULATOR || registryChain?.fee_calculator?.address;
    if (!feeCalculator) {
        throw new Error(
            `No FeeCalculator for network "${network}": set FEE_CALCULATOR or add it to deployment_registry.json`
        );
    }

    const networkRoles = resolveRolesNetwork(network);
    const unpauser = networkRoles.UNPAUSER_ROLE[0];
    const pausers = networkRoles.PAUSER_ROLE;
    if (!pausers || pausers.length === 0) {
        throw new Error(`No PAUSER_ROLE wallets for network "${network}" in roles.json`);
    }
    const executorSetter = networkRoles.EXECUTOR_SETTER_ROLE[0];
    const routerFeeSetter = networkRoles.ROUTER_FEE_SETTER[0];
    const executors = resolveExecutors(network);
    await assertDeployed(executors);

    console.log(`Deploying TychoRouterV3 to ${network} with:`);
    console.log(`- permit2: ${permit2}`);
    console.log(`- feeCalculator: ${feeCalculator}`);
    console.log(`- executors (${executors.length}):`);
    executors.forEach((executor) => console.log(`    ${executor}`));
    console.log(`- pausers (${pausers.length}):`);
    pausers.forEach((pauser) => console.log(`    ${pauser}`));
    console.log(`- unpauserAdmin: ${unpauser}`);
    console.log(`- executorSetterAdmin: ${executorSetter}`);
    console.log(`- routerFeeSetterAdmin: ${routerFeeSetter}`);

    const [deployer] = await ethers.getSigners();
    console.log(`Deploying with account: ${deployer.address}`);
    console.log(`Account balance: ${ethers.utils.formatEther(await deployer.getBalance())} ETH`);

    // Deterministic Deployment Proxy
    // More info: https://getfoundry.sh/guides/deterministic-deployments-using-create2/
    const create2FactoryAddress = "0x4e59b44847b379578588920cA78FbF26c0B4956C";
    console.log(`Using CREATE2 factory at: ${create2FactoryAddress}`);

    // Get TychoRouterV3 bytecode with constructor arguments
    const TychoRouterV3 = await ethers.getContractFactory("TychoRouterV3");
    const deployTx = TychoRouterV3.getDeployTransaction(
        permit2,
        feeCalculator,
        executors,
        pausers,
        unpauser,
        executorSetter,
        routerFeeSetter
    );
    const bytecode = deployTx.data;

    // Use a salt based on network and contract name for deterministic addresses
    const salt = ethers.utils.id(`TychoRouterV3-${network}`);

    // Compute the address where the contract will be deployed
    // CREATE2 address = keccak256(0xff ++ factory_address ++ salt ++ keccak256(bytecode))[12:]
    const bytecodeHash = ethers.utils.keccak256(bytecode);
    const computedAddress = ethers.utils.getCreate2Address(create2FactoryAddress, salt, bytecodeHash);
    console.log(`TychoRouterV3 will be deployed to: ${computedAddress}`);

    // The address is derived from the bytecode and the constructor arguments, so
    // an existing contract there is this exact build. Skipping the deployment
    // makes the script re-runnable, which matters when verification has to be
    // retried.
    const deployed =
        (await ethers.provider.getCode(computedAddress)) !== "0x";
    if (deployed) {
        console.log("TychoRouterV3 already deployed, skipping deployment");
    } else {
        const deploymentData = ethers.utils.concat([salt, bytecode]);
        const tx = await deployer.sendTransaction({
            to: create2FactoryAddress,
            data: deploymentData,
        });
        await tx.wait();
        console.log(`TychoRouterV3 deployed to: ${computedAddress}`);
    }

    // Verify on Tenderly
    try {
        console.log("Verifying contract on Tenderly...");
        await hre.tenderly.verify({
            name: "TychoRouterV3",
            address: computedAddress,
        });
        console.log("Contract verified successfully on Tenderly");
    } catch (error) {
        console.error("Error during contract verification:", error);
    }

    if (!deployed) {
        console.log("Waiting for 1 minute before verifying the contract...");
        await new Promise(resolve => setTimeout(resolve, 60000));
    }

    // Verify on the block explorer
    try {
        await verifyOnExplorer({
            network,
            address: computedAddress,
            contractFqn: "src/TychoRouterV3.sol:TychoRouterV3",
            constructorArgs: [
                permit2,
                feeCalculator,
                executors,
                pausers,
                unpauser,
                executorSetter,
                routerFeeSetter,
            ],
        });
        console.log(`TychoRouterV3 verified successfully on blockchain explorer!`);
    } catch (error) {
        console.error(`Error during blockchain explorer verification:`, error);
    }

}

main()
    .then(() => process.exit(0))
    .catch((error) => {
        console.error("Deployment failed:", error);
        process.exit(1);
    });