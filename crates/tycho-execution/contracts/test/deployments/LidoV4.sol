pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";
import {STETH_ADDR} from "../Constants.sol";

address constant WSTETH_ADDR =
    address(0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0);

string constant LIDO_V4_PROTOCOL_SYSTEM = "lido_v4";

abstract contract LidoV4Deployment is ExecutorDeployments {
    address internal immutable lidoV4Executor =
        executorAddress(LIDO_V4_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_lidoV4Deployment);
    }

    function _lidoV4Deployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            LIDO_V4_PROTOCOL_SYSTEM,
            "LidoV4Executor",
            abi.encode(STETH_ADDR, WSTETH_ADDR)
        );
    }
}
