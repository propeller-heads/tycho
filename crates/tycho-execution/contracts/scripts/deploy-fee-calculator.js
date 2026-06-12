require('dotenv').config();
const {ethers} = require("hardhat");
const hre = require("hardhat");

async function main() {
    const network = hre.network.name;

    // The routerFeeSetter is the address that will be granted
    // ROUTER_FEE_SETTER_ROLE to manage fee configuration.
    const routerFeeSetter = process.env.ROUTER_FEE_SETTER;
    if (!routerFeeSetter) {
        throw new Error(
            "ROUTER_FEE_SETTER env var is required"
        );
    }

    console.log(`Deploying FeeCalculator to ${network} with:`);
    console.log(`- routerFeeSetter: ${routerFeeSetter}`);

    const [deployer] = await ethers.getSigners();
    console.log(`Deploying with account: ${deployer.address}`);
    console.log(
        `Account balance: ${ethers.utils.formatEther(await deployer.getBalance())} ETH`
    );

    // Deterministic Deployment Proxy
    // More info: https://getfoundry.sh/guides/deterministic-deployments-using-create2/
    const create2FactoryAddress =
        "0x4e59b44847b379578588920cA78FbF26c0B4956C";
    console.log(`Using CREATE2 factory at: ${create2FactoryAddress}`);

    const FeeCalculator =
        await ethers.getContractFactory("FeeCalculator");
    const deployTx =
        FeeCalculator.getDeployTransaction(routerFeeSetter);
    const bytecode = deployTx.data;

    const salt = ethers.utils.id(`FeeCalculator-${network}`);

    const bytecodeHash = ethers.utils.keccak256(bytecode);
    const computedAddress = ethers.utils.getCreate2Address(
        create2FactoryAddress,
        salt,
        bytecodeHash
    );
    console.log(`FeeCalculator will be deployed to: ${computedAddress}`);

    const deploymentData = ethers.utils.concat([salt, bytecode]);
    const tx = await deployer.sendTransaction({
        to: create2FactoryAddress,
        data: deploymentData,
        gasLimit: 3_000_000,
    });
    await tx.wait();
    console.log(`FeeCalculator deployed to: ${computedAddress}`);

    // Verify on Tenderly
    try {
        await hre.tenderly.verify({
            name: "FeeCalculator",
            address: computedAddress,
        });
        console.log("Contract verified successfully on Tenderly");
    } catch (error) {
        console.error("Error during contract verification:", error);
    }

    console.log(
        "Waiting for 1 minute before verifying the contract..."
    );
    await new Promise(resolve => setTimeout(resolve, 60000));

    // Verify on Etherscan
    try {
        await hre.run("verify:verify", {
            address: computedAddress,
            constructorArguments: [routerFeeSetter],
        });
        console.log(
            "FeeCalculator verified successfully on blockchain explorer!"
        );
    } catch (error) {
        console.error(
            "Error during blockchain explorer verification:",
            error
        );
    }
}

main()
    .then(() => process.exit(0))
    .catch((error) => {
        console.error("Deployment failed:", error);
        process.exit(1);
    });
