/** @type import('hardhat/config').HardhatUserConfig */
require("@tenderly/hardhat-tenderly");
require("@nomicfoundation/hardhat-verify");
require("@nomiclabs/hardhat-ethers");
require("@nomicfoundation/hardhat-foundry");

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

    // Etherscan-family explorers only. Blockscout chains verify through their
    // native v2 API in scripts/utils.js, which hardhat-verify never sees.
    etherscan: {
        apiKey: process.env.BLOCKCHAIN_EXPLORER_API_KEY,
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
            }
        ]
    }
};
