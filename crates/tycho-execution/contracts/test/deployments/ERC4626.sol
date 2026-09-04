pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant ERC4626_PROTOCOL_SYSTEM = "erc4626";

abstract contract ERC4626Deployment is ExecutorDeployments {
    address internal immutable erc4626Executor =
        executorAddress(ERC4626_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_erc4626Deployment);
    }

    function _erc4626Deployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return
            ExecutorDeployment(ERC4626_PROTOCOL_SYSTEM, "ERC4626Executor", "");
    }
}
