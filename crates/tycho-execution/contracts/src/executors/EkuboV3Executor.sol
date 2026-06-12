// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {ICore} from "@ekubo-v3/interfaces/ICore.sol";
import {
    IFlashAccountant,
    ILocker
} from "@ekubo-v3/interfaces/IFlashAccountant.sol";
import {CoreLib} from "@ekubo-v3/libraries/CoreLib.sol";
import {FlashAccountantLib} from "@ekubo-v3/libraries/FlashAccountantLib.sol";
import {SafeTransferLib} from "@solady/utils/SafeTransferLib.sol";
import {LibBytes} from "@solady/utils/LibBytes.sol";
import {LibCall} from "@solady/utils/LibCall.sol";
import {SafeCastLib} from "@solady/utils/SafeCastLib.sol";
import {
    SqrtRatio,
    MIN_SQRT_RATIO,
    MAX_SQRT_RATIO
} from "@ekubo-v3/types/sqrtRatio.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";
import {PoolKey} from "@ekubo-v3/types/poolKey.sol";
import {PoolConfig} from "@ekubo-v3/types/poolConfig.sol";
import {NATIVE_TOKEN_ADDRESS} from "@ekubo-v3/math/constants.sol";
import {PoolBalanceUpdate} from "@ekubo-v3/types/poolBalanceUpdate.sol";
import {PoolState} from "@ekubo-v3/types/poolState.sol";
import {
    createSwapParameters,
    SwapParameters
} from "@ekubo-v3/types/swapParameters.sol";

using CoreLib for ICore;
using FlashAccountantLib for ICore;

address payable constant CORE_ADDRESS =
    payable(0x00000000000014aA86C5d3c41765bb24e11bd701);
ICore constant CORE = ICore(CORE_ADDRESS);
address constant MEV_CAPTURE_ADDRESS =
    0x5555fF9Ff2757500BF4EE020DcfD0210CFfa41Be;

