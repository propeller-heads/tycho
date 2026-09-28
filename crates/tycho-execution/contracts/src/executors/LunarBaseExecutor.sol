// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

interface ILunarBasePool {
    struct ExactInputParams {
        address tokenIn;
        address tokenOut;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint256 deadline;
    }

    function swapExactIn(ExactInputParams calldata params)
        external
        returns (uint256 amountOut);

    function swapExactInNative(
        address tokenOut,
        address recipient,
        uint256 amountOutMinimum,
        uint256 deadline
    ) external payable returns (uint256 amountOut);
}

error LunarBaseExecutor__InvalidDataLength();

contract LunarBaseExecutor is IExecutor {
    address internal constant LUNARBASE_NATIVE_TOKEN =
        0x0000000000000000000000000000000000000000;
    uint256 internal constant DATA_LENGTH = 60;

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
        (address pool, address tokenIn, address tokenOut,) = _decodeData(data);

        if (tokenIn == ETH_ADDRESS) {
            // slither-disable-next-line arbitrary-send-eth,unused-return
            ILunarBasePool(pool).swapExactInNative{value: amountIn}(
                _toLunarBaseToken(tokenOut), receiver, 0, block.timestamp
            );
            return;
        }

        // slither-disable-next-line unused-return
        ILunarBasePool(pool)
            .swapExactIn(
                ILunarBasePool.ExactInputParams({
                    tokenIn: _toLunarBaseToken(tokenIn),
                    tokenOut: _toLunarBaseToken(tokenOut),
                    recipient: receiver,
                    amountIn: amountIn,
                    amountOutMinimum: 0,
                    deadline: block.timestamp
                })
            );
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
        address pool;
        (pool, tokenIn, tokenOut,) = _decodeData(data);

        if (tokenIn == ETH_ADDRESS) {
            transferType = TransferManager.TransferType.TransferNativeInExecutor;
            receiver = address(0);
        } else {
            transferType = TransferManager.TransferType.ProtocolWillDebit;
            receiver = pool;
        }
        outputToRouter = false;
    }

    function _toLunarBaseToken(address token) internal pure returns (address) {
        return token == ETH_ADDRESS ? LUNARBASE_NATIVE_TOKEN : token;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            address pool,
            address tokenIn,
            address tokenOut,
            uint256 amountOutMinimum
        )
    {
        if (data.length != DATA_LENGTH) {
            revert LunarBaseExecutor__InvalidDataLength();
        }
        pool = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
        amountOutMinimum = 0;
    }
}
