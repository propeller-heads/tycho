pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant LUNAR_BASE_PROTOCOL_SYSTEM = "lunarbase";

abstract contract LunarBaseDeployment is ExecutorDeployments {
    address internal immutable lunarBaseExecutor =
        executorAddress(LUNAR_BASE_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_lunarBaseDeployment);
    }

    function _lunarBaseDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            LUNAR_BASE_PROTOCOL_SYSTEM, "LunarBaseExecutor", ""
        );
    }
}
