pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant BICONOMY_PROTOCOL_SYSTEM = "biconomy";

abstract contract BiconomyDeployment is ExecutorDeployments {
    address internal immutable biconomyExecutor =
        executorAddress(BICONOMY_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_biconomyDeployment);
    }

    function _biconomyDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return
            ExecutorDeployment(BICONOMY_PROTOCOL_SYSTEM, "PropAMMExecutor", "");
    }
}
