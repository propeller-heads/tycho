pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant BALANCER_V3_PROTOCOL_SYSTEM = "vm:balancer_v3";

abstract contract BalancerV3Deployment is ExecutorDeployments {
    address internal immutable balancerV3Executor =
        executorAddress(BALANCER_V3_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_balancerV3Deployment);
    }

    function _balancerV3Deployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            BALANCER_V3_PROTOCOL_SYSTEM, "BalancerV3Executor", ""
        );
    }
}
