// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {
    FallbackExecutor,
    FallbackExecutor__InvalidDataLength
} from "./FallbackExecutor.sol";
import {TychoFallbackRouter} from "../fallback/TychoFallbackRouter.sol";
import {NativeFallbackRouter} from "../fallback/NativeFallbackRouter.sol";

/// @title NativeFallbackExecutor
/// @notice Runs one swap through `NativeFallbackRouter`.
contract NativeFallbackExecutor is FallbackExecutor {
    uint256 private constant _NATIVE_DATA_START = 44;

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
        NativeFallbackRouter(fallbackRouter)
            .swap(
                swap_,
                data[_NATIVE_DATA_START:fallbackOffset],
                data[fallbackOffset:]
            );
    }

    function _fallbackOffset(bytes calldata data)
        internal
        pure
        override
        returns (uint256 fallbackOffset)
    {
        if (data.length < _NATIVE_DATA_START) {
            revert FallbackExecutor__InvalidDataLength(data.length);
        }
        fallbackOffset =
            _NATIVE_DATA_START + uint32(bytes4(data[40:_NATIVE_DATA_START]));
        if (data.length <= fallbackOffset) {
            revert FallbackExecutor__InvalidDataLength(data.length);
        }
    }
}
