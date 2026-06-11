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

interface IFewWrappedToken {
    function wrapTo(uint256 amount, address to) external returns (uint256);
    function unwrapTo(uint256 amount, address to) external returns (uint256);
}

error RingSwapV2Executor__InvalidDataLength();

contract RingSwapV2Executor is IExecutor {
    using SafeERC20 for IERC20;

    uint256 private constant FEE_BPS = 30;

    function fundsExpectedAddress(bytes calldata data)
        external
        view
        returns (address receiver)
    {
        _decodeData(data);
        return msg.sender;
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (
            address target,
            address tokenIn,,
            address fwTokenIn,
            address fwTokenOut
        ) = _decodeData(data);

        IERC20(tokenIn).forceApprove(fwTokenIn, amountIn);
        uint256 fwAmountIn =
            IFewWrappedToken(fwTokenIn).wrapTo(amountIn, target);
        IERC20(tokenIn).forceApprove(fwTokenIn, 0);

        bool zeroForOne = fwTokenIn < fwTokenOut;
        uint256 fwAmountOut = _swap(
            IUniswapV2Pair(target), fwAmountIn, zeroForOne, address(this)
        );

        uint256 amountOut =
            IFewWrappedToken(fwTokenOut).unwrapTo(fwAmountOut, receiver);
        require(amountOut > 0, "U");
    }

    function _swap(
        IUniswapV2Pair pool,
        uint256 amountIn,
        bool zeroForOne,
        address receiver
    ) internal returns (uint256 amountOut) {
        // slither-disable-next-line unused-return
        (uint112 reserve0, uint112 reserve1,) = pool.getReserves();
        uint112 reserveIn = zeroForOne ? reserve0 : reserve1;
        uint112 reserveOut = zeroForOne ? reserve1 : reserve0;

        amountOut = _getAmountOut(amountIn, reserveIn, reserveOut);

        if (zeroForOne) {
            pool.swap(0, amountOut, receiver, "");
        } else {
            pool.swap(amountOut, 0, receiver, "");
        }
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            address target,
            address tokenIn,
            address tokenOut,
            address fwTokenIn,
            address fwTokenOut
        )
    {
        if (data.length != 100) {
            revert RingSwapV2Executor__InvalidDataLength();
        }
        target = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
        fwTokenIn = address(bytes20(data[60:80]));
        fwTokenOut = address(bytes20(data[80:100]));
    }

    function _getAmountOut(
        uint256 amountIn,
        uint112 reserveIn,
        uint112 reserveOut
    ) internal pure returns (uint256 amount) {
        require(reserveIn > 0 && reserveOut > 0, "L");
        uint256 amountInWithFee = amountIn * (10000 - FEE_BPS);
        uint256 numerator = amountInWithFee * uint256(reserveOut);
        uint256 denominator = (uint256(reserveIn) * 10000) + amountInWithFee;
        amount = numerator / denominator;
    }

    function getTransferData(bytes calldata data)
        external
        view
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        (, address decodedTokenIn, address decodedTokenOut,,) =
            _decodeData(data);
        return (
            TransferManager.TransferType.Transfer,
            msg.sender,
            decodedTokenIn,
            decodedTokenOut,
            false
        );
    }
}
