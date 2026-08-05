// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {TransferManager} from "../TransferManager.sol";

interface IFluidDexV2 {
    function startOperation(bytes calldata data)
        external
        payable
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

struct DexKey {
    address token0;
    address token1;
    uint24 fee; // The fee here tells the fee if its a static fee pool or acts as a dynamic fee flag, i.e, type(uint24).max or 0xFFFFFF for dynamic fee pools.
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

error FluidV2Executor__InvalidDataLength();
error FluidV2Executor__InvalidCallback();
error FluidV2Executor__UnknownCallback(bytes4 selector);
error FluidV2Executor__UnknownDexType(uint8 dexType);
error FluidV2Executor__ZeroDexV2Address();

contract FluidV2Executor is IExecutor, ICallback {
    uint256 private constant _DEX_V2_SWAP_MODULE_ID = 1;

    bytes4 private constant _START_OPERATION_CALLBACK_SELECTOR =
        bytes4(keccak256("startOperationCallback(bytes)"));
    bytes4 private constant _DEX_CALLBACK_SELECTOR =
        bytes4(keccak256("dexCallback(address,address,uint256)"));
    bytes4 private constant _SWAP_IN_SELECTOR = bytes4(
        keccak256(
            "swapIn(((address,address,uint24,uint24,address),bool,uint256,uint256,bytes))"
        )
    );

    uint8 private constant _D3_DEX_TYPE = 3;
    uint8 private constant _D4_DEX_TYPE = 4;

    address private constant _FLUID_NATIVE_TOKEN =
        0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE;

    IFluidDexV2 public immutable dexV2;

    constructor(address dexV2_) {
        if (dexV2_ == address(0)) {
            revert FluidV2Executor__ZeroDexV2Address();
        }
        dexV2 = IFluidDexV2(dexV2_);
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
        external
        returns (bytes memory result)
    {
        verifyCallback(data);

        bytes4 selector = bytes4(data[:4]);

        if (selector == _START_OPERATION_CALLBACK_SELECTOR) {
            return abi.encode(
                _handleStartOperationCallback(abi.decode(data[4:], (bytes)))
            );
        }

        if (selector == _DEX_CALLBACK_SELECTOR) {
            (address token, address to, uint256 amount) =
                abi.decode(data[4:], (address, address, uint256));
            dexCallback(token, to, amount);
            return abi.encode("");
        }

        revert FluidV2Executor__UnknownCallback(selector);
    }

    function verifyCallback(bytes calldata data) public view {
        bytes4 selector = bytes4(data[:4]);

        if (
            msg.sender != address(dexV2)
                || (selector != _START_OPERATION_CALLBACK_SELECTOR
                    && selector != _DEX_CALLBACK_SELECTOR)
        ) {
            revert FluidV2Executor__InvalidCallback();
        }
    }

    function getTransferData(bytes calldata data)
        external
        payable
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        (, DexKey memory dexKey, bool swap0To1,) = _decodeData(data);

        tokenIn = _toTychoToken(swap0To1 ? dexKey.token0 : dexKey.token1);
        tokenOut = _toTychoToken(swap0To1 ? dexKey.token1 : dexKey.token0);

        return (
            TransferManager.TransferType.None,
            address(0),
            tokenIn,
            tokenOut,
            false
        );
    }

    function getCallbackTransferData(bytes calldata data, address tokenIn)
        external
        payable
        returns (TransferManager.TransferType transferType, address receiver)
    {
        bytes4 selector = bytes4(data[:4]);

        if (selector == _START_OPERATION_CALLBACK_SELECTOR) {
            return (TransferManager.TransferType.None, address(0));
        }

        if (selector != _DEX_CALLBACK_SELECTOR) {
            revert FluidV2Executor__UnknownCallback(selector);
        }

        address fluidToken;
        (fluidToken, receiver,) =
            abi.decode(data[4:], (address, address, uint256));

        if (_toTychoToken(fluidToken) != tokenIn) {
            revert FluidV2Executor__InvalidCallback();
        }

        if (tokenIn == address(0)) {
            return
                (
                    TransferManager.TransferType.TransferNativeInExecutor,
                    receiver
                );
        }

        return (TransferManager.TransferType.Transfer, receiver);
    }

    function dexCallback(
        address, /* token */
        address, /* to */
        uint256 /* amount */
    )
        public
        pure {}

    function _handleStartOperationCallback(bytes memory callbackData)
        internal
        returns (bytes memory)
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

        SwapInParams memory params = SwapInParams({
            dexKey: dexKey,
            swap0To1: swap0To1,
            amountIn: amountIn,
            amountOutMin: 0,
            controllerData: controllerData
        });

        bytes memory swapResult = dexV2.operate(
            dexType,
            _DEX_V2_SWAP_MODULE_ID,
            abi.encodeWithSelector(_SWAP_IN_SELECTOR, params)
        );

        (uint256 amountOut, uint256 protocolFee, uint256 lpFee) =
            abi.decode(swapResult, (uint256, uint256, uint256));

        _settleSwap(
            dexType,
            dexKey,
            swap0To1,
            amountIn,
            amountOut,
            protocolFee,
            lpFee,
            receiver
        );

        return swapResult;
    }

