// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {
    FallbackExecutor,
    FallbackExecutor__InvalidDataLength
} from "./FallbackExecutor.sol";
import {TychoFallbackRouter} from "../fallback/TychoFallbackRouter.sol";
import {MetricFallbackRouter} from "../fallback/MetricFallbackRouter.sol";

/// @title MetricFallbackExecutor
/// @notice Runs one swap through `MetricFallbackRouter`.
contract MetricFallbackExecutor is FallbackExecutor {
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
        MetricFallbackRouter(fallbackRouter)
            .swap(
                swap_,
                address(bytes20(data[40:60])),
                data[60:fallbackOffset],
                data[fallbackOffset:]
            );
    }

    function _fallbackOffset(bytes calldata data)
        internal
        pure
        override
        returns (uint256)
    {
        if (data.length <= 61) {
            revert FallbackExecutor__InvalidDataLength(data.length);
        }
        return 61;
    }
}
