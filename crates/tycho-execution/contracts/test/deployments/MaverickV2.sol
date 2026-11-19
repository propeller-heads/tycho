pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant MAVERICK_V2_PROTOCOL_SYSTEM = "vm:maverick_v2";

abstract contract MaverickV2Deployment is ExecutorDeployments {
    address internal immutable maverickV2Executor =
        executorAddress(MAVERICK_V2_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_maverickV2Deployment);
    }

    function _maverickV2Deployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            MAVERICK_V2_PROTOCOL_SYSTEM, "MaverickV2Executor", ""
        );
    }
}
