// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {
    FallbackExecutor,
    FallbackExecutor__InvalidDataLength
} from "./FallbackExecutor.sol";
import {TychoFallbackRouter} from "../fallback/TychoFallbackRouter.sol";
import {LiquoriceFallbackRouter} from "../fallback/LiquoriceFallbackRouter.sol";

/// @title LiquoriceFallbackExecutor
/// @notice Runs one swap through `LiquoriceFallbackRouter`.
contract LiquoriceFallbackExecutor is FallbackExecutor {
    uint256 private constant _SETTLEMENT_CALLDATA_START = 44;

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
        LiquoriceFallbackRouter(fallbackRouter)
            .swap(
                swap_,
                data[_SETTLEMENT_CALLDATA_START:fallbackOffset],
                data[fallbackOffset:]
            );
    }

    function _fallbackOffset(bytes calldata data)
        internal
        pure
        override
        returns (uint256 fallbackOffset)
    {
        if (data.length < _SETTLEMENT_CALLDATA_START) {
            revert FallbackExecutor__InvalidDataLength(data.length);
        }
        fallbackOffset = _SETTLEMENT_CALLDATA_START
            + uint32(bytes4(data[40:_SETTLEMENT_CALLDATA_START]));
        if (data.length <= fallbackOffset) {
            revert FallbackExecutor__InvalidDataLength(data.length);
        }
    }
}
