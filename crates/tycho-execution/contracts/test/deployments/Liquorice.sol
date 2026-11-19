pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// Liquorice
address constant LIQUORICE_SETTLEMENT =
    0x43Dcd6586e6209eE7235A21bDC4aa301E5Bc44e8;
address constant LIQUORICE_BALANCE_MANAGER =
    0x56B4720e40dF52C560E520238E34e9C33d57E593;

string constant LIQUORICE_PROTOCOL_SYSTEM = "rfq:liquorice";

abstract contract LiquoriceDeployment is ExecutorDeployments {
    address internal immutable liquoriceExecutor =
        executorAddress(LIQUORICE_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_liquoriceDeployment);
    }

    function _liquoriceDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            LIQUORICE_PROTOCOL_SYSTEM,
            "LiquoriceExecutor",
            abi.encode(LIQUORICE_SETTLEMENT, LIQUORICE_BALANCE_MANAGER)
        );
    }
}
