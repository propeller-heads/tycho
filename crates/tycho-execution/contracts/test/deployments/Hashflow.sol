pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// Hashflow Router
address constant HASHFLOW_ROUTER = 0x55084eE0fEf03f14a305cd24286359A35D735151;

string constant HASHFLOW_PROTOCOL_SYSTEM = "rfq:hashflow";

abstract contract HashflowDeployment is ExecutorDeployments {
    address internal immutable hashflowExecutor =
        executorAddress(HASHFLOW_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_hashflowDeployment);
    }

    function _hashflowDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            HASHFLOW_PROTOCOL_SYSTEM,
            "HashflowExecutor",
            abi.encode(HASHFLOW_ROUTER)
        );
    }
}
