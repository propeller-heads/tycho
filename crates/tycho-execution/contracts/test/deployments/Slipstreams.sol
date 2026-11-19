pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant SLIPSTREAMS_PROTOCOL_SYSTEM = "aerodrome_slipstreams";

abstract contract SlipstreamsDeployment is ExecutorDeployments {
    address internal immutable slipstreamsExecutor =
        executorAddress(SLIPSTREAMS_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_slipstreamsDeployment);
    }

    function _slipstreamsDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            SLIPSTREAMS_PROTOCOL_SYSTEM, "SlipstreamsExecutor", ""
        );
    }
}