contract EkuboV3Executor is IExecutor, ICallback {
    error EkuboV3Executor__InvalidDataLength();
    error EkuboV3Executor__CoreOnly();
    error EkuboV3Executor__UnknownCallback();

    uint256 private constant _POOL_DATA_OFFSET = 56;
    uint256 private constant _HOP_BYTE_LEN = 52;

    uint256 private constant _SKIP_AHEAD = 0;

    using SafeERC20 for IERC20;

    modifier coreOnly() {
        if (msg.sender != CORE_ADDRESS) revert EkuboV3Executor__CoreOnly();
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
        // Ekubo uses flash accounting: no pre-swap transfer needed.
        // Tokens are paid during the callback in the Dispatcher
        return (
            TransferManager.TransferType.None,
            address(0),
            tokenIn,
            tokenOut,
            false
        );
    }

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        // Callback-based protocol: funds stay in the router between swaps.
        return msg.sender;
    }

    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        if (data.length < 72) revert EkuboV3Executor__InvalidDataLength();

        address tokenIn = address(bytes20(data[0:20]));
        // Swap data uses ETH_ADDRESS for native ETH; translate to
        // address(0) for Ekubo V3 protocol interaction.
        if (tokenIn == ETH_ADDRESS) tokenIn = address(0);
        // startPayments needs to be called in CORE before we transfer the token IN (which happens during callback)
        // slither-disable-next-line unused-return
        LibCall.callContract(
            CORE_ADDRESS,
            abi.encodeWithSelector(
                IFlashAccountant.startPayments.selector, tokenIn
            )
        );

        // amountIn must be at most type(int128).max
        // slither-disable-next-line unused-return
        LibCall.callContract(
            CORE_ADDRESS,
            abi.encodePacked(
                IFlashAccountant.lock.selector,
                bytes16(uint128(SafeCastLib.toInt128(amountIn))),
                bytes20(receiver),
                data
            )
        );
    }

    function handleCallback(bytes calldata raw) public returns (bytes memory) {
        verifyCallback(raw);

        // Without selector and locker id
        _locked(raw[36:]);
        return "";
    }

    function verifyCallback(bytes calldata raw) public view coreOnly {
        bytes4 selector = bytes4(raw[:4]);
        if (selector != ILocker.locked_6416899205.selector) {
            revert EkuboV3Executor__UnknownCallback();
        }
    }

    function getCallbackTransferData(
        bytes calldata, /* data */
        address tokenIn,
        address /* caller */
    )
        external
        view
        returns (TransferManager.TransferType transferType, address receiver)
    {
        receiver = CORE_ADDRESS;

        if (tokenIn == ETH_ADDRESS) {
            // Native ETH: Dispatcher updates delta accounting; actual transfer
            // happens inside _pay() via safeTransferETH.
            transferType = TransferManager.TransferType.TransferNativeInExecutor;
        } else {
            transferType = TransferManager.TransferType.Transfer;
        }
    }

    function _locked(bytes calldata swapData) private {
        uint128 amountIn = uint128(bytes16(swapData[0:16]));
        int128 nextAmountIn = int128(amountIn);
        address receiver = address(bytes20(swapData[16:36]));
        address tokenIn = address(bytes20(swapData[36:56]));
        // Swap data uses ETH_ADDRESS for native ETH; translate to
        // address(0) for Ekubo V3 protocol interaction.
        if (tokenIn == ETH_ADDRESS) tokenIn = address(0);
        address nextTokenOut = address(0);

        address nextTokenIn = tokenIn;

        uint256 hopsLength =
            (swapData.length - _POOL_DATA_OFFSET) / _HOP_BYTE_LEN;

        uint256 offset = _POOL_DATA_OFFSET;

        for (uint256 i = 0; i < hopsLength; i++) {
            nextTokenOut =
                address(bytes20(LibBytes.loadCalldata(swapData, offset)));
            if (nextTokenOut == ETH_ADDRESS) nextTokenOut = address(0);
            PoolConfig poolConfig =
                PoolConfig.wrap(LibBytes.loadCalldata(swapData, offset + 20));

            (
                address token0,
                address token1,
                bool isToken1,
                SqrtRatio sqrtRatioLimit
            ) = nextTokenIn > nextTokenOut
                ? (nextTokenOut, nextTokenIn, true, MAX_SQRT_RATIO)
                : (nextTokenIn, nextTokenOut, false, MIN_SQRT_RATIO);

            PoolKey memory pk =
                PoolKey({token0: token0, token1: token1, config: poolConfig});

            SwapParameters swapParameters = createSwapParameters({
                _sqrtRatioLimit: sqrtRatioLimit,
                _amount: nextAmountIn,
                _isToken1: isToken1,
                _skipAhead: _SKIP_AHEAD
            });

            PoolBalanceUpdate balanceUpdate;

            if (poolConfig.extension() == MEV_CAPTURE_ADDRESS) {
                (balanceUpdate,) = abi.decode(
                    // slither-disable-next-line calls-loop
                    CORE.forward(
                        MEV_CAPTURE_ADDRESS, abi.encode(pk, swapParameters)
                    ),
                    (PoolBalanceUpdate, PoolState)
                );
            } else {
                PoolState _stateAfter;
                // slither-disable-next-line calls-loop
                (balanceUpdate, _stateAfter) = CORE.swap(0, pk, swapParameters);
            }

            nextTokenIn = nextTokenOut;
            nextAmountIn =
            -(isToken1 ? balanceUpdate.delta0() : balanceUpdate.delta1());

            offset += _HOP_BYTE_LEN;
        }

        _pay(tokenIn, amountIn);
        CORE.withdraw(nextTokenIn, receiver, uint128(nextAmountIn));
    }

    function _pay(address token, uint128 amount) private {
        if (token == NATIVE_TOKEN_ADDRESS) {
            SafeTransferLib.safeTransferETH(CORE_ADDRESS, amount);
            return;
        }
        bytes memory _result = LibCall.callContract(
            CORE_ADDRESS,
            abi.encodeWithSelector(
                IFlashAccountant.completePayments.selector, token
            )
        );
    }
}
