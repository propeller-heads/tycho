// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {
    IUniswapV2Pair
} from "@uniswap-v2/contracts/interfaces/IUniswapV2Pair.sol";
import {TransferManager} from "../TransferManager.sol";

interface IFewWrappedToken {
    function wrapTo(uint256 amount, address to) external returns (uint256);
    function unwrapTo(uint256 amount, address to) external returns (uint256);
}

interface IFewFactory {
    function getWrappedToken(address originalToken)
        external
        view
        returns (address);
}

interface IRingSwapV2Factory {
    function getPair(address tokenA, address tokenB)
        external
        view
        returns (address);
}

error RingSwapV2Executor__InvalidDataLength();
error RingSwapV2Executor__InvalidFewToken(address token, address fwToken);
error RingSwapV2Executor__InvalidPair(
    address pair, address fwTokenIn, address fwTokenOut
);
error RingSwapV2Executor__InsufficientLiquidity();
error RingSwapV2Executor__ZeroFewFactory();
error RingSwapV2Executor__ZeroRingSwapFactory();

contract RingSwapV2Executor is IExecutor {
    uint256 private constant FEE_BPS = 30;
    address public immutable fewFactory;
    address public immutable ringSwapFactory;

    constructor(address fewFactory_, address ringSwapFactory_) {
        if (fewFactory_ == address(0)) {
            revert RingSwapV2Executor__ZeroFewFactory();
        }
        if (ringSwapFactory_ == address(0)) {
            revert RingSwapV2Executor__ZeroRingSwapFactory();
        }
        fewFactory = fewFactory_;
        ringSwapFactory = ringSwapFactory_;
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
            address target,
            address tokenIn,
            address tokenOut,
            address fwTokenIn,
            address fwTokenOut
        ) = _decodeData(data);

        _validateSwapData(target, tokenIn, tokenOut, fwTokenIn, fwTokenOut);

        uint256 fwAmountIn =
            IFewWrappedToken(fwTokenIn).wrapTo(amountIn, target);

        bool zeroForOne = fwTokenIn < fwTokenOut;
        uint256 fwAmountOut = _swap(
            IUniswapV2Pair(target), fwAmountIn, zeroForOne, address(this)
        );

        // slither-disable-next-line unused-return
        IFewWrappedToken(fwTokenOut).unwrapTo(fwAmountOut, receiver);
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

    function _validateFewToken(address token, address fwToken) internal view {
        if (
            fwToken == address(0)
                || IFewFactory(fewFactory).getWrappedToken(token) != fwToken
        ) {
            revert RingSwapV2Executor__InvalidFewToken(token, fwToken);
        }
    }

    function _validateSwapData(
        address target,
        address tokenIn,
        address tokenOut,
        address fwTokenIn,
        address fwTokenOut
    ) internal view {
        _validateFewToken(tokenIn, fwTokenIn);
        _validateFewToken(tokenOut, fwTokenOut);
        _validatePair(target, fwTokenIn, fwTokenOut);
    }

    function _validatePair(address pair, address fwTokenIn, address fwTokenOut)
        internal
        view
    {
        if (
            pair == address(0)
                || IRingSwapV2Factory(ringSwapFactory)
                        .getPair(fwTokenIn, fwTokenOut) != pair
        ) {
            revert RingSwapV2Executor__InvalidPair(pair, fwTokenIn, fwTokenOut);
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
        if (reserveIn == 0 || reserveOut == 0) {
            revert RingSwapV2Executor__InsufficientLiquidity();
        }
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
        (
            address target,
            address decodedTokenIn,
            address decodedTokenOut,
            address fwTokenIn,
            address fwTokenOut
        ) = _decodeData(data);
        _validateSwapData(
            target, decodedTokenIn, decodedTokenOut, fwTokenIn, fwTokenOut
        );
        return (
            TransferManager.TransferType.ProtocolWillDebit,
            fwTokenIn,
            decodedTokenIn,
            decodedTokenOut,
            false
        );
    }
}
