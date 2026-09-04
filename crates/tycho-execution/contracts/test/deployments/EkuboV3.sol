pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// Ekubo V3 SignedExclusiveSwap extension on mainnet
address constant EKUBO_V3_SIGNED_EXCLUSIVE_SWAP =
    0x55b703eED01b35641963da2FB2E14885993605A3;

string constant EKUBO_V3_PROTOCOL_SYSTEM = "ekubo_v3";

abstract contract EkuboV3Deployment is ExecutorDeployments {
    address internal immutable ekuboV3Executor =
        executorAddress(EKUBO_V3_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_ekuboV3Deployment);
    }

    function _ekuboV3Deployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            EKUBO_V3_PROTOCOL_SYSTEM,
            "EkuboV3Executor",
            abi.encode(EKUBO_V3_SIGNED_EXCLUSIVE_SWAP)
        );
    }
}
