pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant BALANCER_V2_PROTOCOL_SYSTEM = "vm:balancer_v2";

abstract contract BalancerV2Deployment is ExecutorDeployments {
    address internal immutable balancerV2Executor =
        executorAddress(BALANCER_V2_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_balancerV2Deployment);
    }

    function _balancerV2Deployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            BALANCER_V2_PROTOCOL_SYSTEM, "BalancerV2Executor", ""
        );
    }
}
