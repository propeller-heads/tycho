pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// Eden Network's view quoter for Uniswap V3 pools, used by TychoFallbackRouter
address constant UNISWAP_V3_STATIC_QUOTER =
    0xc80f61d1bdAbD8f5285117e1558fDDf8C64870FE;

// Pools
address constant DAI_WETH_USV3 = 0xC2e9F25Be6257c210d7Adf0D4Cd6E3E881ba25f8;
address constant DAI_USDT_USV3 = 0x48DA0965ab2d2cbf1C17C09cFB5Cbe67Ad5B1406;
address constant USDC_WETH_USV3 = 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640; // 0.05% fee
address constant USDC_WETH_USV3_2 = 0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8; // 0.3% fee

string constant UNISWAP_V3_PROTOCOL_SYSTEM = "uniswap_v3";

abstract contract UniswapV3Deployment is ExecutorDeployments {
    address internal immutable uniswapV3Executor =
        executorAddress(UNISWAP_V3_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_uniswapV3Deployment);
    }

    function _uniswapV3Deployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            UNISWAP_V3_PROTOCOL_SYSTEM, "UniswapV3Executor", ""
        );
    }
}
