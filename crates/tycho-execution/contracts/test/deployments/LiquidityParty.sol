pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant LIQUIDITY_PARTY_PROTOCOL_SYSTEM = "vm:liquidityparty";

abstract contract LiquidityPartyDeployment is ExecutorDeployments {
    address internal immutable liquidityPartyExecutor =
        executorAddress(LIQUIDITY_PARTY_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_liquidityPartyDeployment);
    }

    function _liquidityPartyDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            LIQUIDITY_PARTY_PROTOCOL_SYSTEM, "LiquidityPartyExecutor", ""
        );
    }
}
