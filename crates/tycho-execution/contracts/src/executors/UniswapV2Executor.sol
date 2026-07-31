// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {
    IUniswapV2Pair
} from "@uniswap-v2/contracts/interfaces/IUniswapV2Pair.sol";
import {TransferManager} from "../TransferManager.sol";

error UniswapV2Executor__InvalidDataLength();
error UniswapV2Executor__InvalidFee();

contract UniswapV2Executor is IExecutor {
    using SafeERC20 for IERC20;

    uint256 public immutable feeBps;

    constructor(uint256 feeBps_) {
        if (feeBps_ > 30) {
            revert UniswapV2Executor__InvalidFee();
        }
        feeBps = feeBps_;
    }

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
        address tokenIn;
        address tokenOut;

        (target, tokenIn, tokenOut) = _decodeData(data);

        bool zeroForOne = (tokenIn < tokenOut);

        IUniswapV2Pair pool = IUniswapV2Pair(target);

        _swap(pool, amountIn, zeroForOne, receiver);
    }

    function _swap(
        IUniswapV2Pair pool,
        uint256 amountIn,
        bool zeroForOne,
        address receiver
    ) internal {
        // slither-disable-next-line unused-return
        (uint112 reserve0, uint112 reserve1,) = pool.getReserves();
        uint112 reserveIn = zeroForOne ? reserve0 : reserve1;
        uint112 reserveOut = zeroForOne ? reserve1 : reserve0;

        uint256 calculatedAmount =
            _getAmountOut(amountIn, reserveIn, reserveOut);

        if (zeroForOne) {
            pool.swap(0, calculatedAmount, receiver, "");
        } else {
            pool.swap(calculatedAmount, 0, receiver, "");
        }
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address target, address tokenIn, address tokenOut)
    {
        if (data.length != 60) {
            revert UniswapV2Executor__InvalidDataLength();
        }
        target = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
    }

    function _getAmountOut(
        uint256 amountIn,
        uint112 reserveIn,
        uint112 reserveOut
    ) internal view returns (uint256 amount) {
        require(reserveIn > 0 && reserveOut > 0, "L");
        uint256 amountInWithFee = amountIn * (10000 - feeBps);
        uint256 numerator = amountInWithFee * uint256(reserveOut);
        uint256 denominator = (uint256(reserveIn) * 10000) + amountInWithFee;
        amount = numerator / denominator;
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
            revert UniswapV2Executor__InvalidDataLength();
        }
        address target = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));

        receiver = target;
        transferType = TransferManager.TransferType.Transfer;
        outputToRouter = false;
    }
}
