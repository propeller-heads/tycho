pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";
import {ETH_ADDR, STETH_ADDR} from "../Constants.sol";

// Pools
address constant TRIPOOL = 0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7;
address constant TRICRYPTO_POOL = 0x7F86Bf177Dd4F3494b841a37e810A34dD56c829B;
address constant STETH_POOL = 0xDC24316b9AE028F1497c275EB9192a3Ea0f67022;

string constant CURVE_PROTOCOL_SYSTEM = "vm:curve";

abstract contract CurveDeployment is ExecutorDeployments {
    address internal immutable curveExecutor =
        executorAddress(CURVE_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_curveDeployment);
    }

    function _curveDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            CURVE_PROTOCOL_SYSTEM,
            "CurveExecutor",
            abi.encode(ETH_ADDR, STETH_ADDR)
        );
    }
}
