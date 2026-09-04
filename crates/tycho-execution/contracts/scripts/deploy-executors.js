require('dotenv').config();
const {ethers} = require("hardhat");
const hre = require("hardhat");
const {verifyOnExplorer} = require("./utils");

// Constructor args for each executor live in the shared config.
// See config/executor_deployments.json.
const executorDeployments = require("../../config/executor_deployments.json");

// Deploys every protocol configured for the network in executor_deployments.json. Set
// EXECUTORS to a comma-separated list of protocol names to deploy only those, e.g.
// EXECUTORS=uniswap_v2,rfq:bebop
function protocolsToDeploy(network, networkDeployments) {
    const configured = Object.keys(networkDeployments);
    if (!process.env.EXECUTORS) {
        return configured;
    }
    const requested = process.env.EXECUTORS.split(",");
    const unknown = requested.filter(name => !configured.includes(name));
    if (unknown.length > 0) {
        throw new Error(
            `EXECUTORS names ${unknown.join(", ")} are not configured for network '${network}' in executor_deployments.json`
        );
    }
    return requested;
}

async function main() {
    const network = hre.network.name;
    console.log(`Deploying executors to ${network}`);

    const [deployer] = await ethers.getSigners();
    console.log(`Deploying with account: ${deployer.address}`);
    console.log(`Account balance: ${ethers.utils.formatEther(await deployer.getBalance())} ETH`);

    // Deterministic Deployment Proxy
    // More info: https://getfoundry.sh/guides/deterministic-deployments-using-create2/
    const create2FactoryAddress = "0x4e59b44847b379578588920cA78FbF26c0B4956C";
    console.log(`Using CREATE2 factory at: ${create2FactoryAddress}`);

    const networkDeployments = executorDeployments[network];
    if (!networkDeployments) {
        throw new Error(`No executor deployments configured for network '${network}' in executor_deployments.json`);
    }
    const protocols = protocolsToDeploy(network, networkDeployments);
    console.log(`Deploying ${protocols.length} executors: ${protocols.join(", ")}`);

    for (const protocol of protocols) {
        const {contract: contractName, args} = networkDeployments[protocol];
        const Executor = await ethers.getContractFactory(contractName);
        // The Blockscout verification path needs the fully qualified name.
        const {sourceName} = await hre.artifacts.readArtifact(contractName);

        // Get bytecode with constructor arguments
        const deployTx = Executor.getDeployTransaction(...args);
        const bytecode = deployTx.data;

        // Use a salt that includes network and executor name
        const salt = ethers.utils.id(`${contractName}-${network}`);

        // Compute the address where the contract will be deployed
        // CREATE2 address = keccak256(0xff ++ factory_address ++ salt ++ keccak256(bytecode))[12:]
        const bytecodeHash = ethers.utils.keccak256(bytecode);
        const computedAddress = ethers.utils.getCreate2Address(create2FactoryAddress, salt, bytecodeHash);
        console.log(`${contractName} (${protocol}) will be deployed to: ${computedAddress}`);

        // The address is derived from the bytecode and the constructor
        // arguments, so an existing contract there is this exact build.
        // Skipping the deployment makes the script re-runnable, which matters
        // when verification has to be retried.
        const deployed =
            (await ethers.provider.getCode(computedAddress)) !== "0x";
        if (deployed) {
            console.log(`${contractName} already deployed, skipping deployment`);
        } else {
            const deploymentData = ethers.utils.concat([salt, bytecode]);
            const tx = await deployer.sendTransaction({
                to: create2FactoryAddress,
                data: deploymentData,
            });
            await tx.wait();
            console.log(`${contractName} deployed to: ${computedAddress}`);
        }

        // Verify on Tenderly
        try {
            await hre.tenderly.verify({
                name: contractName,
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
                contractFqn: `${sourceName}:${contractName}`,
                constructorArgs: args,
            });
            console.log(`${contractName} verified successfully on blockchain explorer!`);
        } catch (error) {
            console.error(`Error during blockchain explorer verification:`, error);
        }
    }
}

if (require.main === module) {
    main()
        .then(() => process.exit(0))
        .catch((error) => {
            console.error("Deployment failed:", error);
            process.exit(1);
        });
}

module.exports = {executorDeployments};
