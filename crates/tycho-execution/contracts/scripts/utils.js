const {ethers} = require("hardhat");
const hre = require("hardhat");
const Safe = require('@safe-global/protocol-kit').default;
const {EthersAdapter} = require('@safe-global/protocol-kit');
const {default: SafeApiKit} = require("@safe-global/api-kit");
const fs = require("fs");
const path = require("path");
const roles = require("./roles.json");

// Chains whose explorer is a Blockscout instance rather than an Etherscan one.
// Verification goes through Blockscout's native v2 API because hardhat-verify
// and forge verify-contract cannot set a browser User-Agent, and the instance
// sits behind Cloudflare (see USER_AGENT). No API key is required.
const BLOCKSCOUT_NETWORKS = {
    robinhood: {
        chainId: 4663,
        apiBase: "https://robinhoodchain.blockscout.com/api/v2",
        browserUrl: "https://robinhoodchain.blockscout.com/",
    },
};

// Cloudflare answers 403 to requests carrying a curl or node User-Agent.
const USER_AGENT =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 " +
    "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

async function blockscoutGet(url) {
    const res = await fetch(url, {
        headers: {"user-agent": USER_AGENT, accept: "application/json"},
    });
    // Cloudflare answers with an HTML challenge page rather than JSON.
    const text = await res.text();
    try {
        return {ok: res.ok, body: JSON.parse(text)};
    } catch {
        return {ok: false, body: null};
    }
}

function artifactPath(contractsDir, contractFqn, extension) {
    const [sourceName, contractName] = contractFqn.split(":");
    return path.join(
        contractsDir,
        "artifacts",
        sourceName,
        `${contractName}${extension}`
    );
}

/** ABI-encode constructor arguments using the artifact's own input types. */
function encodeConstructorArgs(contractsDir, contractFqn, constructorArgs) {
    const artifact = JSON.parse(
        fs.readFileSync(artifactPath(contractsDir, contractFqn, ".json"), "utf8")
    );
    const constructor = artifact.abi.find(
        (entry) => entry.type === "constructor"
    );
    const types = (constructor?.inputs || []).map((input) => input.type);
    return ethers.utils.defaultAbiCoder
        .encode(types, constructorArgs)
        .replace(/^0x/, "");
}

/**
 * Load the exact standard JSON input that produced the deployed artifact.
 *
 * The contract's .dbg.json names its build-info file, so this always matches
 * the compile the deployment bytecode came from. Do not search build-info by
 * contract name: `IFeeCalculator.sol` and `FeeCalculator.sol` both match a
 * suffix test, and stale build-info files from earlier compiles linger.
 */
function getHardhatStandardJsonInput(contractsDir, contractFqn) {
    const dbgPath = artifactPath(contractsDir, contractFqn, ".dbg.json");
    if (!fs.existsSync(dbgPath)) {
        throw new Error(
            `No Hardhat artifact at ${dbgPath}. ` +
            "Run `npx hardhat compile` first."
        );
    }

    const {buildInfo} = JSON.parse(fs.readFileSync(dbgPath, "utf8"));
    const buildInfoPath = path.resolve(path.dirname(dbgPath), buildInfo);
    const info = JSON.parse(fs.readFileSync(buildInfoPath, "utf8"));

    return {
        standardJson: JSON.stringify(info.input),
        compilerVersion: `v${info.solcLongVersion}`,
    };
}

/**
 * Blockscout's verification endpoints answer HTTP 500 for any address it does
 * not hold as a contract, so check the two reasons that happens up front.
 *
 * @returns {boolean} whether Blockscout already holds the contract as verified.
 *     Submitting again for such an address is rejected, and deploy scripts are
 *     meant to be re-runnable.
 */
async function checkVerifiable(config, address) {
    if ((await ethers.provider.getCode(address)) === "0x") {
        throw new Error(
            `No contract deployed at ${address}. The address is derived from ` +
            "the current artifact bytecode, so a recompile since deployment " +
            "moves it. Check out the deployed commit and recompile."
        );
    }

    const {ok, body} = await blockscoutGet(
        `${config.apiBase}/addresses/${address}`
    );
    if (ok && body.is_contract === false) {
        throw new Error(
            `Blockscout has not indexed ${address} as a contract yet, and it ` +
            "rejects verification for unknown addresses. CREATE2 deployments " +
            "reach Blockscout through internal transactions, which this " +
            "instance indexes behind the chain head. Check " +
            `${config.browserUrl}address/${address} and retry once the ` +
            "contract creation shows up."
        );
    }

    return ok && body.is_verified === true;
}

/**
 * Wait for the submitted verification to land.
 *
 * `is_verified` is the only signal available: Blockscout's v2 smart-contract
 * response carries no verification status field, and it pushes the outcome of a
 * submission over a websocket rather than over HTTP. A rejected verification is
 * therefore indistinguishable from a slow queue, and surfaces as this timeout.
 */
