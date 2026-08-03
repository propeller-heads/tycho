const {ethers} = require("hardhat");
const Safe = require('@safe-global/protocol-kit').default;
const {EthersAdapter} = require('@safe-global/protocol-kit');
const {default: SafeApiKit} = require("@safe-global/api-kit");
const roles = require("./roles.json");

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
    resolveRolesNetwork
}