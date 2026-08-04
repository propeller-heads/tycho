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

/// Chain-agnostic Ekubo V3 executor logic. Extensions deployed at the same
/// deterministic address on every chain (MEVCapture) are handled here.
/// Deployment-specific extensions (SignedExclusiveSwap, Ve33) live in the
/// per-chain executors, which override `_swapHop` — and `_hopEnd` when their
/// hops carry a self-describing tail — so each deployed executor only
/// contains code reachable on its chain.
abstract contract EkuboV3ExecutorBase is IExecutor, ICallback {
    error EkuboV3Executor__InvalidDataLength();
    error EkuboV3Executor__CoreOnly();
    error EkuboV3Executor__UnknownCallback();

    uint256 private constant _POOL_DATA_OFFSET = 56;
    uint256 internal constant _HOP_BYTE_LEN = 52;

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
        tokenIn = address(bytes20(data[0:20]));

        // Length-aware walk to find the last hop's tokenOut (the group
        // output). Here `data` is tokenIn(20) followed by the hops, so the
        // first hop starts at offset 20. Hops with a self-describing tail
        // advance past it via `_hopEnd`; fixed-size hops advance by 52.
        uint256 offset = 20;
        uint256 lastHopOffset = offset;
        while (offset < data.length) {
            if (offset + _HOP_BYTE_LEN > data.length) {
                revert EkuboV3Executor__InvalidDataLength();
            }
            lastHopOffset = offset;
            PoolConfig poolConfig =
                PoolConfig.wrap(bytes32(data[offset + 20:offset + 52]));
            offset = _hopEnd(data, offset + _HOP_BYTE_LEN, poolConfig);
        }
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

    /// @dev Executes one hop against Core and returns its balance update and
    /// the offset of the next hop. `offset` points just past the hop's fixed
    /// 52-byte header (tokenOut | poolConfig), which the loop has already
    /// parsed and bounds-checked. Implementations whose hops carry a
    /// self-describing tail (e.g. signed hops) read it from `swapData`
    /// starting at `offset`, must bounds-check it against `swapData.length`,
    /// and return the advanced offset; fixed-size hops return `offset`
    /// unchanged.
    function _swapHop(
        PoolKey memory poolKey,
        SwapParameters swapParameters,
        bytes calldata swapData,
        uint256 offset
    )
        internal
        virtual
        returns (PoolBalanceUpdate balanceUpdate, uint256 nextOffset)
    {
        swapData;
        if (poolKey.config.extension() == MEV_CAPTURE_ADDRESS) {
            (balanceUpdate,) = abi.decode(
                // slither-disable-next-line calls-loop
                CORE.forward(
                    MEV_CAPTURE_ADDRESS, abi.encode(poolKey, swapParameters)
                ),
                (PoolBalanceUpdate, PoolState)
            );
        } else {
            PoolState _stateAfter;
            // slither-disable-next-line calls-loop
            (balanceUpdate, _stateAfter) = CORE.swap(0, poolKey, swapParameters);
        }
        nextOffset = offset;
    }

    /// @dev Returns the offset just past a hop's tail. `offset` points just
    /// past the hop's fixed 52-byte header. Fixed-size hops return `offset`
    /// unchanged; implementations whose hops carry a self-describing tail
    /// must bounds-check it against `data.length`.
    function _hopEnd(bytes calldata, uint256 offset, PoolConfig)
        internal
        pure
        virtual
        returns (uint256)
    {
        return offset;
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

        // Length-aware walk over the hops. Each hop begins with a fixed
        // 52-byte header (tokenOut + poolConfig); `_swapHop` consumes any
        // chain-specific tail and returns the offset of the next hop.
        uint256 offset = _POOL_DATA_OFFSET;

        while (offset < swapData.length) {
            if (offset + _HOP_BYTE_LEN > swapData.length) {
                revert EkuboV3Executor__InvalidDataLength();
            }

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
            (balanceUpdate, offset) =
                _swapHop(pk, swapParameters, swapData, offset + _HOP_BYTE_LEN);

            nextTokenIn = nextTokenOut;
            nextAmountIn =
            -(isToken1 ? balanceUpdate.delta0() : balanceUpdate.delta1());
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
