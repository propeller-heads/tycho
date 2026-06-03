// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

error BaselineExecutor__InvalidDataLength();
error BaselineExecutor__InvalidTokenPair();
error BaselineExecutor__ZeroAddress();

interface IBaselineRelay {
    function reserve(address bToken) external view returns (address);

    function buyTokensExactIn(address bToken, uint256 amountIn, uint256 limitAmount)
        external
        returns (uint256 amountOut, uint256 feesReceived);

    function sellTokensExactIn(address bToken, uint256 amountIn, uint256 limitAmount)
        external
        returns (uint256 amountOut, uint256 feesReceived);
}

contract BaselineExecutor is IExecutor {
    address public immutable relay;

    constructor(address relay_) {
        if (relay_ == address(0)) {
            revert BaselineExecutor__ZeroAddress();
        }
        relay = relay_;
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

    function swap(
        uint256 amountIn,
        bytes calldata data,
        address /* receiver */
    )
        external
        payable
    {
        (address bToken, address tokenIn, address tokenOut) = _decodeData(data);
        _validateTokenPair(bToken, tokenIn, tokenOut);

        if (tokenOut == bToken) {
            IBaselineRelay(relay).buyTokensExactIn(bToken, amountIn, 0);
        } else {
            IBaselineRelay(relay).sellTokensExactIn(bToken, amountIn, 0);
        }
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
        address bToken;
        (bToken, tokenIn, tokenOut) = _decodeData(data);
        _validateTokenPair(bToken, tokenIn, tokenOut);

        transferType = TransferManager.TransferType.ProtocolWillDebit;
        receiver = relay;
        outputToRouter = true;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address bToken, address tokenIn, address tokenOut)
    {
        if (data.length != 60) {
            revert BaselineExecutor__InvalidDataLength();
        }

        bToken = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
    }

    function _validateTokenPair(address bToken, address tokenIn, address tokenOut) internal view {
        address reserve_ = IBaselineRelay(relay).reserve(bToken);
        bool isBuy = tokenOut == bToken && tokenIn == reserve_;
        bool isSell = tokenIn == bToken && tokenOut == reserve_;

        if (reserve_ == address(0) || (!isBuy && !isSell)) {
            revert BaselineExecutor__InvalidTokenPair();
        }
    }
}
