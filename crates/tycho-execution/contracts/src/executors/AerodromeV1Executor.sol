// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

interface IAerodromeV1Pool {
    function getAmountOut(uint256 amountIn, address tokenIn)
        external
        view
        returns (uint256);
    function swap(
        uint256 amount0Out,
        uint256 amount1Out,
        address to,
        bytes calldata data
    ) external;
}

error AerodromeV1Executor__InvalidDataLength();

contract AerodromeV1Executor is IExecutor {
    function fundsExpectedAddress(bytes calldata data)
        external
        pure
        returns (address receiver)
    {
        return address(bytes20(data[0:20]));
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        address target;
        address tokenIn;
        bool zeroForOne;

        (target, tokenIn,, zeroForOne) = _decodeData(data);

        IAerodromeV1Pool pool = IAerodromeV1Pool(target);
        _swap(pool, amountIn, tokenIn, zeroForOne, receiver);
    }

    function _swap(
        IAerodromeV1Pool pool,
        uint256 amountIn,
        address tokenIn,
        bool zeroForOne,
        address receiver
    ) internal {
        uint256 amountOut = pool.getAmountOut(amountIn, tokenIn);
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
            bool zeroForOne
        )
    {
        if (data.length != 61) {
            revert AerodromeV1Executor__InvalidDataLength();
        }
        target = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
        zeroForOne = uint8(data[60]) > 0;
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
        address target;
        address decodedTokenIn;
        address decodedTokenOut;
        bool ignoredZeroForOne;
        (target, decodedTokenIn, decodedTokenOut, ignoredZeroForOne) =
            _decodeData(data);
        return (
            TransferManager.TransferType.Transfer,
            target,
            decodedTokenIn,
            decodedTokenOut,
            false
        );
    }
}
