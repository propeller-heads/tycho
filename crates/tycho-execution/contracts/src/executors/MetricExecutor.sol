// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
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
error MetricExecutor__InvalidOracleUpdateFlag();
error MetricExecutor__AmountInTooLarge();
error MetricExecutor__PriceLimitTooLarge();
error MetricExecutor__InvalidCallback();

contract MetricExecutor is IExecutor, ICallback {
    using Address for address;

    uint256 private constant _BASE_DATA_LENGTH = 114;
    uint256 private constant _ORACLE_UPDATE_HEADER_LENGTH = 24;
    uint256 private constant _INT128_MAX = uint256(uint128(type(int128).max));

    bytes4 private constant _METRIC_CALLBACK_SELECTOR = 0xc3251075;
    // The docs currently mention both names; accept the older selector while
    // Metric finalizes the public callback naming.
    bytes4 private constant _COOL_CALLBACK_SELECTOR = 0xa4b618b2;

    bytes32 private constant _CURRENT_METRIC_POOL_SLOT =
        0x680ea5c1857f782d9caf44a6f1020e0d3bb55355ec15df645080b605b3154b99;

    constructor() {}

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
        (
            address pool,
            bool zeroForOne,
            uint128 priceLimitX64,
            bool shouldUpdateOracle,
            address oracleTarget,
            bytes calldata oracleCalldata
        ) = _decodeSwapData(data);

        if (amountIn > _INT128_MAX) {
            revert MetricExecutor__AmountInTooLarge();
        }

        if (shouldUpdateOracle) {
            oracleTarget.functionCall(oracleCalldata);
        }

        _setCurrentPool(pool);
        // Checked above against int128 max.
        // forge-lint: disable-next-line(unsafe-typecast)
        uint128 amountIn128 = uint128(amountIn);
        // Checked above against int128 max.
        // forge-lint: disable-next-line(unsafe-typecast)
        int128 amountSpecified = int128(amountIn128);
        IMetricPool(pool)
            .swap(receiver, zeroForOne, amountSpecified, priceLimitX64, "");
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
        returns (
            address pool,
            bool zeroForOne,
            uint128 priceLimitX64,
            bool shouldUpdateOracle,
            address oracleTarget,
            bytes calldata oracleCalldata
        )
    {
        uint256 oracleCalldataLength = _validateDataLength(data);
        pool = address(bytes20(data[40:60]));
        zeroForOne = uint8(data[80]) > 0;

        uint256 rawPriceLimit = uint256(bytes32(data[81:113]));
        if (rawPriceLimit > type(uint128).max) {
            revert MetricExecutor__PriceLimitTooLarge();
        }
        // Checked above against uint128 max.
        // forge-lint: disable-next-line(unsafe-typecast)
        priceLimitX64 = uint128(rawPriceLimit);

        shouldUpdateOracle = uint8(data[113]) == 1;
        oracleCalldata = data[data.length:data.length];
        if (shouldUpdateOracle) {
            oracleTarget = address(bytes20(data[114:134]));
            oracleCalldata = data[
                _BASE_DATA_LENGTH
                    + _ORACLE_UPDATE_HEADER_LENGTH:
                    _BASE_DATA_LENGTH + _ORACLE_UPDATE_HEADER_LENGTH
                        + oracleCalldataLength
            ];
        }
    }

    function _validateDataLength(bytes calldata data)
        internal
        pure
        returns (uint256 oracleCalldataLength)
    {
        if (data.length < _BASE_DATA_LENGTH) {
            revert MetricExecutor__InvalidDataLength();
        }

        uint8 oracleUpdateFlag = uint8(data[113]);
        if (oracleUpdateFlag > 1) {
            revert MetricExecutor__InvalidOracleUpdateFlag();
        }

        if (oracleUpdateFlag == 0) {
            if (data.length != _BASE_DATA_LENGTH) {
                revert MetricExecutor__InvalidDataLength();
            }
            return 0;
        }

        uint256 minLength = _BASE_DATA_LENGTH + _ORACLE_UPDATE_HEADER_LENGTH;
        if (data.length < minLength) {
            revert MetricExecutor__InvalidDataLength();
        }

        oracleCalldataLength = uint32(bytes4(data[134:138]));
        if (data.length != minLength + oracleCalldataLength) {
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
