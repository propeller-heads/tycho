// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";
import {
    IERC20,
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";

/// @title EuclidExecutor
/// @notice Executor for Euclid Protocol RFQ swaps
/// @dev Euclid quotes are maker-signed 1inch limit-order-protocol v3 RFQ
///      orders settled through the 1inch Aggregation Router v5
///      (fillOrderRFQTo). The firm quote carries the full settlement calldata;
///      this executor validates the target and selector, patches the filled
///      taker amount downward when an earlier route leg delivered less than
///      quoted, and forwards the call.
/// @dev Only supports single token in to single token out swaps
contract EuclidExecutor is IExecutor {
    using SafeERC20 for IERC20;
    using Address for address;

    /// @notice Euclid-specific errors
    error EuclidExecutor__InvalidDataLength();
    error EuclidExecutor__ZeroAddress();
    error EuclidExecutor__InvalidTarget();
    error EuclidExecutor__InvalidSelector();

    /// @notice AggregationRouterV5.fillOrderRFQTo
    bytes4 private constant _FILL_ORDER_RFQ_TO_SELECTOR = 0x5a099843;

    /// @notice The settlement router Euclid orders are filled through
    ///         (1inch Aggregation Router v5)
    address public immutable euclidSettlement;

    constructor(address euclidSettlement_) {
        if (euclidSettlement_ == address(0)) {
            revert EuclidExecutor__ZeroAddress();
        }
        euclidSettlement = euclidSettlement_;
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

    /// @notice Executes a swap through Euclid's RFQ system
    /// @param amountIn The amount of input token to swap
    /// @param data Encoded swap data containing tokens and settlement calldata
    /// @param receiver The address to receive output tokens
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        address target;
        uint8 partialFillOffset;
        uint256 originalFilledTakerAmount;
        bytes memory euclidCalldata;
        (target, partialFillOffset, originalFilledTakerAmount, euclidCalldata) =
            _decodeData(data);

        // Only allow calling the settlement router, and only its RFQ fill
        // function.
        _validateCall(target, bytes4(euclidCalldata));

        // Cap the filled taker amount at what earlier route legs actually
        // delivered. If unchanged, the original calldata is returned.
        bytes memory finalCalldata = _modifyFilledTakerAmount(
            euclidCalldata,
            amountIn,
            originalFilledTakerAmount,
            partialFillOffset
        );

        // slither-disable-next-line unused-return
        target.functionCall(finalCalldata);
    }

    /// @dev Reverts unless target is the settlement router and the selector is
    ///      the RFQ fill function.
    function _validateCall(address target, bytes4 selector) internal view {
        if (target != euclidSettlement) {
            revert EuclidExecutor__InvalidTarget();
        }
        if (selector != _FILL_ORDER_RFQ_TO_SELECTOR) {
            revert EuclidExecutor__InvalidSelector();
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
            bytes memory euclidCalldata
        )
    {
        // Need at least 93 bytes for the minimum fixed fields
        // 20 (tokenIn) + 20 (tokenOut) + 20 (target) + 1 (offset) +
        // 32 (amount) = 93
        if (data.length < 93) revert EuclidExecutor__InvalidDataLength();

        target = address(bytes20(data[40:60]));
        partialFillOffset = uint8(data[60]);
        originalFilledTakerAmount = uint256(bytes32(data[61:93]));
        euclidCalldata = data[93:];
    }

    /// @dev Patches the filledTakerAmount (the fillOrderRFQTo flagsAndAmount
    ///      word — plain amount, no flags) to handle slippage from earlier
    ///      route legs. Only ever patches DOWN: the signed order carries the
    ///      quoted amounts and the maker never over-delivers.
    /// @param euclidCalldata The original settlement calldata
    /// @param amountIn The actual amount available from the router
    /// @param originalFilledTakerAmount The amount quoted when the order was signed
    /// @param partialFillOffset The word index of filledTakerAmount in the calldata
    /// @return The calldata with the filledTakerAmount updated if needed
    function _modifyFilledTakerAmount(
        bytes memory euclidCalldata,
        uint256 amountIn,
        uint256 originalFilledTakerAmount,
        uint8 partialFillOffset
    ) internal pure returns (bytes memory) {
        // Position = 4 bytes (selector) + offset * 32 bytes
        uint256 filledTakerAmountPos = 4 + uint256(partialFillOffset) * 32;

        // Cap the fill amount at what we actually have available
        uint256 newFilledTakerAmount = originalFilledTakerAmount > amountIn
            ? amountIn
            : originalFilledTakerAmount;

        if (newFilledTakerAmount == originalFilledTakerAmount) {
            return euclidCalldata;
        }

        // slither-disable-next-line assembly
        assembly {
            let dataPtr := add(euclidCalldata, 0x20)
            let actualPos := add(dataPtr, filledTakerAmountPos)
            mstore(actualPos, newFilledTakerAmount)
        }

        return euclidCalldata;
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
            revert EuclidExecutor__InvalidDataLength();
        }

        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        address target = address(bytes20(data[40:60]));
        if (target != euclidSettlement) {
            revert EuclidExecutor__InvalidTarget();
        }
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        // The settlement router pulls tokenIn from the caller via
        // transferFrom, so the approval must go to it.
        receiver = target;
        outputToRouter = true;
    }
}
