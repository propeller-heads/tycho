pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

// Native Relay V6 routers
address constant NATIVE_ROUTER_V6_ETHEREUM =
    0x4777A6B3A9A889ABfd4C7666Bdd2a7AB633293be;
address constant NATIVE_ROUTER_V6_BASE =
    0x9706D3fff42571305Fc201F996ADC0e768d72911;
address constant NATIVE_ROUTER_V6_ARBITRUM =
    0x0183D055c77310aF03dCB397eFAA7E9cfB6dB59b;
address constant NATIVE_ROUTER_V6_BSC =
    0x1fDED89D98CBeADd96a109D28689c2638025dad3;
address constant NATIVE_ROUTER_V6_ROBINHOOD =
    0x4Df7557734B382EB542BEa6c74786D398DF4CC19;

string constant NATIVE_PROTOCOL_SYSTEM = "rfq:native";

abstract contract NativeDeployment is ExecutorDeployments {
    address internal immutable nativeExecutor =
        executorAddress(NATIVE_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_nativeDeployment);
    }

    function _nativeDeployment()
        private
        view
        returns (ExecutorDeployment memory)
    {
        return ExecutorDeployment(
            NATIVE_PROTOCOL_SYSTEM,
            "NativeExecutor",
            abi.encode(nativeRouterV6())
        );
    }
}

function nativeRouterV6() view returns (address router) {
    if (block.chainid == 1) router = NATIVE_ROUTER_V6_ETHEREUM;
    if (block.chainid == 8453) router = NATIVE_ROUTER_V6_BASE;
    if (block.chainid == 42161) router = NATIVE_ROUTER_V6_ARBITRUM;
    if (block.chainid == 56) router = NATIVE_ROUTER_V6_BSC;
    if (block.chainid == 4663) router = NATIVE_ROUTER_V6_ROBINHOOD;
    require(
        router != address(0), "No Native V6 router is configured for this chain"
    );
}
