// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {
    FallbackExecutor,
    FallbackExecutor__InvalidDataLength
} from "./FallbackExecutor.sol";
import {TychoFallbackRouter} from "../fallback/TychoFallbackRouter.sol";
import {PropAMMFallbackRouter} from "../fallback/PropAMMFallbackRouter.sol";

/// @title PropAMMFallbackExecutor
/// @notice Runs one swap through `PropAMMFallbackRouter`.
/// @dev Swap data is `[tokenIn: 20][tokenOut: 20][pamm: 20][fallback]`.
contract PropAMMFallbackExecutor is FallbackExecutor {
    constructor(address fallbackRouter_) FallbackExecutor(fallbackRouter_) {}

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        _validateDataLength(data);
        TychoFallbackRouter.Swap memory swap_ = TychoFallbackRouter.Swap({
            tokenIn: address(bytes20(data[0:20])),
            tokenOut: address(bytes20(data[20:40])),
            amountIn: amountIn,
            receiver: receiver
        });
        PropAMMFallbackRouter(fallbackRouter)
            .swap(swap_, address(bytes20(data[40:60])), data[60:]);
    }

    function _validateDataLength(bytes calldata data) internal pure override {
        if (data.length <= 60) {
            revert FallbackExecutor__InvalidDataLength(data.length);
        }
    }
}
