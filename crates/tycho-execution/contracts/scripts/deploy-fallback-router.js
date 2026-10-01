require('dotenv').config();
const hre = require("hardhat");
const {deployCreate2} = require("./utils");

// Deploys the fallback router named by FALLBACK_ROUTER: `propamm` (the default),
// `metric`, `bebop` or `hashflow`.
//
// Every fallback router takes three per-chain singletons: Uniswap V4's PoolManager,
// Fluid's liquidity layer and the Uniswap V3 static quoter. The first two come
// from the chain's executor deployments, the quoter from STATIC_QUOTERS below.
// A missing one is deployed as address(0): the protocol reverts
// ProtocolUnavailable, or Uniswap V3 is quoted by simulation.
// `SUPPORTED_PROTOCOLS` in the Rust encoder must agree with what this deploys;
// its tests check that against executor_deployments.json.
//
// MetricFallbackRouter also takes Metric's swap quoter, from METRIC_SWAP_QUOTERS.
// BebopFallbackRouter also takes the Bebop settlement and router, from the chain's
// `rfq:bebop` entry. HashflowFallbackRouter also takes the Hashflow router, from the
// chain's `rfq:hashflow` entry.
//
// Then deploy the matching executor with deploy-executors.js: add an entry with
// the printed router address to executor_deployments.json (`fallback` for
// PropAMMFallbackExecutor, `fallback:rfq:metric` for MetricFallbackExecutor,
// `fallback:rfq:bebop` for BebopFallbackExecutor, `fallback:rfq:hashflow` for
// HashflowFallbackExecutor). A Bebop or Hashflow router also needs its address as
// `fallback_router` under its protocol in protocol_specific_addresses.json.
const executorDeployments = require("../../config/executor_deployments.json");

const ZERO_ADDRESS = "0x0000000000000000000000000000000000000000";

// Metric's MetricOmmSwapQuoter, per chain. It must belong to the factory that
// created the pools the Metric API serves: on Base that is factory
// 0x622911384e7973439b8be305f5e3Fc3c5736EDe4.
const METRIC_SWAP_QUOTERS = {
    base: "0xaB6C48D981B943F62A23bb4EB2db125182E6753c",
};

const ROUTERS = {
    propamm: "PropAMMFallbackRouter",
    metric: "MetricFallbackRouter",
    bebop: "BebopFallbackRouter",
    hashflow: "HashflowFallbackRouter",
};

// Eden Network's Uniswap V3 static quoter, per chain:
// https://github.com/eden-network/uniswap-v3-static-quoter#deployments
const STATIC_QUOTERS = {
    ethereum: "0xc80f61d1bdAbD8f5285117e1558fDDf8C64870FE",
    base: "0x28aF629a9F3ECE3c8D9F0b7cCf6349708CeC8cFb",
};

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
    const poolManager = deployments.uniswap_v4?.args?.[0] ?? ZERO_ADDRESS;
    const fluidLiquidity = deployments.fluid_v1?.args?.[0] ?? ZERO_ADDRESS;
    const staticQuoter = STATIC_QUOTERS[base] ?? ZERO_ADDRESS;

    const kind = process.env.FALLBACK_ROUTER ?? "propamm";
    const contractName = ROUTERS[kind];
    if (!contractName) {
        throw new Error(
            `Unknown FALLBACK_ROUTER '${kind}': use one of ` +
            Object.keys(ROUTERS).join(", ")
        );
    }
    const args = [poolManager, fluidLiquidity, staticQuoter];
    if (kind === "metric") {
        const metricQuoter = METRIC_SWAP_QUOTERS[base];
        if (!metricQuoter) {
            throw new Error(
                `No Metric swap quoter for network '${base}': add it to ` +
                "METRIC_SWAP_QUOTERS"
            );
        }
        args.push(metricQuoter);
    }
    if (kind === "bebop") {
        const bebopArgs = deployments["rfq:bebop"]?.args;
        if (!bebopArgs) {
            throw new Error(
                `No rfq:bebop entry for network '${base}' in ` +
                "executor_deployments.json: the Bebop settlement and router are needed"
            );
        }
        args.push(...bebopArgs);
    }
    if (kind === "hashflow") {
        const hashflowRouter = deployments["rfq:hashflow"]?.args?.[0];
        if (!hashflowRouter) {
            throw new Error(
                `No rfq:hashflow entry for network '${base}' in ` +
                "executor_deployments.json: the Hashflow router is needed"
            );
        }
        args.push(hashflowRouter);
    }

    console.log(`Deploying ${contractName} to ${network} with:`);
    console.log(
        `- poolManager: ${describe(poolManager, "Uniswap V4 disabled")}`
    );
    console.log(
        `- fluidLiquidity: ${describe(fluidLiquidity, "Fluid V1 disabled")}`
    );
    console.log(
        `- uniswapV3StaticQuoter: ${describe(
            staticQuoter,
            "Uniswap V3 quoted by simulation"
        )}`
    );

    if (kind === "metric") {
        console.log(`- metricQuoter: ${args[3]}`);
    }
    if (kind === "bebop") {
        console.log(`- bebopSettlement: ${args[3]}`);
        console.log(`- bebopRouter: ${args[4]}`);
    }
    if (kind === "hashflow") {
        console.log(`- hashflowRouter: ${args[3]}`);
    }

    await deployCreate2({
        contractName,
        contractFqn: `src/fallback/${contractName}.sol:${contractName}`,
        args,
        network,
    });
}

function describe(address, whenZero) {
    return address === ZERO_ADDRESS
        ? `${address} (${whenZero}: not configured for this network)`
        : address;
}

main()
    .then(() => process.exit(0))
    .catch((error) => {
        console.error("Deployment failed:", error);
        process.exit(1);
    });
