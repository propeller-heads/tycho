// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

interface IFluidDexV2 {
    function startOperation(bytes calldata data)
        external
        returns (bytes memory result);

    function operate(
        uint256 dexType,
        uint256 implementationId,
        bytes memory data
    ) external returns (bytes memory returnData);

    function settle(
        address token,
        int256 supplyAmount,
        int256 borrowAmount,
        int256 storeAmount,
        address to,
        bool isCallback
    ) external payable;
}

interface IFluidDexV2Callbacks {
    function startOperationCallback(bytes calldata data)
        external
        returns (bytes memory);

    function dexCallback(address token, address to, uint256 amount) external;
}

/// @dev `fee` is the static LP fee in pips, or 0xFFFFFF for a dynamic-fee
/// pool. The same key addresses a D3 and a D4 pool; the dex type tells them
/// apart.
struct DexKey {
    address token0;
    address token1;
    uint24 fee;
    uint24 tickSpacing;
    address controller;
}

struct SwapInParams {
    DexKey dexKey;
    bool swap0To1;
    uint256 amountIn;
    uint256 amountOutMin;
    bytes controllerData;
}

interface IFluidDexV2SwapModule {
    function swapIn(SwapInParams calldata params)
        external
        returns (
            uint256 amountOut,
            uint256 protocolFeeAccrued,
            uint256 lpFeeAccrued
        );
}

error FluidV2Executor__ZeroAddress();
error FluidV2Executor__InvalidDataLength();
error FluidV2Executor__InvalidCallback();
error FluidV2Executor__UnknownCallback(bytes4 selector);
error FluidV2Executor__UnexpectedCallbackToken(address token);
error FluidV2Executor__UnexpectedCallbackRecipient(address to);

