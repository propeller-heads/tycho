// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {ICore} from "@ekubo/interfaces/ICore.sol";
import {ILocker, IPayer} from "@ekubo/interfaces/IFlashAccountant.sol";
import {NATIVE_TOKEN_ADDRESS} from "@ekubo/math/constants.sol";
import {SafeTransferLib} from "@solady/utils/SafeTransferLib.sol";
import {LibBytes} from "@solady/utils/LibBytes.sol";
import {Config, PoolKey} from "@ekubo/types/poolKey.sol";
import {
    MAX_SQRT_RATIO,
    MIN_SQRT_RATIO,
    SqrtRatio
} from "@ekubo/types/sqrtRatio.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";

contract EkuboExecutor is IExecutor, ILocker, IPayer, ICallback {
    error EkuboExecutor__AddressZero();
    error EkuboExecutor__InvalidDataLength();
    error EkuboExecutor__CoreOnly();
    error EkuboExecutor__UnknownCallback();

    ICore private immutable _core;
    address private immutable _mevResist;

    uint256 private constant _POOL_DATA_OFFSET = 56;
    uint256 private constant _HOP_BYTE_LEN = 52;

    bytes4 private constant _LOCKED_SELECTOR = 0xb45a3c0e; // locked(uint256)
    bytes4 private constant _PAY_CALLBACK_SELECTOR = 0x599d0714; // payCallback(uint256,address)

    uint256 private constant _SKIP_AHEAD = 0;

    using SafeERC20 for IERC20;

    constructor(address core, address mevResist) {
        _core = ICore(core);

        if (mevResist == address(0)) {
            revert EkuboExecutor__AddressZero();
        }
        _mevResist = mevResist;
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

    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        if (data.length < 72) {
            revert EkuboExecutor__InvalidDataLength();
        }

        // amountIn must be at most type(int128).MAX
        _lock(
            abi.encodePacked(
                bytes16(uint128(amountIn)), bytes20(receiver), data
            )
        );
    }

    function handleCallback(bytes calldata raw)
        external
        returns (bytes memory)
    {
        verifyCallback(raw);

        bytes4 selector = bytes4(raw[:4]);

        if (selector == _LOCKED_SELECTOR) {
            _locked(raw[36:]);
        } else if (selector == _PAY_CALLBACK_SELECTOR) {
            // The paying is done in the Dispatcher using getCallbackTransferData
        } else {
            revert EkuboExecutor__UnknownCallback();
        }

        return "";
    }

    function verifyCallback(bytes calldata) public view coreOnly {}

    function locked(uint256) external coreOnly {
        _locked(msg.data[36:]);
    }

    function payCallback(
        uint256,
        address /*token*/
    )
        external
        coreOnly
    {
        // The paying is done in the Dispatcher using getCallbackTransferData. Nothing to do here
    }

    function _lock(bytes memory data) internal {
        // Prepend selector of lock() to calldata
        // We must use assembly here since the Ekubo Core's lock method expects the raw
        // bytes directly and not ABI-encoded bytes
        bytes memory callData = abi.encodePacked(bytes4(0xf83d08ba), data);

        // slither-disable-next-line low-level-calls
        (bool success, bytes memory result) = address(_core).call(callData);

        if (!success) {
            // slither-disable-next-line assembly
            assembly ("memory-safe") {
                revert(add(result, 32), mload(result))
            }
        }
    }

    function _locked(bytes calldata swapData) internal {
        int128 nextAmountIn = int128(uint128(bytes16(swapData[0:16])));
        uint128 tokenInDebtAmount = uint128(nextAmountIn);
        address receiver = address(bytes20(swapData[16:36]));
        address tokenIn = address(bytes20(swapData[36:56]));
        // Swap data uses ETH_ADDRESS for native ETH; translate to
        // address(0) for Ekubo protocol interaction (pool keys, pay,
        // withdraw).
        if (tokenIn == ETH_ADDRESS) tokenIn = address(0);

        address nextTokenIn = tokenIn;
        address nextTokenOut = address(0);

        uint256 hopsLength =
            (swapData.length - _POOL_DATA_OFFSET) / _HOP_BYTE_LEN;

        uint256 offset = _POOL_DATA_OFFSET;

        for (uint256 i = 0; i < hopsLength; i++) {
            nextTokenOut =
                address(bytes20(LibBytes.loadCalldata(swapData, offset)));
            if (nextTokenOut == ETH_ADDRESS) nextTokenOut = address(0);
            Config poolConfig =
                Config.wrap(LibBytes.loadCalldata(swapData, offset + 20));

            (
                address token0,
                address token1,
                bool isToken1,
                SqrtRatio sqrtRatioLimit
            ) = nextTokenIn > nextTokenOut
                ? (nextTokenOut, nextTokenIn, true, MAX_SQRT_RATIO)
                : (nextTokenIn, nextTokenOut, false, MIN_SQRT_RATIO);

            PoolKey memory pk = PoolKey(token0, token1, poolConfig);

            int128 delta0;
            int128 delta1;

            if (poolConfig.extension() == _mevResist) {
                (delta0, delta1) = abi.decode(
                    _forward(
                        _mevResist,
                        abi.encode(
                            pk,
                            nextAmountIn,
                            isToken1,
                            sqrtRatioLimit,
                            _SKIP_AHEAD
                        )
                    ),
                    (int128, int128)
                );
            } else {
                // slither-disable-next-line calls-loop
                (delta0, delta1) = _core.swap_611415377(
                    pk, nextAmountIn, isToken1, sqrtRatioLimit, _SKIP_AHEAD
                );
            }

            nextTokenIn = nextTokenOut;
            nextAmountIn = -(isToken1 ? delta0 : delta1);

            offset += _HOP_BYTE_LEN;
        }

        _pay(tokenIn, tokenInDebtAmount);
        _core.withdraw(nextTokenIn, receiver, uint128(nextAmountIn));
    }

    function _forward(address to, bytes memory data)
        internal
        returns (bytes memory result)
    {
        // Prepend forward(address) selector to the data
        // We must use assembly here since the Ekubo Core's lock method expects the raw
        // bytes directly and not ABI-encoded bytes
        bytes memory callData = abi.encodePacked(
            bytes4(0x101e8952), bytes32(uint256(uint160(to))), data
        );

        // slither-disable-next-line low-level-calls,calls-loop
        (bool success, bytes memory returnData) = address(_core).call(callData);

        // Assembly is necessary to be able to revert with arbitrary bytes memory
        if (!success) {
            // slither-disable-next-line assembly
            assembly ("memory-safe") {
                revert(add(returnData, 32), mload(returnData))
            }
        }

        return returnData;
    }

    function _pay(address token, uint128 amount) internal {
        if (token == NATIVE_TOKEN_ADDRESS) {
            SafeTransferLib.safeTransferETH(address(_core), amount);
        } else {
            bytes memory callData = abi.encodePacked(
                bytes4(0x0c11dedd), // pay(address) selector
                bytes32(uint256(uint160(token))),
                bytes16(amount)
            );

            // slither-disable-next-line low-level-calls
            (bool success, bytes memory result) = address(_core).call(callData);

            if (!success) {
                // slither-disable-next-line assembly
                assembly ("memory-safe") {
                    revert(add(result, 32), mload(result))
                }
            }
        }
    }

    // To receive withdrawals from Core
    receive() external payable {}

    modifier coreOnly() {
        if (msg.sender != address(_core)) revert EkuboExecutor__CoreOnly();
        _;
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
        uint256 hopsLength =
            (data.length - _POOL_DATA_OFFSET + 36) / _HOP_BYTE_LEN;
        uint256 lastHopOffset = 20 + (hopsLength - 1) * _HOP_BYTE_LEN;
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[lastHopOffset:lastHopOffset + 20]));
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
        address tokenIn,
        address /* caller */
    )
        external
        view
        returns (TransferManager.TransferType transferType, address receiver)
    {
        bytes4 selector = bytes4(data[:4]);
        if (selector == _PAY_CALLBACK_SELECTOR) {
            transferType = TransferManager.TransferType.Transfer;
            receiver = address(_core);
        } else {
            // _LOCKED_SELECTOR
            if (tokenIn == ETH_ADDRESS) {
                // ETH transfers are handled in the Executor, so we need to set the
                // transferType to TransferNativeInExecutor to update delta accounting.
                transferType =
                TransferManager.TransferType.TransferNativeInExecutor;
            } else {
                // Locked callback: no transfer needed for ERC20 tokens. This is
                // done in the Pay callback.
                transferType = TransferManager.TransferType.None;
            }
        }
    }
}
