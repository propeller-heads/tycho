pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// Bebop Settlement
address constant BEBOP_SETTLEMENT = 0xbbbbbBB520d69a9775E85b458C58c648259FAD5F;
// Bebop Router
address constant BEBOP_ROUTER = 0xBeb0009ACa35087ce7cCF11637E24dd1Aad3bf2A;

string constant BEBOP_PROTOCOL_SYSTEM = "rfq:bebop";

abstract contract BebopDeployment is ExecutorDeployments {
    address internal immutable bebopExecutor =
        executorAddress(BEBOP_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_bebopDeployment);
    }

    function _bebopDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            BEBOP_PROTOCOL_SYSTEM,
            "BebopExecutor",
            abi.encode(BEBOP_SETTLEMENT, BEBOP_ROUTER)
        );
    }
}
