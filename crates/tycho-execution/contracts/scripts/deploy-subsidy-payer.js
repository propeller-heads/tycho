require('dotenv').config();
const hre = require("hardhat");
const {deployCreate2, resolveRolesNetwork} = require("./utils");

// Deploys the SubsidyPayer for the chain's TychoRouter. Only that router can
// call `pay`.
//
// Then deploy TychoSubsidizingExecutor with deploy-executors.js: set the
// printed payer address as the `subsidy` entry's argument in
// executor_deployments.json. The executor constructor rejects a payer address
// with no code.
const routerAddresses = require("../../config/router_addresses.json");

async function main() {
    const network = hre.network.name;
    // Strip tenderly_ to match the router_addresses.json keys.
    const base = network.replace(/^tenderly_/, "");

    const router = routerAddresses[base];
    if (!router) {
        throw new Error(
            `No router configured for network '${base}' in router_addresses.json`
        );
    }

    const networkRoles = resolveRolesNetwork(network);
    const signer = networkRoles.SUBSIDY_SIGNER[0];
    const roles = {
        signerSetter: networkRoles.EXECUTOR_SETTER_ROLE[0],
        budgetManager: networkRoles.EXECUTOR_SETTER_ROLE[0],
        pauserAdmin: networkRoles.UNPAUSER_ROLE[0],
        unpauser: networkRoles.UNPAUSER_ROLE[0],
        pausers: networkRoles.PAUSER_ROLE,
    };

    console.log(`Deploying SubsidyPayer to ${network} with:`);
    console.log(`- router: ${router}`);
    console.log(`- signer: ${signer}`);
    console.log(`- signerSetter: ${roles.signerSetter}`);
    console.log(`- budgetManager: ${roles.budgetManager}`);
    console.log(`- pauserAdmin: ${roles.pauserAdmin}`);
    console.log(`- unpauser: ${roles.unpauser}`);
    console.log(`- pausers: ${roles.pausers.join(", ")}`);

    const address = await deployCreate2({
        contractName: "SubsidyPayer",
        contractFqn: "src/SubsidyPayer.sol:SubsidyPayer",
        args: [router, signer, roles],
        network,
    });

    console.log(
        `\nSet this in executor_deployments.json under "${base}":\n` +
        `"subsidy": { "contract": "TychoSubsidizingExecutor", ` +
        `"args": ["${address}"] }`
    );
}

main()
    .then(() => process.exit(0))
    .catch((error) => {
        console.error("Deployment failed:", error);
        process.exit(1);
    });
