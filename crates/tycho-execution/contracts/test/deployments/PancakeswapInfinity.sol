pragma solidity ^0.8.26;

import {
    ExecutorDeployment,
    ExecutorDeployments,
    executorAddress
} from "../ExecutorDeployment.sol";

string constant PANCAKESWAP_INFINITY_PROTOCOL_SYSTEM =
    "pancakeswap_infinity_cl";

abstract contract PancakeswapInfinityDeployment is ExecutorDeployments {
    address internal immutable pancakeswapInfinityExecutor =
        executorAddress(PANCAKESWAP_INFINITY_PROTOCOL_SYSTEM);

    constructor() {
        deploymentHooks.push(_pancakeswapInfinityDeployment);
    }

    function _pancakeswapInfinityDeployment()
        private
        pure
        returns (ExecutorDeployment memory)
    {
        // Base and BNB share these addresses.
        return ExecutorDeployment(
            PANCAKESWAP_INFINITY_PROTOCOL_SYSTEM,
            "PancakeswapInfinityExecutor",
            abi.encode(
                0x238a358808379702088667322f80aC48bAd5e6c4,
                0xa0FfB9c1CE1Fe56963B0321B32E7A0302114058b,
                0xC697d2898e0D09264376196696c51D7aBbbAA4a9
            )
        );
    }
}
