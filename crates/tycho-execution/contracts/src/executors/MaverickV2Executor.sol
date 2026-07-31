// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {TransferManager} from "../TransferManager.sol";

error MaverickV2Executor__InvalidDataLength();

contract MaverickV2Executor is IExecutor {
    using SafeERC20 for IERC20;

    constructor() {}

    function fundsExpectedAddress(bytes calldata data)
        external
        pure
        returns (address receiver)
    {
        address target = address(bytes20(data[0:20]));
        return target;
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        address target;
        IERC20 tokenIn;

        (target, tokenIn) = _decodeData(data);

        IMaverickV2Pool pool = IMaverickV2Pool(target);

        bool isTokenAIn = pool.tokenA() == tokenIn;
        int32 tickLimit = isTokenAIn ? type(int32).max : type(int32).min;
        IMaverickV2Pool.SwapParams memory swapParams = IMaverickV2Pool.SwapParams({
            amount: amountIn,
            tokenAIn: isTokenAIn,
            exactOutput: false,
            tickLimit: tickLimit
        });

        // slither-disable-next-line unused-return
        pool.swap(receiver, swapParams, "");
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address target, IERC20 inToken)
    {
        if (data.length != 60) {
            revert MaverickV2Executor__InvalidDataLength();
        }
        target = address(bytes20(data[0:20]));
        inToken = IERC20(address(bytes20(data[20:40])));
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
        if (data.length != 60) {
            revert MaverickV2Executor__InvalidDataLength();
        }
        receiver = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
        transferType = TransferManager.TransferType.Transfer;
        outputToRouter = false;
    }
}

interface IMaverickV2Pool {
    struct SwapParams {
        uint256 amount;
        bool tokenAIn;
        bool exactOutput;
        int32 tickLimit;
    }

    function swap(
        address recipient,
        SwapParams memory params,
        bytes calldata data
    ) external returns (uint256 amountIn, uint256 amountOut);

    function tokenA() external view returns (IERC20);
    function tokenB() external view returns (IERC20);
}
