require('dotenv').config();
const {ethers} = require("hardhat");
const hre = require("hardhat");
const {resolveRolesNetwork, verifyOnExplorer} = require("./utils");

// Fee units: 100_000_000 = 100%.
const ROUTER_FEE_ON_CLIENT_FEE = 20_000_000;

// Client addresses stay out of the repository: fill these in locally before a
// deployment.
const SLIPPAGE_EXEMPT_CLIENTS = [];

// Keyed by network name without the tenderly_ prefix.
const SLIPPAGE_EXEMPT_CLIENTS_BY_NETWORK = {};

const CLIENT_FEE_EXEMPT_CLIENTS = [];

// A load-balanced RPC can serve reads from a node that has not yet seen the
// latest mined block, so a read right after tx.wait() can return old state.
async function waitUntil(isDone, failureMessage) {
    for (let attempt = 1; attempt <= 30; attempt++) {
        if (await isDone()) return;
        await new Promise(resolve => setTimeout(resolve, 2000));
    }
    throw new Error(`${failureMessage} after 60s`);
}

async function sendAndWait(description, txPromise) {
    const tx = await txPromise;
    await tx.wait();
    console.log(`- ${description} (tx ${tx.hash})`);
}

// The constructor already sets the router positive slippage to 100%.
async function setInitialFees(feeCalculator, network) {
    const base = network.replace(/^tenderly_/, "");
    const slippageExempt = [
        ...SLIPPAGE_EXEMPT_CLIENTS,
        ...(SLIPPAGE_EXEMPT_CLIENTS_BY_NETWORK[base] ?? []),
    ];

    console.log("Setting initial fees:");
    await sendAndWait(
        `router fee on client fee = ${ROUTER_FEE_ON_CLIENT_FEE}`,
        feeCalculator.setRouterFeeOnClientFee(ROUTER_FEE_ON_CLIENT_FEE)
    );
    for (const client of slippageExempt) {
        await sendAndWait(
            `router positive slippage for ${client} = 0`,
            feeCalculator.setCustomRouterPositiveSlippageBps(client, 0)
        );
    }
    for (const client of CLIENT_FEE_EXEMPT_CLIENTS) {
        await sendAndWait(
            `router fee on client fee for ${client} = 0`,
            feeCalculator.setCustomRouterFeeOnClientFee(client, 0)
        );
    }
}

// The deployer holds ROUTER_FEE_SETTER_ROLE only until the initial fees are
// set. Then the role moves to the fee setter from roles.json.
async function configureAndHandOver(feeCalculator, deployer, routerFeeSetter, network) {
    const role = await feeCalculator.ROUTER_FEE_SETTER_ROLE();
    if (!(await feeCalculator.hasRole(role, deployer.address))) {
        console.log("Deployer no longer holds ROUTER_FEE_SETTER_ROLE, skipping fee setup");
        return;
    }

    await setInitialFees(feeCalculator, network);

    console.log("Handing over ROUTER_FEE_SETTER_ROLE:");
    await sendAndWait(
        `grant role to ${routerFeeSetter}`,
        feeCalculator.grantRole(role, routerFeeSetter)
    );
    await sendAndWait(
        `renounce role of deployer ${deployer.address}`,
        feeCalculator.renounceRole(role, deployer.address)
    );

    await waitUntil(
        () => feeCalculator.hasRole(role, routerFeeSetter),
        `Fee setter ${routerFeeSetter} does not hold ROUTER_FEE_SETTER_ROLE`
    );
    await waitUntil(
        async () => !(await feeCalculator.hasRole(role, deployer.address)),
        `Deployer ${deployer.address} still holds ROUTER_FEE_SETTER_ROLE`
    );
}

async function main() {
    const network = hre.network.name;

    // The routerFeeSetter is the address that will hold
    // ROUTER_FEE_SETTER_ROLE to manage fee configuration.
    const networkRoles = resolveRolesNetwork(network);
    const routerFeeSetter = networkRoles.ROUTER_FEE_SETTER[0];
    // The routerFeeReceiver owns the vault balance every router fee is credited
    // to. It must be an address that can call withdraw() on the router — the
    // CREATE2 factory below deploys the contract but cannot withdraw, which is
    // why the receiver is a constructor argument rather than the deployer.
    const routerFeeReceiver = networkRoles.ROUTER_FEE_RECEIVER[0];

    console.log(`Deploying FeeCalculator to ${network} with:`);
    console.log(`- routerFeeSetter: ${routerFeeSetter}`);
    console.log(`- routerFeeReceiver: ${routerFeeReceiver}`);

    const [deployer] = await ethers.getSigners();
    console.log(`Deploying with account: ${deployer.address}`);
    if (deployer.address.toLowerCase() === routerFeeSetter.toLowerCase()) {
        throw new Error("The deployer must not be the fee setter: it renounces the role");
    }
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
        FeeCalculator.getDeployTransaction(deployer.address, routerFeeReceiver);
    const bytecode = deployTx.data;

    const salt = ethers.utils.id(`FeeCalculator-${network}`);

    const bytecodeHash = ethers.utils.keccak256(bytecode);
    const computedAddress = ethers.utils.getCreate2Address(
        create2FactoryAddress,
        salt,
        bytecodeHash
    );
    console.log(`FeeCalculator will be deployed to: ${computedAddress}`);

    // The address is derived from the bytecode, so an existing contract there is
    // this exact build. Skipping the deployment makes the script re-runnable,
    // which matters when verification has to be retried.
    const deployed =
        (await ethers.provider.getCode(computedAddress)) !== "0x";
    if (deployed) {
        console.log("FeeCalculator already deployed, skipping deployment");
    } else {
        const deploymentData = ethers.utils.concat([salt, bytecode]);
        const tx = await deployer.sendTransaction({
            to: create2FactoryAddress,
            data: deploymentData,
            gasLimit: 3_000_000,
        });
        await tx.wait();
        console.log(`FeeCalculator deployed to: ${computedAddress}`);
        await waitUntil(
            async () => (await ethers.provider.getCode(computedAddress)) !== "0x",
            `No code at ${computedAddress}`
        );
    }

    const feeCalculator = FeeCalculator.attach(computedAddress).connect(deployer);
    await configureAndHandOver(feeCalculator, deployer, routerFeeSetter, network);

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

    if (!deployed) {
        console.log(
            "Waiting for 1 minute before verifying the contract..."
        );
        await new Promise(resolve => setTimeout(resolve, 60000));
    }

    // Verify on the block explorer
    try {
        await verifyOnExplorer({
            network,
            address: computedAddress,
            contractFqn: "src/FeeCalculator.sol:FeeCalculator",
            constructorArgs: [deployer.address, routerFeeReceiver],
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
