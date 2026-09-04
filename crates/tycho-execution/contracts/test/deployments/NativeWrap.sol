pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";
import {WETH_ADDR} from "../Constants.sol";

string constant NATIVE_WRAP_PROTOCOL_SYSTEM = "native_wrapper";

abstract contract NativeWrapDeployment is ExecutorDeployments {
    address internal immutable nativeWrapExecutor =
        executorAddress(NATIVE_WRAP_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_nativeWrapDeployment);
    }

    function _nativeWrapDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            NATIVE_WRAP_PROTOCOL_SYSTEM,
            "NativeWrapExecutor",
            abi.encode(WETH_ADDR)
        );
    }
}
