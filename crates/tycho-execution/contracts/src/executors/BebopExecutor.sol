// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";
import {Math} from "@openzeppelin/contracts/utils/math/Math.sol";
import {
    IERC20,
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";

/// @title BebopExecutor
/// @notice Executor for Bebop PMM RFQ (Request for Quote) swaps
/// @dev Handles Single and Aggregate RFQ swaps through either the Bebop
///      settlement contract or the Bebop router contract, selected per-quote
/// @dev Only supports single token in to single token out swaps
contract BebopExecutor is IExecutor {
    using Math for uint256;
    using SafeERC20 for IERC20;
    using Address for address;

    /// @notice Bebop-specific errors
    error BebopExecutor__InvalidDataLength();
    error BebopExecutor__ZeroAddress();
    error BebopExecutor__InvalidTarget();
    error BebopExecutor__InvalidSelector();

    /// @notice BebopSettlement.swapSingle
    bytes4 private constant _SWAP_SINGLE_SELECTOR = 0x4dcebcba;
    /// @notice BebopSettlement.swapAggregate
    bytes4 private constant _SWAP_AGGREGATE_SELECTOR = 0xa2f74893;
    /// @notice BebopRouter.swap
    bytes4 private constant _ROUTER_SWAP_SELECTOR = 0x9586d0e8;

    /// @notice The Bebop settlement contract address
    address public immutable bebopSettlement;

    /// @notice The Bebop router contract address
    address public immutable bebopRouter;

    constructor(address bebopSettlement_, address bebopRouter_) {
        if (bebopSettlement_ == address(0) || bebopRouter_ == address(0)) {
            revert BebopExecutor__ZeroAddress();
        }
        bebopSettlement = bebopSettlement_;
        bebopRouter = bebopRouter_;
    }

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return msg.sender;
    }

    /// @notice Executes a swap through Bebop's PMM RFQ system
    /// @param amountIn The amount of input token to swap
    /// @param data Encoded swap data containing tokens and bebop calldata
    /// @param receiver The address to receive output tokens
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        address target;
        uint8 partialFillOffset;
        uint256 originalFilledTakerAmount;
        bytes memory bebopCalldata;
        (target, partialFillOffset, originalFilledTakerAmount, bebopCalldata) =
            _decodeData(data);

        // Only allow calling the settlement or router contract, and only with
        // the swap selectors each one exposes.
        _validateCall(target, bytes4(bebopCalldata));

        // Modify the filledTakerAmount in the calldata
        // If the filledTakerAmount is the same as the original, the original calldata is returned
        bytes memory finalCalldata = _modifyFilledTakerAmount(
            bebopCalldata,
            amountIn,
            originalFilledTakerAmount,
            partialFillOffset
        );

        // Use OpenZeppelin's Address library for safe call
        // This will revert if the call fails
        // slither-disable-next-line unused-return
        target.functionCall(finalCalldata);
    }

    /// @dev Reverts unless target is an allowed contract and the selector is
    ///      one that target exposes for swaps.
    function _validateCall(address target, bytes4 selector) internal view {
        if (target == bebopSettlement) {
            if (
                selector != _SWAP_SINGLE_SELECTOR
                    && selector != _SWAP_AGGREGATE_SELECTOR
            ) {
                revert BebopExecutor__InvalidSelector();
            }
        } else if (target == bebopRouter) {
            if (selector != _ROUTER_SWAP_SELECTOR) {
                revert BebopExecutor__InvalidSelector();
            }
        } else {
            revert BebopExecutor__InvalidTarget();
        }
    }

    /// @dev Decodes the packed calldata
    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            address target,
            uint8 partialFillOffset,
            uint256 originalFilledTakerAmount,
            bytes memory bebopCalldata
        )
    {
        // Need at least 93 bytes for the minimum fixed fields
        // 20 (tokenIn) + 20 (tokenOut) + 20 (target) + 1 (offset) +
        // 32 (amount) = 93
        if (data.length < 93) revert BebopExecutor__InvalidDataLength();

        target = address(bytes20(data[40:60]));
        partialFillOffset = uint8(data[60]);
        originalFilledTakerAmount = uint256(bytes32(data[61:93]));
        bebopCalldata = data[93:];
    }

    /// @dev Modifies the filledTakerAmount in the bebop calldata to handle slippage
    /// @param bebopCalldata The original calldata for the bebop settlement
    /// @param amountIn The actual amount available from the router
    /// @param originalFilledTakerAmount The original amount expected when the quote was generated
    /// @param partialFillOffset The offset from Bebop API indicating where filledTakerAmount is located
    /// @return The modified calldata with updated filledTakerAmount
    function _modifyFilledTakerAmount(
        bytes memory bebopCalldata,
        uint256 amountIn,
        uint256 originalFilledTakerAmount,
        uint8 partialFillOffset
    ) internal pure returns (bytes memory) {
        // Use the offset from Bebop API to locate filledTakerAmount
        // Position = 4 bytes (selector) + offset * 32 bytes
        uint256 filledTakerAmountPos = 4 + uint256(partialFillOffset) * 32;

        // Cap the fill amount at what we actually have available
        uint256 newFilledTakerAmount = originalFilledTakerAmount > amountIn
            ? amountIn
            : originalFilledTakerAmount;

        // If the new filledTakerAmount is the same as the original, return the original calldata
        if (newFilledTakerAmount == originalFilledTakerAmount) {
            return bebopCalldata;
        }

        // Use assembly to modify the filledTakerAmount at the correct position
        // slither-disable-next-line assembly
        assembly {
            // Get pointer to the data portion of the bytes array
            let dataPtr := add(bebopCalldata, 0x20)

            // Calculate the actual position and store the new value
            let actualPos := add(dataPtr, filledTakerAmountPos)
            mstore(actualPos, newFilledTakerAmount)
        }

        return bebopCalldata;
    }

    /**
     * @dev Allow receiving ETH for settlement calls that require ETH
     * This is needed when the executor handles native ETH swaps
     * In production, ETH typically comes from router or settlement contracts
     * In tests, it may come from EOA addresses via the test harness
     */
    receive() external payable {
        // Allow ETH transfers for Bebop settlement functionality
    }

    function getTransferData(bytes calldata data)
        external
        view
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        if (data.length < 93) {
            revert BebopExecutor__InvalidDataLength();
        }

        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        address target = address(bytes20(data[40:60]));
        if (target != bebopSettlement && target != bebopRouter) {
            revert BebopExecutor__InvalidTarget();
        }
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        // Both the settlement and the router pull tokenIn from the caller via
        // transferFrom, so the approval must go to whichever one we call.
        receiver = target;
        outputToRouter = true;
    }
}
