// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {
    IUniswapV3Pool
} from "@uniswap/v3-core/contracts/interfaces/IUniswapV3Pool.sol";
import {ICallback} from "@interfaces/ICallback.sol";
import {TransferManager} from "../TransferManager.sol";

error UniswapV3Executor__InvalidDataLength();

contract UniswapV3Executor is IExecutor, ICallback {
    using SafeERC20 for IERC20;

    uint160 private constant _MIN_SQRT_RATIO = 4295128739;
    uint160 private constant _MAX_SQRT_RATIO =
        1461446703485210103287273052203988822378723970342;

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
        address target;
        bool zeroForOne;
        (target, zeroForOne) = _decodeData(data);

        IUniswapV3Pool pool = IUniswapV3Pool(target);

        // slither-disable-next-line unused-return
        pool.swap(
            receiver,
            zeroForOne,
            // positive means exactIn
            int256(amountIn),
            zeroForOne ? _MIN_SQRT_RATIO + 1 : _MAX_SQRT_RATIO - 1,
            ""
        );
    }

    function handleCallback(
        bytes calldata /* msgData */
    )
        public
        pure
        returns (bytes memory)
    {
        // All transfers are done in the dispatcher - nothing to do here.
        return "";
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address target, bool zeroForOne)
    {
        if (data.length != 64) {
            revert UniswapV3Executor__InvalidDataLength();
        }
        target = address(bytes20(data[43:63]));
        zeroForOne = uint8(data[63]) > 0;
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
        if (data.length >= 40) {
            tokenIn = address(bytes20(data[0:20]));
            tokenOut = address(bytes20(data[20:40]));
        }
        return (
            TransferManager.TransferType.None,
            address(0),
            tokenIn,
            tokenOut,
            false
        );
    }

    function getCallbackTransferData(
        bytes calldata, /* data */
        address, /* tokenIn */
        address caller
    )
        external
        pure
        returns (TransferManager.TransferType transferType, address receiver)
    {
        transferType = TransferManager.TransferType.Transfer;
        receiver = caller;
    }
}
