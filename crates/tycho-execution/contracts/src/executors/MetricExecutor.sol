// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {TransferManager} from "../TransferManager.sol";

interface IMetricPool {
    function swap(
        address receiver,
        bool zeroForOne,
        int128 amountSpecified,
        uint128 priceLimitX64,
        bytes calldata data
    ) external;
}

error MetricExecutor__InvalidDataLength();
error MetricExecutor__AmountInTooLarge();
error MetricExecutor__InvalidCallback();

contract MetricExecutor is IExecutor, ICallback {
    uint256 private constant _DATA_LENGTH = 61;
    uint256 private constant _INT128_MAX = uint256(uint128(type(int128).max));

    bytes4 private constant _METRIC_CALLBACK_SELECTOR = 0xc3251075;
    // Metric docs still mention both names, so keep accepting the older selector for now.
    bytes4 private constant _COOL_CALLBACK_SELECTOR = 0xa4b618b2;

    // keccak256("MetricExecutor#CURRENT_METRIC_POOL")
    bytes32 private constant _CURRENT_METRIC_POOL_SLOT =
        0x4d22bc52e1e7b4ceea27d9d7b99b3dc629ed6f14191a4839f79502a7ee831121;

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return msg.sender;
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address pool, bool zeroForOne) = _decodeSwapData(data);

        if (amountIn > _INT128_MAX) {
            revert MetricExecutor__AmountInTooLarge();
        }

        // We already checked that this fits in int128.
        // forge-lint: disable-next-line(unsafe-typecast)
        uint128 amountIn128 = uint128(amountIn);
        // We already checked that this fits in int128.
        // forge-lint: disable-next-line(unsafe-typecast)
        int128 amountSpecified = int128(amountIn128);

        _setCurrentPool(pool);
        IMetricPool(pool)
            .swap(
                receiver,
                zeroForOne,
                amountSpecified,
                zeroForOne ? 0 : type(uint128).max,
                ""
            );
        _setCurrentPool(address(0));
    }

    function handleCallback(bytes calldata data)
        public
        view
        returns (bytes memory)
    {
        _verifyCallbackSelector(data);
        if (msg.sender != _getCurrentPool()) {
            revert MetricExecutor__InvalidCallback();
        }
        return "";
    }

    function getTransferData(bytes calldata data)
        external
        pure
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        _validateDataLength(data);
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        return (
            TransferManager.TransferType.None,
            address(0),
            tokenIn,
            tokenOut,
            false
        );
    }

    function getCallbackTransferData(
        bytes calldata data,
        address, /* tokenIn */
        address caller
    )
        external
        view
        returns (TransferManager.TransferType transferType, address receiver)
    {
        _verifyCallbackSelector(data);
        transferType = TransferManager.TransferType.Transfer;
        receiver = caller;
    }

    function _decodeSwapData(bytes calldata data)
        internal
        pure
        returns (address pool, bool zeroForOne)
    {
        _validateDataLength(data);
        pool = address(bytes20(data[40:60]));
        zeroForOne = uint8(data[60]) > 0;
    }

    function _validateDataLength(bytes calldata data) internal pure {
        if (data.length != _DATA_LENGTH) {
            revert MetricExecutor__InvalidDataLength();
        }
    }

    function _verifyCallbackSelector(bytes calldata data) internal pure {
        if (data.length < 4) {
            revert MetricExecutor__InvalidCallback();
        }

        bytes4 selector = bytes4(data[:4]);
        if (
            selector != _METRIC_CALLBACK_SELECTOR
                && selector != _COOL_CALLBACK_SELECTOR
        ) {
            revert MetricExecutor__InvalidCallback();
        }
    }

    function _setCurrentPool(address pool) internal {
        bytes32 slot = _CURRENT_METRIC_POOL_SLOT;
        // slither-disable-next-line assembly
        assembly {
            tstore(slot, pool)
        }
    }

    function _getCurrentPool() internal view returns (address pool) {
        bytes32 slot = _CURRENT_METRIC_POOL_SLOT;
        // slither-disable-next-line assembly
        assembly {
            pool := tload(slot)
        }
    }
}
