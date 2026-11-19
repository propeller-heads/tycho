pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// BopAMM (Bebop on-chain PMM)
address constant BOPAMM_SETTLEMENT = 0xdB13ad0fcD134E9c48f2fDaEa8f6751a0F5349ca;

string constant BOP_AMM_PROTOCOL_SYSTEM = "vm:bopamm";

abstract contract BopAMMDeployment is ExecutorDeployments {
    address internal immutable bopAMMExecutor =
        executorAddress(BOP_AMM_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_bopAMMDeployment);
    }

    function _bopAMMDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            BOP_AMM_PROTOCOL_SYSTEM,
            "BopAMMExecutor",
            abi.encode(BOPAMM_SETTLEMENT)
        );
    }
}
