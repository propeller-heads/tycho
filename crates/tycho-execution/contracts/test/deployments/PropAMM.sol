pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant PROP_AMM_PROTOCOL_SYSTEM = "pricelevelstream";

abstract contract PropAMMDeployment is ExecutorDeployments {
    address internal immutable propAMMExecutor =
        executorAddress(PROP_AMM_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_propAMMDeployment);
    }

    function _propAMMDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return
            ExecutorDeployment(PROP_AMM_PROTOCOL_SYSTEM, "PropAMMExecutor", "");
    }
}
