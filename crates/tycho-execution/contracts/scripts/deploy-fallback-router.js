require('dotenv').config();
const hre = require("hardhat");
const {deployCreate2} = require("./utils");

// TychoFallbackRouter calls two addresses directly: the PoolManager for the
// Uniswap V4 protocol, the Fluid liquidity layer for what dexCallback pays.
// Both already live in the executor config, keyed by protocol.
const executorDeployments = require("../../config/executor_deployments.json");

async function main() {
    const network = hre.network.name;
    // Strip tenderly_ to match the executor_deployments.json keys.
    const base = network.replace(/^tenderly_/, "");

    const deployments = executorDeployments[base];
    if (!deployments) {
        throw new Error(
            `No executor deployments configured for network '${base}' in ` +
            "executor_deployments.json"
        );
    }
    const poolManager = deployments.uniswap_v4?.args?.[0];
    const fluidLiquidity = deployments.fluid_v1?.args?.[0];
    if (!poolManager || !fluidLiquidity) {
        throw new Error(
            `Network '${base}' needs both uniswap_v4 and fluid_v1 in ` +
            "executor_deployments.json"
        );
    }

    console.log(`Deploying TychoFallbackRouter to ${network} with:`);
    console.log(`- poolManager: ${poolManager}`);
    console.log(`- fluidLiquidity: ${fluidLiquidity}`);

    await deployCreate2({
        contractName: "TychoFallbackRouter",
        contractFqn: "src/fallback/TychoFallbackRouter.sol:TychoFallbackRouter",
        args: [poolManager, fluidLiquidity],
        network,
    });
}

main()
    .then(() => process.exit(0))
    .catch((error) => {
        console.error("Deployment failed:", error);
        process.exit(1);
    });
