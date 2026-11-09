// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {TransientSlot} from "@openzeppelin/contracts/utils/TransientSlot.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {TransferManager} from "../TransferManager.sol";

// MetricOmmPool swap interface; see
// https://docs.metric.xyz/RSm94m71kqtGICv4iKRj
// -> Developers -> Smart Contracts Reference -> Swapping directly via pool.
// The returned deltas are intentionally ignored: the Dispatcher verifies output
// via balance-diff.
interface IMetricPool {
    function swap(
        address recipient,
        bool zeroForOne,
        int128 amountSpecified,
        uint128 priceLimitX64,
        bytes calldata callbackData,
        bytes calldata extensionData
    ) external returns (int128 amount0Delta, int128 amount1Delta);
}

error MetricExecutor__InvalidDataLength();
error MetricExecutor__AmountInTooLarge();
error MetricExecutor__InvalidCallback();

contract MetricExecutor is IExecutor, ICallback {
    // One directive per slot type: Slither 0.11.6 can fail to analyse a
    // contract that uses `using TransientSlot for *`.
    using TransientSlot for TransientSlot.AddressSlot;
    uint256 private constant _DATA_LENGTH = 61;
    uint256 private constant _INT128_MAX = uint256(uint128(type(int128).max));

    // metricOmmSwapCallback(int256,int256,bytes) — the only callback the pool emits.
    bytes4 private constant _METRIC_CALLBACK_SELECTOR = 0xc3251075;

    TransientSlot.AddressSlot private constant _CURRENT_METRIC_POOL_SLOT =
        TransientSlot.AddressSlot
            .wrap(keccak256("MetricExecutor#CURRENT_METRIC_POOL"));

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return msg.sender;
    }

    // slither-disable-next-line locked-ether The router delegatecalls swap, so any ETH it receives is held by the router.
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address pool, bool zeroForOne) = _decodeSwapData(data);

        if (amountIn > _INT128_MAX) {
            revert MetricExecutor__AmountInTooLarge();
        }

        uint128 amountIn128 = uint128(amountIn);
        int128 amountSpecified = int128(amountIn128);

        _setCurrentPool(pool);
        // slither-disable-next-line unused-return The Dispatcher derives the output from balance diffs, not from return values.
        IMetricPool(pool)
            .swap(
                receiver,
                zeroForOne,
                amountSpecified,
                zeroForOne ? 0 : type(uint128).max,
                "",
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
        if (selector != _METRIC_CALLBACK_SELECTOR) {
            revert MetricExecutor__InvalidCallback();
        }
    }

    function _setCurrentPool(address pool) internal {
        _CURRENT_METRIC_POOL_SLOT.tstore(pool);
    }

    function _getCurrentPool() internal view returns (address pool) {
        pool = _CURRENT_METRIC_POOL_SLOT.tload();
    }
}
