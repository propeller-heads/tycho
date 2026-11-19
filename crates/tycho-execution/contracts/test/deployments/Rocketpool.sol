pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// Rocketpool v1.4 (Saturn 1) deposit pool
address constant ROCKET_DEPOSIT_POOL =
    0xCE15294273CFb9D9b628F4D61636623decDF4fdC;

string constant ROCKETPOOL_PROTOCOL_SYSTEM = "rocketpool";

abstract contract RocketpoolDeployment is ExecutorDeployments {
    address internal immutable rocketpoolExecutor =
        executorAddress(ROCKETPOOL_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_rocketpoolDeployment);
    }

    function _rocketpoolDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            ROCKETPOOL_PROTOCOL_SYSTEM,
            "RocketpoolExecutor",
            abi.encode(ROCKET_DEPOSIT_POOL)
        );
    }
}
