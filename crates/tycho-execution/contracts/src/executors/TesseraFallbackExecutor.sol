// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {
    FallbackExecutor,
    FallbackExecutor__InvalidDataLength
} from "./FallbackExecutor.sol";
import {TychoFallbackRouter} from "../fallback/TychoFallbackRouter.sol";
import {TesseraFallbackRouter} from "../fallback/TesseraFallbackRouter.sol";

/// @title TesseraFallbackExecutor
/// @notice Runs one swap through `TesseraFallbackRouter`.
/// @dev Swap data is `[tokenIn: 20][tokenOut: 20][fallback]`: Tessera finds the pair by its
/// tokens, so the fallback starts right after them.
contract TesseraFallbackExecutor is FallbackExecutor {
    // tokenIn (20) + tokenOut (20)
    uint256 private constant _FALLBACK_SWAP_START = 40;

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
        TesseraFallbackRouter(fallbackRouter).swap(swap_, data[fallbackOffset:]);
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
