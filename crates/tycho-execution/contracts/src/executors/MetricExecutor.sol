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

interface IMetricOracle {
    function updateBySignature(
        address feedCreator,
        uint256 deadline,
        uint256 newSlotValue,
        bytes calldata signature
    ) external;
}

error MetricExecutor__InvalidDataLength();
error MetricExecutor__InvalidOracleUpdateFlag();
error MetricExecutor__AmountInTooLarge();
error MetricExecutor__InvalidCallback();
error MetricExecutor__InvalidOracle();

contract MetricExecutor is IExecutor, ICallback {
    uint256 private constant _BASE_DATA_LENGTH = 62;
    uint256 private constant _ORACLE_ARGS_HEADER_LENGTH = 4;
    uint256 private constant _INT128_MAX = uint256(uint128(type(int128).max));
    // Keep this lined up with the MetricOracleUpdatePolicy byte from the Rust swap encoder.
    enum OracleUpdateMode {
        Never,
        Always,
        RetryOnRevert
    }

    bytes4 private constant _METRIC_CALLBACK_SELECTOR = 0xc3251075;
    // Metric docs still mention both names, so keep accepting the older selector for now.
    bytes4 private constant _COOL_CALLBACK_SELECTOR = 0xa4b618b2;

    // keccak256("MetricExecutor#CURRENT_METRIC_POOL")
    bytes32 private constant _CURRENT_METRIC_POOL_SLOT =
        0x4d22bc52e1e7b4ceea27d9d7b99b3dc629ed6f14191a4839f79502a7ee831121;

    IMetricOracle public immutable oracle;

    constructor(address oracle_) {
        if (oracle_ == address(0)) {
            revert MetricExecutor__InvalidOracle();
        }
        oracle = IMetricOracle(oracle_);
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

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (
            address pool,
            bool zeroForOne,
            OracleUpdateMode oracleUpdateMode,
            bytes calldata oracleArgs
        ) = _decodeSwapData(data);

        if (amountIn > _INT128_MAX) {
            revert MetricExecutor__AmountInTooLarge();
        }

        if (oracleUpdateMode == OracleUpdateMode.Always) {
            // Always mode updates the configured Metric oracle before the first swap.
            // Keep the target on the immutable instead of user data since this executor runs
            // through TychoRouter delegatecall.
            _updateOracle(oracleArgs);
        }

        // We already checked that this fits in int128.
        // forge-lint: disable-next-line(unsafe-typecast)
        uint128 amountIn128 = uint128(amountIn);
        // We already checked that this fits in int128.
        // forge-lint: disable-next-line(unsafe-typecast)
        int128 amountSpecified = int128(amountIn128);

        if (oracleUpdateMode == OracleUpdateMode.RetryOnRevert) {
            // Retry mode tries the cheap path first. If the pool reverts, update the oracle and
            // try once more.
            _swapWithOracleRetry(
                pool, receiver, zeroForOne, amountSpecified, oracleArgs
            );
            return;
        }

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
        returns (
            address pool,
            bool zeroForOne,
            OracleUpdateMode oracleUpdateMode,
            bytes calldata oracleArgs
        )
    {
        uint256 oracleArgsLength = _validateDataLength(data);
        pool = address(bytes20(data[40:60]));
        zeroForOne = uint8(data[60]) > 0;

        oracleUpdateMode = OracleUpdateMode(uint8(data[61]));
        oracleArgs = data[data.length:data.length];
        // For modes 1 and 2, the payload is just ABI-encoded oracle args. The oracle target
        // comes from the immutable, so user-supplied swap bytes cannot redirect this call.
        if (oracleUpdateMode != OracleUpdateMode.Never) {
            oracleArgs = data[
                _BASE_DATA_LENGTH
                    + _ORACLE_ARGS_HEADER_LENGTH:
                    _BASE_DATA_LENGTH + _ORACLE_ARGS_HEADER_LENGTH
                        + oracleArgsLength
            ];
        }
    }

    function _validateDataLength(bytes calldata data)
        internal
        pure
        returns (uint256 oracleArgsLength)
    {
        if (data.length < _BASE_DATA_LENGTH) {
            revert MetricExecutor__InvalidDataLength();
        }

        uint8 oracleUpdateMode = uint8(data[61]);
        if (oracleUpdateMode > uint8(OracleUpdateMode.RetryOnRevert)) {
            revert MetricExecutor__InvalidOracleUpdateFlag();
        }

        if (oracleUpdateMode == uint8(OracleUpdateMode.Never)) {
            if (data.length != _BASE_DATA_LENGTH) {
                revert MetricExecutor__InvalidDataLength();
            }
            return 0;
        }

        uint256 minLength = _BASE_DATA_LENGTH + _ORACLE_ARGS_HEADER_LENGTH;
        if (data.length < minLength) {
            revert MetricExecutor__InvalidDataLength();
        }

        oracleArgsLength = uint32(bytes4(data[62:66]));
        if (data.length != minLength + oracleArgsLength) {
            revert MetricExecutor__InvalidDataLength();
        }
    }

    function _swapWithOracleRetry(
        address pool,
        address receiver,
        bool zeroForOne,
        int128 amountSpecified,
        bytes calldata oracleArgs
    ) internal {
        _setCurrentPool(pool);
        try IMetricPool(pool)
            .swap(
                receiver,
                zeroForOne,
                amountSpecified,
                zeroForOne ? 0 : type(uint128).max,
                ""
            ) {
            _setCurrentPool(address(0));
        } catch {
            // The first swap failed, so clear callback state before calling the oracle. Set it
            // again for the retry.
            _setCurrentPool(address(0));
            _updateOracle(oracleArgs);
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
    }

    function _updateOracle(bytes calldata oracleArgs) internal {
        (
            address feedCreator,
            uint256 deadline,
            uint256 newSlotValue,
            bytes memory signature
        ) = abi.decode(oracleArgs, (address, uint256, uint256, bytes));

        oracle.updateBySignature(feedCreator, deadline, newSlotValue, signature);
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
