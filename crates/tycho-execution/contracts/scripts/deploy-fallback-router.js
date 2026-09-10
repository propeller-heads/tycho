require('dotenv').config();
const {ethers} = require("hardhat");
const hre = require("hardhat");
const {verifyOnExplorer} = require("./utils");

async function main() {
    const network = hre.network.name;

    // TychoFallbackRouter calls these two directly: the PoolManager for the
    // Uniswap V4 venue, the Fluid liquidity layer for what dexCallback pays.
    let poolManager;
    let fluidLiquidity;
    if (network === "ethereum") {
        poolManager = "0x000000000004444c5dc75cB358380D2e3dE08A90";
        fluidLiquidity = "0x52Aa899454998Be5b000Ad077a46Bbe360F4e497";
    } else {
        throw new Error(`Unsupported network: ${network}`);
    }

    console.log(`Deploying TychoFallbackRouter to ${network} with:`);
    console.log(`- poolManager: ${poolManager}`);
    console.log(`- fluidLiquidity: ${fluidLiquidity}`);

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

    const TychoFallbackRouter =
        await ethers.getContractFactory("TychoFallbackRouter");
    const deployTx = TychoFallbackRouter.getDeployTransaction(
        poolManager,
        fluidLiquidity
    );
    const bytecode = deployTx.data;

    const salt = ethers.utils.id(`TychoFallbackRouter-${network}`);

    const bytecodeHash = ethers.utils.keccak256(bytecode);
    const computedAddress = ethers.utils.getCreate2Address(
        create2FactoryAddress,
        salt,
        bytecodeHash
    );
    console.log(
        `TychoFallbackRouter will be deployed to: ${computedAddress}`
    );

    // The address is derived from the bytecode and the constructor arguments,
    // so an existing contract there is this exact build. Skipping the
    // deployment makes the script re-runnable, which matters when verification
    // has to be retried.
    const deployed =
        (await ethers.provider.getCode(computedAddress)) !== "0x";
    if (deployed) {
        console.log(
            "TychoFallbackRouter already deployed, skipping deployment"
        );
    } else {
        const deploymentData = ethers.utils.concat([salt, bytecode]);
        const tx = await deployer.sendTransaction({
            to: create2FactoryAddress,
            data: deploymentData,
            gasLimit: 3_000_000,
        });
        await tx.wait();
        console.log(`TychoFallbackRouter deployed to: ${computedAddress}`);
    }

    // Verify on Tenderly
    try {
        await hre.tenderly.verify({
            name: "TychoFallbackRouter",
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
            contractFqn:
                "src/fallback/TychoFallbackRouter.sol:TychoFallbackRouter",
            constructorArgs: [poolManager, fluidLiquidity],
        });
        console.log(
            "TychoFallbackRouter verified successfully on blockchain explorer!"
        );
    } catch (error) {
        console.error(
            `Error during blockchain explorer verification:`,
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