async function pollBlockscoutVerification(config, address) {
    const url = `${config.apiBase}/smart-contracts/${address}`;

    for (let attempt = 0; attempt < 30; attempt++) {
        const {ok, body} = await blockscoutGet(url);
        if (ok && body.is_verified) {
            return;
        }
        await new Promise(resolve => setTimeout(resolve, 5000));
    }
    throw new Error(
        "Timed out waiting for Blockscout verification. It reports the reason " +
        "for a rejected submission only in its UI: see " +
        `${config.browserUrl}address/${address}#code`
    );
}

/**
 * Verify on a Blockscout instance via its native v2 standard-input API.
 * Compiler settings come from the artifact's own build-info, so they match the
 * deployment by construction.
 */
async function verifyOnBlockscout({network, address, contractFqn, constructorArgs}) {
    const config = BLOCKSCOUT_NETWORKS[network];
    if (!config) {
        throw new Error(`No Blockscout config for network "${network}"`);
    }

    console.log(`Verifying on Blockscout (chain ${config.chainId})...`);
    if (await checkVerifiable(config, address)) {
        console.log(`Already verified: ${config.browserUrl}address/${address}#code`);
        return;
    }

    const contractsDir = path.resolve(__dirname, "..");
    const constructorArgsHex = encodeConstructorArgs(
        contractsDir,
        contractFqn,
        constructorArgs
    );

    const {standardJson, compilerVersion} = getHardhatStandardJsonInput(
        contractsDir,
        contractFqn
    );

    const form = new FormData();
    form.append("compiler_version", compilerVersion);
    form.append("contract_name", contractFqn.split(":")[1]);
    form.append(
        "files[0]",
        new Blob([standardJson], {type: "application/json"}),
        "standard-input.json"
    );
    form.append("autodetect_constructor_args", "false");
    form.append("constructor_args", constructorArgsHex);
    form.append("license_type", "none");

    const verifyUrl =
        `${config.apiBase}/smart-contracts/${address}` +
        "/verification/via/standard-input";
    const res = await fetch(verifyUrl, {
        method: "POST",
        headers: {"user-agent": USER_AGENT, accept: "application/json"},
        body: form,
    });
    const body = await res.text();
    if (!res.ok) {
        throw new Error(
            `HTTP ${res.status} from ${verifyUrl}: ${body.slice(0, 500)}`
        );
    }

    console.log(`Verification submitted: ${body.trim()}`);
    await pollBlockscoutVerification(config, address);
    console.log(`Verified: ${config.browserUrl}address/${address}#code`);
}

/**
 * Verify on whichever explorer the network uses: Blockscout's native v2 API for
 * Blockscout chains, hardhat-verify's Etherscan flow everywhere else.
 *
 * @param {string} network Hardhat network name
 * @param {string} address Deployed contract address
 * @param {string} contractFqn Source path and contract name, `src/X.sol:X`.
 *     Used by the Blockscout path; hardhat-verify detects it from the bytecode.
 * @param {Array} constructorArgs Constructor arguments, in declaration order
 */
async function verifyOnExplorer({network, address, contractFqn, constructorArgs}) {
    if (BLOCKSCOUT_NETWORKS[network]) {
        await verifyOnBlockscout({
            network,
            address,
            contractFqn,
            constructorArgs,
        });
        return;
    }

    await hre.run("verify:verify", {
        address,
        constructorArguments: constructorArgs,
    });
}

const txServiceUrls = {
    ethereum: "https://safe-transaction-mainnet.safe.global",
    base: "https://safe-transaction-base.safe.global",
    unichain: "https://safe-transaction-unichain.safe.global",
};

const txServiceUrl = txServiceUrls[hre.network.name];

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

async function proposeOrSendTransaction(safeAddress, txData, signer, methodName) {
    if (safeAddress) {
        return proposeTransaction(safeAddress, txData, signer, methodName);
    } else {
        console.log(`Executing the transaction directly`);
        const tx = await signer.sendTransaction(txData);
        await tx.wait();
        return tx.hash;
    }
}

async function proposeTransaction(safeAddress, txData, signer, methodName) {
    const signerAddress = await signer.getAddress();
    console.log(`Proposing transaction to Safe: ${safeAddress} with account: ${signerAddress}`);

    const ethAdapter = new EthersAdapter({
        ethers,
        signerOrProvider: signer,
    });

    const safeService = new SafeApiKit({txServiceUrl, ethAdapter});

    const safeSdk = await Safe.create({
        ethAdapter,
        safeAddress,
    });
    let next_nonce = await safeService.getNextNonce(safeAddress);
    const safeTransaction = await safeSdk.createTransaction({
        safeTransactionData: {
            ...txData,
            nonce: next_nonce
        }
    });
    const safeTxHash = await safeSdk.getTransactionHash(safeTransaction);
    const senderSignature = await safeSdk.signTransactionHash(safeTxHash);

    const proposeArgs = {
        safeAddress,
        safeTransactionData: safeTransaction.data,
        safeTxHash,
        senderAddress: signerAddress,
        senderSignature: senderSignature.data,
        origin: `Proposed from hardhat: ${methodName}`,
        nonce: next_nonce,
    };

    await safeService.proposeTransaction(proposeArgs);
    return safeTxHash;
}

module.exports = {
    proposeOrSendTransaction,
    resolveRolesNetwork,
    verifyOnExplorer,
}
