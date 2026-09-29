// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

/// @notice The Bebop call checks and calldata rewrite shared by `BebopExecutor` and
/// `BebopFallbackRouter`.
library BebopCalldata {
    /// @notice Why `checkCall` rejects a call.
    enum CallCheck {
        Valid,
        InvalidTarget,
        InvalidSelector
    }

    /// @notice BebopSettlement.swapSingle
    bytes4 internal constant SWAP_SINGLE_SELECTOR = 0x4dcebcba;
    /// @notice BebopSettlement.swapAggregate
    bytes4 internal constant SWAP_AGGREGATE_SELECTOR = 0xa2f74893;
    /// @notice BebopRouter.swap
    bytes4 internal constant ROUTER_SWAP_SELECTOR = 0x9586d0e8;

    /// @notice Allows only the settlement and router contracts, each with only its swap
    /// selectors.
    function checkCall(
        address target,
        bytes4 selector,
        address settlement,
        address router
    ) internal pure returns (CallCheck) {
        if (target == settlement) {
            return selector == SWAP_SINGLE_SELECTOR
                || selector == SWAP_AGGREGATE_SELECTOR
                ? CallCheck.Valid
                : CallCheck.InvalidSelector;
        }
        if (target == router) {
            return selector == ROUTER_SWAP_SELECTOR
                ? CallCheck.Valid
                : CallCheck.InvalidSelector;
        }
        return CallCheck.InvalidTarget;
    }

    /// @notice Caps the calldata's `filledTakerAmount` at `amountIn`, in place.
    /// @param partialFillOffset The Bebop API's offset of `filledTakerAmount`, in 32-byte words
    /// after the selector.
    /// @return The same `bebopCalldata`, rewritten only when `amountIn` is below
    /// `originalFilledTakerAmount`.
    function capFilledTakerAmount(
        bytes memory bebopCalldata,
        uint256 amountIn,
        uint256 originalFilledTakerAmount,
        uint8 partialFillOffset
    ) internal pure returns (bytes memory) {
        if (amountIn >= originalFilledTakerAmount) {
            return bebopCalldata;
        }

        uint256 filledTakerAmountPos = 4 + uint256(partialFillOffset) * 32;
        // slither-disable-next-line assembly
        assembly {
            mstore(
                add(add(bebopCalldata, 0x20), filledTakerAmountPos),
                amountIn
            )
        }
        return bebopCalldata;
    }
}
