pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant AERODROME_V1_PROTOCOL_SYSTEM = "aerodrome_v1";

abstract contract AerodromeV1Deployment is ExecutorDeployments {
    address internal immutable aerodromeV1Executor =
        executorAddress(AERODROME_V1_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_aerodromeV1Deployment);
    }

    function _aerodromeV1Deployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            AERODROME_V1_PROTOCOL_SYSTEM, "AerodromeV1Executor", ""
        );
    }
}
