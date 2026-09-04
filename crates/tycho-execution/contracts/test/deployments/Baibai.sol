pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant BAIBAI_PROTOCOL_SYSTEM = "baibai";

abstract contract BaibaiDeployment is ExecutorDeployments {
    address internal immutable baibaiExecutor =
        executorAddress(BAIBAI_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_baibaiDeployment);
    }

    function _baibaiDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            BAIBAI_PROTOCOL_SYSTEM,
            "BaibaiExecutor",
            abi.encode(0x98c1D9E102Eb2806D902b13186BDc7892aC4fFBa)
        );
    }
}
