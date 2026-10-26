// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";

/// @title EuclidExecutor
/// @notice Executor for Euclid Protocol RFQ swaps
/// @dev Euclid quotes are maker-signed 1inch limit-order-protocol v3 RFQ
///      orders settled through the 1inch Aggregation Router v5
///      (fillOrderRFQTo). The firm quote carries the full fill calldata; this
///      executor forwards it to the pinned router, after lowering the filled
///      taker amount when an earlier route leg delivered less than quoted. The
///      output goes to the target signed into the calldata, which is the
///      router that runs this executor.
/// @dev Only supports single token in to single token out swaps
contract EuclidExecutor is IExecutor {
    using Address for address;

    /// @notice Euclid-specific errors
    error EuclidExecutor__InvalidDataLength();
    error EuclidExecutor__ZeroAddress();
    error EuclidExecutor__InvalidSelector();

    /// @notice AggregationRouterV5.fillOrderRFQTo
    bytes4 public constant FILL_ORDER_RFQ_TO_SELECTOR = 0x5a099843;

    // tokenIn (20) + tokenOut (20) + originalFilledTakerAmount (32)
    uint256 private constant _FIXED_HEADER_LENGTH = 72;
    // fillOrderRFQTo(OrderRFQ order, bytes signature, uint256 flagsAndAmount,
    // address target): the seven order words come inline, then the
    // signature's offset, flagsAndAmount and target. The amount sits in word
    // 8 after the selector, and no call is shorter than its ten head words.
    uint256 private constant _FLAGS_AND_AMOUNT_OFFSET = 4 + 8 * 32;
    uint256 private constant _MIN_FILL_CALLDATA_LENGTH = 4 + 10 * 32;

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
    /// @dev The output goes to the target signed into the calldata, so the
    ///      receiver argument is not read.
    function swap(
        uint256 amountIn,
        bytes calldata data,
        address /* receiver */
    )
        external
        payable
    {
        (,, uint256 originalFilledTakerAmount, bytes memory fillCalldata) =
            _decodeData(data);

        // Only the router's RFQ fill function.
        if (bytes4(fillCalldata) != FILL_ORDER_RFQ_TO_SELECTOR) {
            revert EuclidExecutor__InvalidSelector();
        }

        // Cap the filled taker amount at what earlier route legs actually
        // delivered. Only ever downward: the signed order carries the quoted
        // amounts and the maker never over-delivers.
        if (amountIn < originalFilledTakerAmount) {
            _setFilledTakerAmount(fillCalldata, amountIn);
        }

        // slither-disable-next-line unused-return
        euclidSettlement.functionCall(fillCalldata);
    }

    /// @dev Decodes the packed calldata
    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            address tokenIn,
            address tokenOut,
            uint256 originalFilledTakerAmount,
            bytes memory fillCalldata
        )
    {
        if (data.length < _FIXED_HEADER_LENGTH + _MIN_FILL_CALLDATA_LENGTH) {
            revert EuclidExecutor__InvalidDataLength();
        }
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        originalFilledTakerAmount = uint256(bytes32(data[40:72]));
        fillCalldata = data[_FIXED_HEADER_LENGTH:];
    }

    /// @dev Writes `amount` over the fillOrderRFQTo flagsAndAmount word, a
    ///      plain amount with no flags. The length check in `_decodeData`
    ///      keeps the word inside the calldata.
    function _setFilledTakerAmount(bytes memory fillCalldata, uint256 amount)
        private
        pure
    {
        assembly ("memory-safe") {
            mstore(
                add(add(fillCalldata, 0x20), _FLAGS_AND_AMOUNT_OFFSET),
                amount
            )
        }
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
        (tokenIn, tokenOut,,) = _decodeData(data);
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        // The settlement router pulls tokenIn from the caller via
        // transferFrom, so the approval must go to it.
        receiver = euclidSettlement;
        outputToRouter = true;
    }
}
