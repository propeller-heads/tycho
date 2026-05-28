// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {
    IERC20,
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";

interface IWrapped is IERC20 {
    function deposit() external payable;
    function withdraw(uint256) external;
}

error NativeWrapExecutor__InvalidDataLength();
error NativeWrapExecutor__ZeroAddress();

contract NativeWrapExecutor is IExecutor {
    using SafeERC20 for IWrapped;
    using SafeERC20 for IERC20;

    IWrapped public immutable wrapped;

    constructor(address wrappedAddress) {
        if (wrappedAddress == address(0)) {
            revert NativeWrapExecutor__ZeroAddress();
        }
        wrapped = IWrapped(wrappedAddress);
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
        bool isWrapping;
        isWrapping = _decodeData(data);

        if (isWrapping) {
            // Native -> Wrapped: Wrap
            wrapped.deposit{value: amountIn}();
        } else {
            // Wrapped -> Native: Unwrap
            wrapped.withdraw(amountIn);
        }
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (bool isWrapping)
    {
        if (data.length != 1) {
            revert NativeWrapExecutor__InvalidDataLength();
        }

        isWrapping = uint8(data[0]) == 1;
        return isWrapping;
    }

    /// @dev Required to receive ETH
    receive() external payable {}

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
        if (data.length != 1) {
            revert NativeWrapExecutor__InvalidDataLength();
        }

        bool isWrapping = uint8(data[0]) == 1;

        if (isWrapping) {
            // Native -> Wrapped: Wrap
            tokenIn = ETH_ADDRESS;
            tokenOut = address(wrapped);
            transferType = TransferManager.TransferType.TransferNativeInExecutor;
        } else {
            // Wrapped -> Native: Unwrap
            tokenIn = address(wrapped);
            tokenOut = ETH_ADDRESS;
            transferType = TransferManager.TransferType.ProtocolWillDebit;
        }

        outputToRouter = true;
        // Since unwrapping withdraws the funds from the msg.sender, the user's funds need to be sent to the
        // TychoRouter initially. This does not require an actual approval since our
        // router is interacting directly with the token contract.
        // We use msg.sender (the TychoRouter) instead of address(this) because
        // getTransferData is called via staticcall.
        receiver = msg.sender;
    }
}
