pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant AERO_BASIC_PROTOCOL_SYSTEM = "aero_basic";

abstract contract AeroBasicDeployment is ExecutorDeployments {
    address internal immutable aeroBasicExecutor =
        executorAddress(AERO_BASIC_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_aeroBasicDeployment);
    }

    function _aeroBasicDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            AERO_BASIC_PROTOCOL_SYSTEM, "AeroBasicExecutor", ""
        );
    }
}
