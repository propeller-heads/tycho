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
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

/// @title LiquoriceExecutor
/// @notice Executor for Liquorice RFQ (Request for Quote) swaps
/// @dev Handles RFQ swaps through Liquorice settlement contracts
///      with support for partial fills and dynamic allowance management
contract LiquoriceExecutor is IExecutor {
    using SafeERC20 for IERC20;
    using Address for address;

    error LiquoriceExecutor__InvalidDataLength();
    error LiquoriceExecutor__ZeroAddress();
    error LiquoriceExecutor__NotAContract();
    error LiquoriceExecutor__AmountBelowMinimum();
    error LiquoriceExecutor__InvalidSelector();

    bytes4 private constant _SETTLE_SINGLE_SELECTOR = 0x9935c868;
    bytes4 private constant _SETTLE_SELECTOR = 0xcba673a7;

    /// @notice The Liquorice settlement contract address
    address public immutable liquoriceSettlement;

    /// @notice The Liquorice balance manager contract address
    address public immutable liquoriceBalanceManager;

    constructor(
        address _liquoriceSettlement,
        address _liquoriceBalanceManager
    ) {
        if (
            _liquoriceSettlement == address(0)
                || _liquoriceBalanceManager == address(0)
        ) {
            revert LiquoriceExecutor__ZeroAddress();
        }
        if (
            _liquoriceSettlement.code.length == 0
                || _liquoriceBalanceManager.code.length == 0
        ) {
            revert LiquoriceExecutor__NotAContract();
        }
        liquoriceSettlement = _liquoriceSettlement;
        liquoriceBalanceManager = _liquoriceBalanceManager;
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

    /// @notice Executes a swap through Liquorice's RFQ system
    /// @param amountIn The amount of input token to swap
    /// @param data Encoded swap data containing tokens and liquorice calldata
    /// @param receiver The address to receive output tokens
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (
            address tokenIn,
            uint32 partialFillOffset,
            uint256 originalBaseTokenAmount,
            uint256 minBaseTokenAmount,
            bytes memory liquoriceCalldata
        ) = _decodeData(data);

        amountIn =
            _clampAmount(amountIn, originalBaseTokenAmount, minBaseTokenAmount);

        bytes4 selector = bytes4(liquoriceCalldata);
        if (selector != _SETTLE_SINGLE_SELECTOR && selector != _SETTLE_SELECTOR)
        {
            revert LiquoriceExecutor__InvalidSelector();
        }

        // Modify the fill amount in the calldata if partial fill is
        // supported. If partialFillOffset is 0, partial fill is not
        // supported.
        bytes memory finalCalldata = liquoriceCalldata;
        if (partialFillOffset > 0 && originalBaseTokenAmount > amountIn) {
            finalCalldata = _modifyFilledTakerAmount(
                liquoriceCalldata, amountIn, partialFillOffset
            );
        }

        uint256 ethValue = tokenIn == ETH_ADDRESS ? amountIn : 0;

        // Execute the swap by forwarding calldata to settlement contract
        // slither-disable-next-line unused-return
        liquoriceSettlement.functionCallWithValue(finalCalldata, ethValue);
    }

    /// @dev Decodes the packed calldata
    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            address tokenIn,
            uint32 partialFillOffset,
            uint256 originalBaseTokenAmount,
            uint256 minBaseTokenAmount,
            bytes memory liquoriceCalldata
        )
    {
        // Minimum fixed fields:
        // tokenIn (20) + tokenOut (20) + partialFillOffset (4) +
        // originalBaseTokenAmount (32) + minBaseTokenAmount (32) +
        // liquoriceCalldata selector (4) = 112 bytes
        if (data.length < 112) {
            revert LiquoriceExecutor__InvalidDataLength();
        }

        tokenIn = address(bytes20(data[0:20]));
        partialFillOffset = uint32(bytes4(data[40:44]));
        originalBaseTokenAmount = uint256(bytes32(data[44:76]));
        minBaseTokenAmount = uint256(bytes32(data[76:108]));
        liquoriceCalldata = data[108:];
    }

    /// @dev Clamps the given amount to be within the valid range
    function _clampAmount(
        uint256 givenAmount,
        uint256 originalBaseTokenAmount,
        uint256 minBaseTokenAmount
    ) internal pure returns (uint256) {
        if (givenAmount < minBaseTokenAmount) {
            revert LiquoriceExecutor__AmountBelowMinimum();
        }
        if (givenAmount > originalBaseTokenAmount) {
            return originalBaseTokenAmount;
        }
        return givenAmount;
    }

    /// @dev Modifies the filledTakerAmount in the liquorice calldata
    /// @param liquoriceCalldata The original calldata for the liquorice settlement
    /// @param givenAmount The actual amount available from the router
    /// @param partialFillOffset The offset from Liquorice API indicating where the fill amount is located
    /// @return The modified calldata with updated fill amount
    function _modifyFilledTakerAmount(
        bytes memory liquoriceCalldata,
        uint256 givenAmount,
        uint32 partialFillOffset
    ) internal pure returns (bytes memory) {
        // Use the offset from Liquorice API to locate the fill amount
        // Position = 4 bytes (selector) + offset bytes
        uint256 fillAmountPos = 4 + uint256(partialFillOffset);

        // slither-disable-next-line assembly
        assembly {
            let dataPtr := add(liquoriceCalldata, 0x20)
            let actualPos := add(dataPtr, fillAmountPos)
            mstore(actualPos, givenAmount)
        }

        return liquoriceCalldata;
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
        if (data.length < 112) {
            revert LiquoriceExecutor__InvalidDataLength();
        }

        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        receiver = liquoriceBalanceManager;
        outputToRouter = true;
    }
}
