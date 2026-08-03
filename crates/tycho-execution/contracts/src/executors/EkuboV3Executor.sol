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
// PLACEHOLDER non-zero address — must NOT be address(0). Signed Ekubo V3
// (SignedExclusiveSwap, forward-only) pools set their pool config extension to
// this address; the executor detects a signed hop by comparing each hop's
// poolConfig.extension() against it and routes that hop through the signed path.
// TODO: replace with the deployed SignedExclusiveSwap extension address.
address constant SIGNED_EXCLUSIVE_SWAP_ADDRESS =
    0x5519eD5e5e5E5E5e5E5E5e5e5e5E5e5e5e5E5E5E;

contract EkuboV3Executor is IExecutor, ICallback {
    error EkuboV3Executor__InvalidDataLength();
    error EkuboV3Executor__CoreOnly();
    error EkuboV3Executor__UnknownCallback();

    uint256 private constant _POOL_DATA_OFFSET = 56;
    uint256 private constant _HOP_BYTE_LEN = 52;
    // A signed hop appends meta(32) | minBalanceUpdate(32) | sigLen(2) | sig.
    // These name the fixed-width parts of that tail; the signature length is
    // read from the 2-byte big-endian `sigLen` field.
    uint256 private constant _SIGNED_FIXED_TAIL_LEN = 64;
    uint256 private constant _SIG_LEN_BYTES = 2;

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

        // Length-aware walk to find the last hop's tokenOut (the group output).
        // Here `data` is tokenIn(20) followed by the hops, so the first hop
        // starts at offset 20. A signed hop carries a self-describing tail, so
        // its stride is 52 + 64 + 2 + sigLen; normal hops advance by 52.
        uint256 offset = 20;
        uint256 lastHopOffset = offset;
        while (offset < data.length) {
            if (offset + _HOP_BYTE_LEN > data.length) {
                revert EkuboV3Executor__InvalidDataLength();
            }
            lastHopOffset = offset;
            PoolConfig poolConfig =
                PoolConfig.wrap(bytes32(data[offset + 20:offset + 52]));
            if (poolConfig.extension() == SIGNED_EXCLUSIVE_SWAP_ADDRESS) {
                uint256 sigLenOffset =
                    offset + _HOP_BYTE_LEN + _SIGNED_FIXED_TAIL_LEN;
                if (sigLenOffset + _SIG_LEN_BYTES > data.length) {
                    revert EkuboV3Executor__InvalidDataLength();
                }
                uint256 sigLen = uint256(
                    uint16(bytes2(data[sigLenOffset:sigLenOffset + 2]))
                );
                offset += _HOP_BYTE_LEN + _SIGNED_FIXED_TAIL_LEN
                    + _SIG_LEN_BYTES + sigLen;
            } else {
                offset += _HOP_BYTE_LEN;
            }
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

        // Length-aware walk over the hops. Normal hops advance by a fixed 52
        // bytes (tokenOut + poolConfig); a signed hop carries a self-describing
        // tail, so its stride is read from the embedded `sigLen`. For all-normal
        // groups this is equivalent to the previous divide-by-52 iteration.
        uint256 offset = _POOL_DATA_OFFSET;

        while (offset < swapData.length) {
            // Each hop begins with tokenOut(20) | poolConfig(32) = 52 bytes.
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

            if (poolConfig.extension() == SIGNED_EXCLUSIVE_SWAP_ADDRESS) {
                // Signed hop tail: meta(32) | minBU(32) |
                // sigLen(2) | sig(sigLen)
                uint256 tailOff = offset + _HOP_BYTE_LEN;
                uint256 sigLenOff = tailOff + _SIGNED_FIXED_TAIL_LEN;
                if (sigLenOff + _SIG_LEN_BYTES > swapData.length) {
                    revert EkuboV3Executor__InvalidDataLength();
                }
                uint256 sigLen = uint256(
                    uint16(
                        bytes2(swapData[sigLenOff:sigLenOff + _SIG_LEN_BYTES])
                    )
                );
                uint256 sigEnd = sigLenOff + _SIG_LEN_BYTES + sigLen;
                if (sigEnd > swapData.length) {
                    revert EkuboV3Executor__InvalidDataLength();
                }

                // slither-disable-next-line calls-loop
                (balanceUpdate,) = abi.decode(
                    CORE.forward(
                        SIGNED_EXCLUSIVE_SWAP_ADDRESS,
                        abi.encode(
                            pk,
                            swapParameters,
                            // SignedSwapMeta (uint256)
                            uint256(bytes32(swapData[tailOff:tailOff + 32])),
                            // minBalanceUpdate
                            PoolBalanceUpdate.wrap(
                                bytes32(swapData[tailOff + 32:tailOff + 64])
                            ),
                            // signature
                            bytes(swapData[sigLenOff + _SIG_LEN_BYTES:sigEnd])
                        )
                    ),
                    (PoolBalanceUpdate, PoolState)
                );

                offset = sigEnd;
            } else if (poolConfig.extension() == MEV_CAPTURE_ADDRESS) {
                (balanceUpdate,) = abi.decode(
                    // slither-disable-next-line calls-loop
                    CORE.forward(
                        MEV_CAPTURE_ADDRESS, abi.encode(pk, swapParameters)
                    ),
                    (PoolBalanceUpdate, PoolState)
                );
                offset += _HOP_BYTE_LEN;
            } else {
                PoolState _stateAfter;
                // slither-disable-next-line calls-loop
                (balanceUpdate, _stateAfter) = CORE.swap(0, pk, swapParameters);
                offset += _HOP_BYTE_LEN;
            }

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
