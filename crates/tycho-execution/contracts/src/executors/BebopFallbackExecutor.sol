// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {
    FallbackExecutor,
    FallbackExecutor__InvalidDataLength
} from "./FallbackExecutor.sol";
import {TychoFallbackRouter} from "../fallback/TychoFallbackRouter.sol";
import {BebopFallbackRouter} from "../fallback/BebopFallbackRouter.sol";

/// @title BebopFallbackExecutor
/// @notice Runs one swap through `BebopFallbackRouter`.
contract BebopFallbackExecutor is FallbackExecutor {
    uint256 private constant _BEBOP_DATA_START = 64;

    constructor(address fallbackRouter_) FallbackExecutor(fallbackRouter_) {}

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        uint256 fallbackStart = _validateAndFindFallback(data);
        TychoFallbackRouter.Swap memory swap_ = TychoFallbackRouter.Swap({
            tokenIn: address(bytes20(data[0:20])),
            tokenOut: address(bytes20(data[20:40])),
            amountIn: amountIn,
            receiver: receiver
        });
        BebopFallbackRouter(fallbackRouter)
            .swap(
                swap_,
                address(bytes20(data[40:60])),
                data[_BEBOP_DATA_START:fallbackStart],
                data[fallbackStart:]
            );
    }

    function _validateDataLength(bytes calldata data) internal pure override {
        _validateAndFindFallback(data);
    }

    /// @dev Reverts unless a fallback follows `bebopData`.
    function _validateAndFindFallback(bytes calldata data)
        internal
        pure
        returns (uint256 fallbackStart)
    {
        if (data.length < _BEBOP_DATA_START) {
            revert FallbackExecutor__InvalidDataLength(data.length);
        }
        fallbackStart =
            _BEBOP_DATA_START + uint32(bytes4(data[60:_BEBOP_DATA_START]));
        if (data.length <= fallbackStart) {
            revert FallbackExecutor__InvalidDataLength(data.length);
        }
    }
}
