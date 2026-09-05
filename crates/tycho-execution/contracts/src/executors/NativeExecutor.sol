// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {TransferManager} from "../TransferManager.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

error NativeExecutor__InvalidDataLength();
error NativeExecutor__InvalidTarget();
error NativeExecutor__InvalidPayload();
error NativeExecutor__InvalidAmountIn();
error NativeExecutor__UnexpectedOverride();
error NativeExecutor__ZeroAddress();
error NativeExecutor__NotAContract();

contract NativeExecutor is IExecutor {
    using Address for address;

    address public immutable nativeRouterV4;

    // Native Router entrypoint:
    // tradeRFQT(RFQTQuote quote, uint256 actualSellerAmount, uint256 actualMinOutputAmount)
    bytes4 public constant TRADE_RFQT_SELECTOR = 0x0947c2d9;
    uint256 private constant _FIXED_HEADER_LENGTH = 92;
    uint256 private constant _MIN_TRADE_RFQT_CALLDATA_LENGTH = 4 + 3 * 32;
    // These positions are fixed by the pinned tradeRFQT selector: the selector
    // occupies 4 bytes, followed by three 32-byte ABI head words.
    uint256 private constant _ACTUAL_SELLER_AMOUNT_OFFSET = 4 + 32;
    uint256 private constant _ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET = 4 + 2 * 32;

    constructor(address _nativeRouterV4) {
        if (_nativeRouterV4 == address(0)) {
            revert NativeExecutor__ZeroAddress();
        }
        if (_nativeRouterV4.code.length == 0) {
            revert NativeExecutor__NotAContract();
        }
        nativeRouterV4 = _nativeRouterV4;
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

    function swap(
        uint256 amountIn,
        bytes calldata data,
        address /* receiver */
    )
        external
        payable
    {
        (
            address tokenIn,
            /* address tokenOut */,
            address target,
            uint256 signedAmountIn,
            bytes memory payload
        ) = _decodeData(data);

        if (!_isValidTarget(target)) {
            revert NativeExecutor__InvalidTarget();
        }

        // _decodeData guarantees at least four payload bytes, and truncating here
        // intentionally reads only the function selector.
        // forge-lint: disable-next-line(unsafe-typecast)
        bytes4 selector = bytes4(payload);
        if (selector != TRADE_RFQT_SELECTOR) {
            revert NativeExecutor__InvalidPayload();
        }

        // signedAmountIn is the Native quote baseline validated and encoded off-chain; amountIn is
        // the amount actually delivered by Tycho's Dispatcher. Native treats a zero
        // actualSellerAmount as "use the signed amount", so Tycho rejects zero amountIn rather
        // than unintentionally executing the signed amount.
        if (amountIn == 0 || signedAmountIn == 0) {
            revert NativeExecutor__InvalidAmountIn();
        }

        _validateOverrideArguments(payload);

        // For an exact fill, leave actualSellerAmount at zero so Native uses the signed amount. For
        // under- or over-delivery, pass the actual amount. When actualMinOutputAmount remains zero,
        // Native automatically adapts the signed slippage control and enforces its flexible-input
        // bounds:
        // https://docs.native.org/native-dev/build-with-native/swap-aggregators/firmquote-swap-apis/miscellaneous/compose-with-amm
        if (amountIn != signedAmountIn) {
            _setActualSellerAmount(payload, amountIn);
        }

        // amountIn is authoritative for the ETH forwarded during execution.
        uint256 executionValue = tokenIn == ETH_ADDRESS ? amountIn : 0;

        // slither-disable-next-line unused-return
        target.functionCallWithValue(payload, executionValue);
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            address tokenIn,
            address tokenOut,
            address target,
            uint256 signedAmountIn,
            bytes memory payload
        )
    {
        // Decode the 92-byte fixed header injected by NativeSwapEncoder.
        // 20 tokenIn + 20 tokenOut + 20 target + 32 signedAmountIn = 92 bytes.
        // The tradeRFQT payload must contain its 4-byte selector and three
        // 32-byte ABI head words. Its dynamic quote data remains opaque and is
        // validated by the Native Router.
        if (
            data.length < _FIXED_HEADER_LENGTH + _MIN_TRADE_RFQT_CALLDATA_LENGTH
        ) {
            revert NativeExecutor__InvalidDataLength();
        }

        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        target = address(bytes20(data[40:60]));
        signedAmountIn = uint256(bytes32(data[60:92]));

        // The remaining bytes are the opaque Native Router calldata
        payload = data[_FIXED_HEADER_LENGTH:];
    }

    function _validateOverrideArguments(bytes memory payload) private pure {
        uint256 actualSellerAmount;
        uint256 actualMinOutputAmount;
        assembly ("memory-safe") {
            actualSellerAmount := mload(
                add(add(payload, 0x20), _ACTUAL_SELLER_AMOUNT_OFFSET)
            )
            actualMinOutputAmount := mload(
                add(add(payload, 0x20), _ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET)
            )
        }

        // actualSellerAmount and actualMinOutputAmount are the only tradeRFQT
        // arguments not covered by the maker's signature, so require both to be
        // zero in the encoded payload.
        if (actualSellerAmount != 0 || actualMinOutputAmount != 0) {
            revert NativeExecutor__UnexpectedOverride();
        }
    }

    function _setActualSellerAmount(bytes memory payload, uint256 amountIn)
        private
        pure
    {
        assembly ("memory-safe") {
            mstore(
                add(add(payload, 0x20), _ACTUAL_SELLER_AMOUNT_OFFSET),
                amountIn
            )
        }
    }

    function _isValidTarget(address target) private view returns (bool) {
        return target == nativeRouterV4;
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
        address target;
        (tokenIn, tokenOut, target,,) = _decodeData(data);

        if (!_isValidTarget(target)) {
            revert NativeExecutor__InvalidTarget();
        }

        if (tokenIn == ETH_ADDRESS) {
            transferType = TransferManager.TransferType.TransferNativeInExecutor;
            // When transferring ETH in the executor, receiver doesn't need to be set
            // because the ETH stays in the Dispatcher until the executor is called with msg.value
            receiver = address(0);
        } else {
            transferType = TransferManager.TransferType.ProtocolWillDebit;
            receiver = target;
        }

        // Binding quotes use TychoRouter as their signed recipient. Dispatcher measures the output
        // there before forwarding it to the route's receiver.
        outputToRouter = true;
    }
}
