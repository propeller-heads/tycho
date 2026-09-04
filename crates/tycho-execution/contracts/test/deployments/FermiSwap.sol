pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// FermiSwap
address constant FERMI_SWAPPER = 0xb1076fE3AB5e28005C7c323Bac5AC06a680d452e;

string constant FERMI_SWAP_PROTOCOL_SYSTEM = "vm:fermiswap";

abstract contract FermiSwapDeployment is ExecutorDeployments {
    address internal immutable fermiSwapExecutor =
        executorAddress(FERMI_SWAP_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_fermiSwapDeployment);
    }

    function _fermiSwapDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            FERMI_SWAP_PROTOCOL_SYSTEM,
            "FermiSwapExecutor",
            abi.encode(FERMI_SWAPPER)
        );
    }
}
