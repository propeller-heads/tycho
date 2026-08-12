require('dotenv').config();
const {ethers} = require("hardhat");
const hre = require("hardhat");

// Constructor args for each executor live in the shared config.
// See config/executor_deployments.json.
const executorDeployments = require("../../config/executor_deployments.json");

// Which protocols to deploy per network. Comment out the protocols you
// don't want to deploy.
const deploy_protocols = {
    "ethereum": [
        "uniswap_v2",
        "ring_swap_v2",
        "pancakeswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "vm:balancer_v2",
        "ekubo_v2",
        "vm:curve",
        "vm:maverick_v2",
        "vm:balancer_v3",
        "rfq:bebop",
        "rfq:hashflow",
        "fluid_v1",
        "erc4626",
        "rocketpool",
        "ekubo_v3",
        "native_wrapper",
        "rfq:liquorice",
        "vm:fermiswap",
        "vm:bopamm",
        "rfq:metric",
        "sky",
    ],
    "base": [
        "uniswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "rfq:bebop",
        "aerodrome_slipstreams",
        "aerodrome_v1",
        "native_wrapper",
        "lunarbase",
        "rfq:metric",
    ],
    "unichain": [
        "uniswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "vm:curve",
        "velodrome_slipstreams",
        "native_wrapper",
    ],
    "arbitrum": [
        "uniswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "native_wrapper",
        "rfq:metric",
    ],
    "polygon": [
        "uniswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "native_wrapper",
        "rfq:metric",
    ],
    "bsc": [
        "uniswap_v2",
        "pancakeswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "native_wrapper",
        "rfq:metric",
    ],
    "plasma": [
        "uniswap_v3",
        "fluid_v1",
        "vm:curve",
        "native_wrapper",
    ],
    "robinhood": [
        "uniswap_v2",
        "uniswap_v3",
        "uniswap_v4",
        "native_wrapper",
    ],
};

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

    const protocols = deploy_protocols[network];
    if (!protocols) {
        throw new Error(`No deploy protocols configured for network: ${network}`);
    }
    const networkDeployments = executorDeployments[network];
    if (!networkDeployments) {
        throw new Error(`No executor deployments configured for network '${network}' in executor_deployments.json`);
    }

    for (const protocol of protocols) {
        const deployment = networkDeployments[protocol];
        if (!deployment) {
            throw new Error(
                `No deployment config for protocol '${protocol}' on network '${network}' in executor_deployments.json`
            );
        }
        const {contract: contractName, args} = deployment;
        const Executor = await ethers.getContractFactory(contractName);

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

        const deploymentData = ethers.utils.concat([salt, bytecode]);
        const tx = await deployer.sendTransaction({
            to: create2FactoryAddress,
            data: deploymentData,
        });
        await tx.wait();
        console.log(`${contractName} deployed to: ${computedAddress}`);

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

        console.log("Waiting for 1 minute before verifying the contract...");
        await new Promise(resolve => setTimeout(resolve, 60000));
        // Verify on Etherscan
        try {
            await hre.run("verify:verify", {
                address: computedAddress,
                constructorArguments: args,
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

module.exports = {deploy_protocols, executorDeployments};
