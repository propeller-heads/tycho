require('dotenv').config();
const {ethers} = require("hardhat");
const hre = require("hardhat");

// Comment out the executors you don't want to deploy
const executors_to_deploy = {
    "ethereum": [
        // USV2 - Args: Fee BPS
        {exchange: "UniswapV2Executor", args: [30]},
        // PANCAKESWAP V2 - Args: Fee BPS
        {exchange: "UniswapV2Executor", args: [25]},
        // USV3 & PANCAKESWAP V3 - Args: (none)
        {exchange: "UniswapV3Executor", args: []},
        // Args: Pool manager, Angstrom hook
        {
            exchange: "UniswapV4Executor", args: [
                "0x000000000004444c5dc75cB358380D2e3dE08A90",
                "0x0000000aa232009084Bd71A5797d089AA4Edfad4"
            ]
        },
        // Args: (none)
        {exchange: "BalancerV2Executor", args: []},
        // Args: Ekubo core contract, mev resist
        {
            exchange: "EkuboExecutor", args: [
                "0xe0e0e08A6A4b9Dc7bD67BCB7aadE5cF48157d444",
                "0x553a2EFc570c9e104942cEC6aC1c18118e54C091"
            ]
        },
        // Args: ETH address in curve pools, stETH address
        {
            exchange: "CurveExecutor", args: [
                "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE",
                "0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84"
            ]
        },
        // Args: (none)
        {exchange: "MaverickV2Executor", args: []},
        // Args: (none)
        {exchange: "BalancerV3Executor", args: []},
        // Args: Bebop Settlement contract
        {
            exchange: "BebopExecutor",
            args: ["0xbbbbbBB520d69a9775E85b458C58c648259FAD5F"]
        },
        // Args: Hashflow router
        {
            exchange: "HashflowExecutor",
            args: ["0x55084eE0fEf03f14a305cd24286359A35D735151"]
        },
        // Args: liquidity
        {
            exchange: "FluidV1Executor", args: [
                "0x52Aa899454998Be5b000Ad077a46Bbe360F4e497"
            ]
        },
        // Args:
        {
            exchange: "ERC4626Executor", args: []
        },
        // Args: deposit pool
        {
            exchange: "RocketpoolExecutor", args: [
                "0xCE15294273CFb9D9b628F4D61636623decDF4fdC",
            ]
        },
        // Args:
        {
            exchange: "EkuboV3Executor", args: []
        },
        // Args: Wrapped native token address
        {
            exchange: "NativeWrapExecutor", args: ["0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"]
        },
        // Args: Liquorice settlement, Liquorice balance manager
        {
            exchange: "LiquoriceExecutor", args: [
                "0x0448633eb8B0A42EfED924C42069E0DcF08fb552",
                "0xb87bAE43a665EB5943A5642F81B26666bC9E5C95"
            ]
        },
        // Args: FermiSwapper
        {
            exchange: "FermiSwapExecutor",
            args: ["0xb1076fE3AB5e28005C7c323Bac5AC06a680d452e"]
        },
    ],
    "base": [
        // USV2 - Args: Fee BPS
        {exchange: "UniswapV2Executor", args: [30]},
        // USV3 - Args: (none)
        {exchange: "UniswapV3Executor", args: []},
        // USV4 - Args: Pool manager, Angstrom hook
        {
            exchange: "UniswapV4Executor", args: [
                "0x498581ff718922c3f8e6a244956af099b2652b2b",
                "0x631352Aaa9d6554848aF674106bCD8Bb9E59a5CF"
            ]
        },
        // Args: Bebop Settlement contract
        {
            exchange: "BebopExecutor",
            args: ["0xbbbbbBB520d69a9775E85b458C58c648259FAD5F"]
        },
        // Aerodrome Slipstreams - Args: (none)
        {exchange: "SlipstreamsExecutor", args: []},
        // Args: Wrapped native token address
        {
            exchange: "NativeWrapExecutor", args: ["0x4200000000000000000000000000000000000006"]
        },
    ],
    "unichain": [
        // USV2 - Args: Fee BPS
        {exchange: "UniswapV2Executor", args: [30]},
        // USV3 - Args: (none)
        {exchange: "UniswapV3Executor", args: []},
        // USV4 - Args: Pool manager, Angstrom hook
        {
            exchange: "UniswapV4Executor", args: [
                "0x1f98400000000000000000000000000000000004",
                // This is the Angstrom address for ethereum. There isn't one for unichain
                "0x0000000aa232009084Bd71A5797d089AA4Edfad4"
            ]
        },
        // Args: ETH address in curve pools, stETH address
        {
            exchange: "CurveExecutor", args: [
                "0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE",
                "0x0000000000000000000000000000000000000000" // No stETH on unichain
            ]
        },
        // Aerodrome Slipstreams - Args: (none)
        {exchange: "SlipstreamsExecutor", args: []},
        // Args: Wrapped native token address
        {
            exchange: "NativeWrapExecutor", args: ["0x4200000000000000000000000000000000000006"]
        },
    ],
    "arbitrum": [
        // USV2 - Args: Fee BPS
        {exchange: "UniswapV2Executor", args: [30]},
        // USV3 & PANCAKESWAP V3 - Args: (none)
        {exchange: "UniswapV3Executor", args: []},
        // USV4 - Args: Pool manager, Angstrom hook
        {
            exchange: "UniswapV4Executor", args: [
                "0x360e68faccca8ca495c1b759fd9eee466db9fb32",
                // This is the Angstrom address for ethereum. There isn't one for
                // arbitrum
                "0x0000000aa232009084Bd71A5797d089AA4Edfad4"
            ]
        },
        // Args: Wrapped native token address (WETH)
        {
            exchange: "NativeWrapExecutor", args: ["0x82aF49447D8a07e3bd95BD0d56f35241523fBab1"]
        },
    ],
    "polygon": [
        // USV2 & QUICKSWAP V2 - Args: Fee BPS
        {exchange: "UniswapV2Executor", args: [30]},
        // USV3 - Args: (none)
        {exchange: "UniswapV3Executor", args: []},
        // USV4 - Args: Pool manager, Angstrom hook
        {
            exchange: "UniswapV4Executor", args: [
                "0x67366782805870060151383f4bbff9dab53e5cd6",
                // This is the Angstrom address for ethereum. There isn't one for
                // polygon
                "0x0000000aa232009084Bd71A5797d089AA4Edfad4"
            ]
        },
        // Args: Wrapped native token address (WPOL)
        {
            exchange: "NativeWrapExecutor", args: ["0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270"]
        },
    ],
    "bsc": [
        // USV2 - Args: Fee BPS
        {exchange: "UniswapV2Executor", args: [30]},
        // PANCAKESWAP V2 - Args: Fee BPS
        {exchange: "UniswapV2Executor", args: [25]},
        // USV3 & PANCAKESWAP V3 - Args: (none)
        {exchange: "UniswapV3Executor", args: []},
        // USV4 - Args: Pool manager, Angstrom hook
        {
            exchange: "UniswapV4Executor", args: [
                "0x28e2ea090877bf75740558f6bfb36a5ffee9e9df",
                // This is the Angstrom address for ethereum. There isn't one for BSC
                "0x0000000aa232009084Bd71A5797d089AA4Edfad4"
            ]
        },
        // Args: Wrapped native token address (WBNB)
        {
            exchange: "NativeWrapExecutor", args: ["0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c"]
        },
    ],
}

