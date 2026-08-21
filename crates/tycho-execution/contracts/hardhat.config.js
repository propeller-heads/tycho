/** @type import('hardhat/config').HardhatUserConfig */
require("@tenderly/hardhat-tenderly");
require("@nomicfoundation/hardhat-verify");
require("@nomiclabs/hardhat-ethers");
require("@nomicfoundation/hardhat-foundry");

// Chains whose explorer is Blockscout rather than an Etherscan instance.
const BLOCKSCOUT_NETWORKS = ["robinhood"];

// hardhat-verify sends every string apiKey to the Etherscan v2 API, which does
// not index Blockscout-only chains. An object keyed by network name keeps
// verification on the customChains apiURL instead. Blockscout accepts any value
// here; a Pro key from dev.blockscout.com raises the request rate limit.
function explorerApiKey() {
    const flag = process.argv.indexOf("--network");
    const network = flag === -1 ? process.env.HARDHAT_NETWORK : process.argv[flag + 1];
    if (BLOCKSCOUT_NETWORKS.includes(network)) {
        return {[network]: process.env.BLOCKCHAIN_EXPLORER_API_KEY || "empty"};
    }
    return process.env.BLOCKCHAIN_EXPLORER_API_KEY;
}

module.exports = {
    solidity: {
        compilers: [
            {
                version: "0.8.26",
                settings: {
                    evmVersion: "cancun",
                    viaIR: true,
                    optimizer: {
                        enabled: true,
                        runs: 1000,
                    },
                },
            },
            {
                version: "0.8.33",
                settings: {
                    evmVersion: "cancun",
                    viaIR: true,
                    optimizer: {
                        enabled: true,
                        runs: 1000,
                    },
                },
            },
        ],
    },

    networks: {
        tenderly_ethereum: {
            url: process.env.RPC_URL,
            accounts: [process.env.PRIVATE_KEY]
        },
        tenderly_base: {
            url: process.env.RPC_URL,
            accounts: [process.env.PRIVATE_KEY]
        },
        ethereum: {
            url: process.env.RPC_URL,
            accounts: [process.env.PRIVATE_KEY],
            chainId: 1
        },
        base: {
            url: process.env.RPC_URL,
            accounts: [process.env.PRIVATE_KEY],
            chainId: 8453
        },
        unichain: {
            url: process.env.RPC_URL,
            accounts: [process.env.PRIVATE_KEY],
            chainId: 130
        },
        arbitrum: {
            url: process.env.RPC_URL,
            accounts: [process.env.PRIVATE_KEY],
            chainId: 42161
        },
        bsc: {
            url: process.env.RPC_URL,
            accounts: [process.env.PRIVATE_KEY],
            chainId: 56
        },
        polygon: {
            url: process.env.RPC_URL,
            accounts: [process.env.PRIVATE_KEY],
            chainId: 137
        },
        plasma: {
            url: process.env.RPC_URL,
            accounts: [process.env.PRIVATE_KEY],
            chainId: 9745
        },
        robinhood: {
            url: process.env.RPC_URL,
            accounts: [process.env.PRIVATE_KEY],
            chainId: 4663
        }
    },

    tenderly: {
        project: "tycho",
        username: "tvinagre",
        privateVerification: false,
    },

    etherscan: {
        apiKey: explorerApiKey(),
        customChains: [
            {
                network: "unichain",
                chainId: 130,
                urls: {
                    apiURL: "https://api.uniscan.xyz/api",
                    browserURL: "https://www.uniscan.xyz/"
                }
            },
            {
                network: "plasma",
                chainId: 9745,
                urls: {
                    apiURL: "https://api.routescan.io/v2/network/mainnet/evm/9745/etherscan/api",
                    browserURL: "https://plasmascan.to/"
                }
            },
            {
                network: "robinhood",
                chainId: 4663,
                urls: {
                    apiURL: "https://robinhoodchain.blockscout.com/api",
                    browserURL: "https://robinhoodchain.blockscout.com/"
                }
            }
        ]
    }
};
