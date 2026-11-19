pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// 1inch Aggregation Router v5, Euclid's settlement target
address constant ONEINCH_ROUTER_V5 = 0x1111111254EEB25477B68fb85Ed929f73A960582;

string constant EUCLID_PROTOCOL_SYSTEM = "rfq:euclid";

abstract contract EuclidDeployment is ExecutorDeployments {
    address internal immutable euclidExecutor =
        executorAddress(EUCLID_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_euclidDeployment);
    }

    function _euclidDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            EUCLID_PROTOCOL_SYSTEM,
            "EuclidExecutor",
            abi.encode(ONEINCH_ROUTER_V5)
        );
    }
}
