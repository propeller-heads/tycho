require('dotenv').config();
const {ethers} = require("hardhat");
const hre = require("hardhat");
const roles = require("./roles.json");

function resolveRolesNetwork(network) {
    // Strip tenderly_ prefix to match roles.json keys
    const base = network.replace(/^tenderly_/, "");
    if (!roles[base]) {
        throw new Error(
            `No roles defined for network "${base}" in roles.json`
        );
    }
    return roles[base];
}

async function main() {
    const network = hre.network.name;
    // Permit2 is deployed at the same address on all EVM chains
    const permit2 = "0x000000000022D473030F116dDEE9F6B43aC78BA3";
    let feeCalculator = process.env.FEE_CALCULATOR;

    const networkRoles = resolveRolesNetwork(network);
    const unpauser = networkRoles.UNPAUSER_ROLE[0];
    const executorSetter = networkRoles.EXECUTOR_SETTER_ROLE[0];
    const routerFeeSetter = networkRoles.ROUTER_FEE_SETTER[0];

    console.log(`Deploying TychoRouter to ${network} with:`);
    console.log(`- permit2: ${permit2}`);
    console.log(`- feeCalculator: ${feeCalculator}`);
    console.log(`- pauserAdmin: ${unpauser}`);
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

    // Get TychoRouter bytecode with constructor arguments
    const TychoRouter = await ethers.getContractFactory("TychoRouter");
    const deployTx = TychoRouter.getDeployTransaction(
        permit2,
        feeCalculator,
        unpauser,
        unpauser,
        executorSetter,
        routerFeeSetter
    );
    const bytecode = deployTx.data;

    // Use a salt based on network and contract name for deterministic addresses
    const salt = ethers.utils.id(`TychoRouter-${network}`);

    // Compute the address where the contract will be deployed
    // CREATE2 address = keccak256(0xff ++ factory_address ++ salt ++ keccak256(bytecode))[12:]
    const bytecodeHash = ethers.utils.keccak256(bytecode);
    const computedAddress = ethers.utils.getCreate2Address(create2FactoryAddress, salt, bytecodeHash);
    console.log(`TychoRouter will be deployed to: ${computedAddress}`);

    const deploymentData = ethers.utils.concat([salt, bytecode]);
    const tx = await deployer.sendTransaction({
        to: create2FactoryAddress,
        data: deploymentData,
    });
    await tx.wait();
    console.log(`TychoRouter deployed to: ${computedAddress}`);

    // Verify on Tenderly
    try {
        console.log("Verifying contract on Tenderly...");
        await hre.tenderly.verify({
            name: "TychoRouter",
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
            constructorArguments: [
                permit2,
                feeCalculator,
                unpauser,
                unpauser,
                executorSetter,
                routerFeeSetter,
            ],
        });
        console.log(`TychoRouter verified successfully on blockchain explorer!`);
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