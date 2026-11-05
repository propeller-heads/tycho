// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {
    FallbackExecutor,
    FallbackExecutor__InvalidDataLength
} from "./FallbackExecutor.sol";
import {TychoFallbackRouter} from "../fallback/TychoFallbackRouter.sol";
import {HashflowFallbackRouter} from "../fallback/HashflowFallbackRouter.sol";

/// @title HashflowFallbackExecutor
/// @notice Runs one swap through `HashflowFallbackRouter`.
contract HashflowFallbackExecutor is FallbackExecutor {
    constructor(address fallbackRouter_) FallbackExecutor(fallbackRouter_) {}

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        uint256 fallbackOffset = _fallbackOffset(data);
        TychoFallbackRouter.Swap memory swap_ = TychoFallbackRouter.Swap({
            tokenIn: address(bytes20(data[0:20])),
            tokenOut: address(bytes20(data[20:40])),
            amountIn: amountIn,
            receiver: receiver
        });
        HashflowFallbackRouter(fallbackRouter)
            .swap(swap_, data[40:fallbackOffset], data[fallbackOffset:]);
    }

    function _fallbackOffset(bytes calldata data)
        internal
        pure
        override
        returns (uint256)
    {
        if (data.length <= 385) {
            revert FallbackExecutor__InvalidDataLength(data.length);
        }
        return 385;
    }
}
