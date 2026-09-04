pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// Ring Swap V2
address constant RING_FEW_FACTORY = 0x7D86394139bf1122E82FDF45Bb4e3b038A4464DD;
address constant RING_SWAP_FACTORY = 0xeb2A625B704d73e82946D8d026E1F588Eed06416;

string constant RING_SWAP_V2_PROTOCOL_SYSTEM = "ring_swap_v2";

abstract contract RingSwapV2Deployment is ExecutorDeployments {
    address internal immutable ringSwapV2Executor =
        executorAddress(RING_SWAP_V2_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_ringSwapV2Deployment);
    }

    function _ringSwapV2Deployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            RING_SWAP_V2_PROTOCOL_SYSTEM,
            "RingSwapV2Executor",
            abi.encode(RING_FEW_FACTORY, RING_SWAP_FACTORY)
        );
    }
}
