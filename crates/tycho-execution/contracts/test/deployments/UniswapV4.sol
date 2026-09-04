pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// Angstrom hook address
address constant ANGSTROM_HOOK = 0x0000000aa232009084Bd71A5797d089AA4Edfad4;

// Uniswap v4 pool manager
address constant POOL_MANAGER = 0x000000000004444c5dc75cB358380D2e3dE08A90;

string constant UNISWAP_V4_PROTOCOL_SYSTEM = "uniswap_v4";

abstract contract UniswapV4Deployment is ExecutorDeployments {
    address internal immutable uniswapV4Executor =
        executorAddress(UNISWAP_V4_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_uniswapV4Deployment);
    }

    function _uniswapV4Deployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            UNISWAP_V4_PROTOCOL_SYSTEM,
            "UniswapV4Executor",
            abi.encode(POOL_MANAGER, ANGSTROM_HOOK)
        );
    }
}
