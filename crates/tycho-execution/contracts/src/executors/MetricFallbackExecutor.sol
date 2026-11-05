// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
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
    // tokenIn (20) + tokenOut (20) + pool (20) + Metric data (1)
    uint256 private constant _FALLBACK_SWAP_START = 61;

    constructor(address fallbackRouter_) FallbackExecutor(fallbackRouter_) {}

    // slither-disable-next-line locked-ether The router delegatecalls swap, so any ETH it receives is held by the router.
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
        if (data.length <= _FALLBACK_SWAP_START) {
            revert FallbackExecutor__InvalidDataLength(data.length);
        }
        return _FALLBACK_SWAP_START;
    }
}