/// @title FluidV2Executor
/// @notice Swaps exact input on a Fluid DEX v2 D3 or D4 pool.
/// @dev The swap runs inside one dex operation: `startOperation` calls back
/// `startOperationCallback`, which swaps through `operate` and settles both
/// tokens. Each settle passes type(int128).max for supply and borrow, which
/// the dex resolves to the router's whole pending amount of that token, so
/// D3 and D4 settle the same way. The token in is pulled through
/// `dexCallback`, paid by the Dispatcher to the dex or to Fluid Liquidity
/// (the dex decides which at execution); native ETH goes in as msg.value.
contract FluidV2Executor is IExecutor, ICallback {
    uint256 private constant _SWAP_MODULE_ID = 1;
    int256 private constant _ENTIRE_PENDING_AMOUNT = type(int128).max;

    IFluidDexV2 public immutable dexV2;
    address public immutable liquidity;

    constructor(address dexV2_, address liquidity_) {
        if (dexV2_ == address(0) || liquidity_ == address(0)) {
            revert FluidV2Executor__ZeroAddress();
        }
        dexV2 = IFluidDexV2(dexV2_);
        liquidity = liquidity_;
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
            uint8 dexType,
            DexKey memory dexKey,
            bool swap0To1,
            bytes calldata controllerData
        ) = _decodeData(data);

        // slither-disable-next-line unused-return
        dexV2.startOperation(
            abi.encode(
                dexType, dexKey, swap0To1, amountIn, receiver, controllerData
            )
        );
    }

    function handleCallback(bytes calldata data)
        public
        returns (bytes memory result)
    {
        verifyCallback(data);

        if (
            bytes4(data[:4])
                == IFluidDexV2Callbacks.startOperationCallback.selector
        ) {
            // The router's fallback returns these bytes raw, and the dex
            // decodes them as the callback's `bytes` return value.
            return abi.encode(_swapAndSettle(abi.decode(data[4:], (bytes))));
        }
        // dexCallback: the Dispatcher already paid the token in.
        return "";
    }

    function verifyCallback(bytes calldata data) public view {
        bytes4 selector = bytes4(data[:4]);
        if (
            msg.sender != address(dexV2)
                || (selector
                        != IFluidDexV2Callbacks.startOperationCallback.selector
                    && selector != IFluidDexV2Callbacks.dexCallback.selector)
        ) {
            revert FluidV2Executor__InvalidCallback();
        }
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
        (, DexKey memory dexKey, bool swap0To1,) = _decodeData(data);
        (tokenIn, tokenOut) = swap0To1
            ? (dexKey.token0, dexKey.token1)
            : (dexKey.token1, dexKey.token0);

        // Fluid's native token sentinel is ETH_ADDRESS, so native legs need no
        // mapping. Native ETH goes in as msg.value of the token-in settle.
        transferType = tokenIn == ETH_ADDRESS
            ? TransferManager.TransferType.TransferNativeInExecutor
            : TransferManager.TransferType.None;
        return (transferType, address(0), tokenIn, tokenOut, false);
    }

    function getCallbackTransferData(
        bytes calldata data,
        address tokenIn,
        address caller
    )
        external
        view
        returns (TransferManager.TransferType transferType, address receiver)
    {
        if (caller != address(dexV2)) {
            revert FluidV2Executor__InvalidCallback();
        }

        bytes4 selector = bytes4(data[:4]);
        if (selector == IFluidDexV2Callbacks.startOperationCallback.selector) {
            return (TransferManager.TransferType.None, address(0));
        }
        if (selector != IFluidDexV2Callbacks.dexCallback.selector) {
            revert FluidV2Executor__UnknownCallback(selector);
        }

        (address token, address to,) =
            abi.decode(data[4:], (address, address, uint256));
        if (token != tokenIn) {
            revert FluidV2Executor__UnexpectedCallbackToken(token);
        }
        // The dex serves a settle from its own balance or from Liquidity.
        if (to != address(dexV2) && to != liquidity) {
            revert FluidV2Executor__UnexpectedCallbackRecipient(to);
        }
        return (TransferManager.TransferType.Transfer, to);
    }

    function _swapAndSettle(bytes memory callbackData)
        internal
        returns (bytes memory swapResult)
    {
        (
            uint8 dexType,
            DexKey memory dexKey,
            bool swap0To1,
            uint256 amountIn,
            address receiver,
            bytes memory controllerData
        ) = abi.decode(
            callbackData, (uint8, DexKey, bool, uint256, address, bytes)
        );

        // amountOutMin is 0: the router enforces the route's minAmountOut.
        swapResult = dexV2.operate(
            dexType,
            _SWAP_MODULE_ID,
            abi.encodeCall(
                IFluidDexV2SwapModule.swapIn,
                (SwapInParams({
                        dexKey: dexKey,
                        swap0To1: swap0To1,
                        amountIn: amountIn,
                        amountOutMin: 0,
                        controllerData: controllerData
                    }))
            )
        );

        (address tokenIn, address tokenOut) = swap0To1
            ? (dexKey.token0, dexKey.token1)
            : (dexKey.token1, dexKey.token0);

        if (tokenIn == ETH_ADDRESS) {
            // slither-disable-next-line arbitrary-send-eth
            dexV2.settle{value: amountIn}(
                tokenIn,
                _ENTIRE_PENDING_AMOUNT,
                _ENTIRE_PENDING_AMOUNT,
                0,
                address(0),
                true
            );
        } else {
            dexV2.settle(
                tokenIn,
                _ENTIRE_PENDING_AMOUNT,
                _ENTIRE_PENDING_AMOUNT,
                0,
                address(0),
                true
            );
        }
        dexV2.settle(
            tokenOut,
            _ENTIRE_PENDING_AMOUNT,
            _ENTIRE_PENDING_AMOUNT,
            0,
            receiver,
            false
        );
    }

    /// @dev Swap data: [dexType: 1][token0: 20][token1: 20][fee: 3]
    /// [tickSpacing: 3][controller: 20][swap0To1: 1][controllerData: rest]
    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            uint8 dexType,
            DexKey memory dexKey,
            bool swap0To1,
            bytes calldata controllerData
        )
    {
        if (data.length < 68) {
            revert FluidV2Executor__InvalidDataLength();
        }

        dexType = uint8(data[0]);
        dexKey.token0 = address(bytes20(data[1:21]));
        dexKey.token1 = address(bytes20(data[21:41]));
        dexKey.fee = uint24(bytes3(data[41:44]));
        dexKey.tickSpacing = uint24(bytes3(data[44:47]));
        dexKey.controller = address(bytes20(data[47:67]));
        swap0To1 = uint8(data[67]) > 0;
        controllerData = data[68:];
    }
}
