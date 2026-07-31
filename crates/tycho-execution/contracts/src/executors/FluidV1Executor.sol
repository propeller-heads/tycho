// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

interface IFluidV1Dex {
    function swapInWithCallback(
        bool swap0to1_,
        uint256 amountIn_,
        uint256 amountOutMin_,
        address to_
    ) external payable returns (uint256 amountOut_);

    function swapIn(
        bool swap0to1_,
        uint256 amountIn_,
        uint256 amountOutMin_,
        address to_
    ) external payable returns (uint256 amountOut_);
}

error FluidV1Executor__ZeroLiquidityAddress();
error FluidV1Executor__InvalidDataLength();
error FluidV1Executor__InvalidCallback();

contract FluidV1Executor is IExecutor, ICallback {
    // keccak(FluidV1Executor#CURRENT_DEX)
    // stores current dex address
    bytes32 private constant _CURRENT_DEX_SLOT =
        0x823205ddf0d345ca541c0f695a3f87b5dce7be9df5ecffce73a87e1ad796ad20;
    // dexCallback(address,amount)
    bytes4 private constant _CALLBACK_SELECTOR = 0x9410ae88;

    address public immutable liquidity;

    constructor(address liquidity_) {
        if (liquidity_ == address(0)) {
            revert FluidV1Executor__ZeroLiquidityAddress();
        }
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
        IFluidV1Dex dex;
        bool zero2one;
        bool isNativeSell;

        (dex, zero2one, isNativeSell) = _decodeData(data);

        if (!isNativeSell) {
            _setCurrentDex(dex);
            // slither-disable-next-line unused-return
            dex.swapInWithCallback(zero2one, amountIn, 0, receiver);
        } else {
            // slither-disable-next-line arbitrary-send-eth,unused-return
            dex.swapIn{value: amountIn}(zero2one, amountIn, 0, receiver);
        }
    }

    // Stores dex address in transient storage
    function _setCurrentDex(IFluidV1Dex dex) internal {
        // slither-disable-next-line assembly
        assembly {
            tstore(_CURRENT_DEX_SLOT, dex)
        }
    }

    function _getCurrentDex() internal view returns (address dex) {
        // slither-disable-next-line assembly
        assembly {
            dex := tload(_CURRENT_DEX_SLOT)
        }
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (IFluidV1Dex dex, bool zero2one, bool isNativeSell)
    {
        // expected calldata layout
        // ---------------------
        // 0  | dex address
        // 20 | zero2one
        // 21 | tokenIn  (parsed in getTransferData)
        // 41 | tokenOut (parsed in getTransferData)
        // 61 | is_native
        // 62 | EOF
        if (data.length != 62) {
            revert FluidV1Executor__InvalidDataLength();
        }
        dex = IFluidV1Dex(address(bytes20(data[0:20])));
        zero2one = uint8(data[20]) > 0;
        isNativeSell = uint8(data[61]) > 0;
    }

    function handleCallback(bytes calldata data)
        public
        view
        returns (bytes memory result)
    {
        verifyCallback(data);
        result = "";
    }

    function verifyCallback(bytes calldata data) public view {
        address dex = _getCurrentDex();
        bytes4 selector = bytes4(data[:4]);
        if (msg.sender != dex || selector != _CALLBACK_SELECTOR) {
            revert FluidV1Executor__InvalidCallback();
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
        bool isNativeSell = uint8(data[61]) > 0;
        tokenOut = address(bytes20(data[41:61]));
        // ETH transfers are handled before the callback by calling dex.swapIn
        // instead of dex.swapInWithCallback.
        if (isNativeSell) {
            transferType = TransferManager.TransferType.TransferNativeInExecutor;
            tokenIn = ETH_ADDRESS;
        } else {
            transferType = TransferManager.TransferType.None;
            tokenIn = address(bytes20(data[21:41]));
        }
        return (transferType, address(0), tokenIn, tokenOut, false);
    }

    function getCallbackTransferData(
        bytes calldata, /* data */
        address, /* tokenIn */
        address /* caller */
    )
        external
        view
        returns (TransferManager.TransferType transferType, address receiver)
    {
        // This is only called for ERC20 swaps. Native sells use swapIn() (no
        // callback) rather than swapInWithCallback(), so this path is never
        // reached for native tokens.
        transferType = TransferManager.TransferType.Transfer;
        receiver = liquidity;
    }
}