async function main() {
    const network = hre.network.name;
    console.log(`Deploying executors to ${network}`);

    const [deployer] = await ethers.getSigners();
    console.log(`Deploying with account: ${deployer.address}`);
    console.log(`Account balance: ${ethers.utils.formatEther(await deployer.getBalance())} ETH`);

    // Deterministic Deployment Proxy
    // More info: https://getfoundry.sh/guides/deterministic-deployments-using-create2/
    const create2FactoryAddress = "0x4e59b44847b379578588920cA78FbF26c0B4956C";
    console.log(`Using CREATE2 factory at: ${create2FactoryAddress}`);

    for (const executor of executors_to_deploy[network]) {
        const {exchange, args} = executor;
        const Executor = await ethers.getContractFactory(exchange);

        // Get bytecode with constructor arguments
        const deployTx = Executor.getDeployTransaction(...args);
        const bytecode = deployTx.data;

        // Use a salt that includes network and executor name
        const salt = ethers.utils.id(`${exchange}-${network}`);

        // Compute the address where the contract will be deployed
        // CREATE2 address = keccak256(0xff ++ factory_address ++ salt ++ keccak256(bytecode))[12:]
        const bytecodeHash = ethers.utils.keccak256(bytecode);
        const computedAddress = ethers.utils.getCreate2Address(create2FactoryAddress, salt, bytecodeHash);
        console.log(`${exchange} will be deployed to: ${computedAddress}`);

        const deploymentData = ethers.utils.concat([salt, bytecode]);
        const tx = await deployer.sendTransaction({
            to: create2FactoryAddress,
            data: deploymentData,
        });
        await tx.wait();
        console.log(`${exchange} deployed to: ${computedAddress}`);

        // Verify on Tenderly
        try {
            await hre.tenderly.verify({
                name: exchange,
                address: computedAddress,
            });
            console.log("Contract verified successfully on Tenderly");
        } catch (error) {
            console.error("Error during contract verification:", error);
        }

        console.log("Waiting for 1 minute before verifying the contract...");
        await new Promise(resolve => setTimeout(resolve, 60000));
        // Verify on Etherscan
        try {
            await hre.run("verify:verify", {
                address: computedAddress,
                constructorArguments: args,
            });
            console.log(`${exchange} verified successfully on blockchain explorer!`);
        } catch (error) {
            console.error(`Error during blockchain explorer verification:`, error);
        }
    }
}

main()
    .then(() => process.exit(0))
    .catch((error) => {
        console.error("Deployment failed:", error);
        process.exit(1);
    });
