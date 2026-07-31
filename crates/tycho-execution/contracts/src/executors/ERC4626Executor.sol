// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {IERC4626} from "@openzeppelin/contracts/interfaces/IERC4626.sol";
import {TransferManager} from "../TransferManager.sol";

error ERC4626Executor__InvalidDataLength();
error ERC4626Executor__InvalidTarget();

contract ERC4626Executor is IExecutor {
    using SafeERC20 for IERC20;

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
        IERC20 tokenIn;

        (tokenIn, target) = _decodeData(data);

        bool isRedeem = (address(tokenIn) == target);
        if (!isRedeem && address(tokenIn) != IERC4626(target).asset()) {
            revert ERC4626Executor__InvalidTarget();
        }

        if (isRedeem) {
            // slither-disable-next-line unused-return
            IERC4626(target).redeem(amountIn, receiver, address(this));
        } else {
            // slither-disable-next-line unused-return
            IERC4626(target).deposit(amountIn, receiver);
        }
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (IERC20 tokenIn, address target)
    {
        if (data.length != 40) {
            revert ERC4626Executor__InvalidDataLength();
        }
        tokenIn = IERC20(address(bytes20(data[0:20])));
        target = address(bytes20(data[20:40]));
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
        if (data.length != 40) {
            revert ERC4626Executor__InvalidDataLength();
        }
        tokenIn = address(bytes20(data[0:20]));
        address target = address(bytes20(data[20:40]));
        receiver = target;
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        outputToRouter = false;

        bool isRedeem = (tokenIn == target);
        if (isRedeem) {
            tokenOut = IERC4626(target).asset();
        } else {
            tokenOut = target;
        }
    }
}
