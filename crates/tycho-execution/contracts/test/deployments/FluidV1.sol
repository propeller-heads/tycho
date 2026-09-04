pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// Fluid liquidity contract on mainnet
address constant FLUIDV1_LIQUIDITY = 0x52Aa899454998Be5b000Ad077a46Bbe360F4e497;

string constant FLUID_V1_PROTOCOL_SYSTEM = "fluid_v1";

abstract contract FluidV1Deployment is ExecutorDeployments {
    address internal immutable fluidV1Executor =
        executorAddress(FLUID_V1_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_fluidV1Deployment);
    }

    function _fluidV1Deployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            FLUID_V1_PROTOCOL_SYSTEM,
            "FluidV1Executor",
            abi.encode(FLUIDV1_LIQUIDITY)
        );
    }
}