    function _settleSwap(
        uint8 dexType,
        DexKey memory dexKey,
        bool swap0To1,
        uint256 amountIn,
        uint256 amountOut,
        uint256 protocolFee,
        uint256 lpFee,
        address receiver
    ) internal {
        if (dexType == _D3_DEX_TYPE) {
            _settleD3(dexKey, swap0To1, amountIn, amountOut, receiver);
            return;
        }

        if (dexType == _D4_DEX_TYPE) {
            _settleD4(
                dexKey,
                swap0To1,
                amountIn,
                amountOut,
                protocolFee,
                lpFee,
                receiver
            );
            return;
        }

        revert FluidV2Executor__UnknownDexType(dexType);
    }

    function _settleD3(
        DexKey memory dexKey,
        bool swap0To1,
        uint256 amountIn,
        uint256 amountOut,
        address receiver
    ) internal {
        address tokenIn = swap0To1 ? dexKey.token0 : dexKey.token1;
        address tokenOut = swap0To1 ? dexKey.token1 : dexKey.token0;

        if (tokenIn == _FLUID_NATIVE_TOKEN) {
            dexV2.settle{value: amountIn}(
                tokenIn, int256(amountIn), 0, 0, receiver, true
            );
        } else {
            dexV2.settle(tokenIn, int256(amountIn), 0, 0, receiver, true);
        }

        dexV2.settle(tokenOut, -int256(amountOut), 0, 0, receiver, true);
    }

    function _settleD4(
        DexKey memory dexKey,
        bool swap0To1,
        uint256 amountIn,
        uint256 amountOut,
        uint256 protocolFee,
        uint256 lpFee,
        address receiver
    ) internal {
        uint256 totalFee = protocolFee + lpFee;
        address tokenIn = swap0To1 ? dexKey.token0 : dexKey.token1;
        address tokenOut = swap0To1 ? dexKey.token1 : dexKey.token0;

        if (tokenIn == _FLUID_NATIVE_TOKEN) {
            dexV2.settle{value: amountIn}(
                tokenIn, 0, -int256(amountIn), 0, receiver, true
            );
        } else {
            dexV2.settle(tokenIn, 0, -int256(amountIn), 0, receiver, true);
        }

        dexV2.settle(
            tokenOut,
            int256(totalFee),
            int256(amountOut + totalFee),
            0,
            receiver,
            true
        );
    }

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

    function _toTychoToken(address fluidToken)
        internal
        pure
        returns (address token)
    {
        return fluidToken == _FLUID_NATIVE_TOKEN ? address(0) : fluidToken;
    }
}
