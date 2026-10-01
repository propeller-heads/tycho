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
        uint256 fallbackOffset = _fallbackOffset(data);
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
                data[_BEBOP_DATA_START:fallbackOffset],
                data[fallbackOffset:]
            );
    }

    function _fallbackOffset(bytes calldata data)
        internal
        pure
        override
        returns (uint256 fallbackOffset)
    {
        if (data.length < _BEBOP_DATA_START) {
            revert FallbackExecutor__InvalidDataLength(data.length);
        }
        fallbackOffset =
            _BEBOP_DATA_START + uint32(bytes4(data[60:_BEBOP_DATA_START]));
        if (data.length <= fallbackOffset) {
            revert FallbackExecutor__InvalidDataLength(data.length);
        }
    }
}
