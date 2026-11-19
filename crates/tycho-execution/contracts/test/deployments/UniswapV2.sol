pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// Pools
address constant DAI_WETH_UNIV2_POOL =
    0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11;
address constant DAI_USDC_POOL = 0xAE461cA67B15dc8dc81CE7615e0320dA1A9aB8D5;
address constant WETH_WBTC_POOL = 0xBb2b8038a1640196FbE3e38816F3e67Cba72D940;
address constant USDC_WBTC_POOL = 0x004375Dff511095CC5A197A54140a24eFEF3A416;
address constant USDC_WETH_USV2 = 0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc;

// Pools on Base
address constant USDC_MAG7_POOL = 0x739c2431670A12E2cF8e11E3603eB96e6728a789;

string constant UNISWAP_V2_PROTOCOL_SYSTEM = "uniswap_v2";

abstract contract UniswapV2Deployment is ExecutorDeployments {
    address internal immutable uniswapV2Executor =
        executorAddress(UNISWAP_V2_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_uniswapV2Deployment);
    }

    function _uniswapV2Deployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            UNISWAP_V2_PROTOCOL_SYSTEM,
            "UniswapV2Executor",
            abi.encode(uint256(30))
        );
    }
}
