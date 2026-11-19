pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant EKUBO_PROTOCOL_SYSTEM = "ekubo_v2";

abstract contract EkuboDeployment is ExecutorDeployments {
    address internal immutable ekuboExecutor =
        executorAddress(EKUBO_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_ekuboDeployment);
    }

    function _ekuboDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            EKUBO_PROTOCOL_SYSTEM,
            "EkuboExecutor",
            abi.encode(
                0xe0e0e08A6A4b9Dc7bD67BCB7aadE5cF48157d444,
                0x553a2EFc570c9e104942cEC6aC1c18118e54C091
            )
        );
    }
}
