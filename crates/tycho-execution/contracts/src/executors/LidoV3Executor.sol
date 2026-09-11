// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

error LidoV3Executor__InvalidDataLength();
error LidoV3Executor__InvalidDirection();
error LidoV3Executor__ZeroAddress();

interface IStETH is IERC20 {
    function submit(address referral) external payable returns (uint256);
}

interface IWstETH is IERC20 {
    function wrap(uint256 stETHAmount) external returns (uint256);
    function unwrap(uint256 wstETHAmount) external returns (uint256);
}

enum LidoV3Direction {
    EthToStEth,
    StEthToWstEth,
    WstEthToStEth
}

contract LidoV3Executor is IExecutor {
    IStETH public immutable stEth;
    IWstETH public immutable wstEth;

    constructor(address stEthAddress, address wstEthAddress) {
        if (stEthAddress == address(0) || wstEthAddress == address(0)) {
            revert LidoV3Executor__ZeroAddress();
        }

        stEth = IStETH(stEthAddress);
        wstEth = IWstETH(wstEthAddress);
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
    function swap(
        uint256 amountIn,
        bytes calldata data,
        address /* receiver */
    )
        external
        payable
    {
        LidoV3Direction direction = _decodeData(data);

        // The Dispatcher measures output via balance checks, so the minted or
        // unwrapped amounts these calls return are not needed here.
        if (direction == LidoV3Direction.EthToStEth) {
            // slither-disable-next-line arbitrary-send-eth,unused-return
            stEth.submit{value: amountIn}(address(0));
        } else if (direction == LidoV3Direction.StEthToWstEth) {
            // slither-disable-next-line unused-return
            wstEth.wrap(amountIn);
        } else if (direction == LidoV3Direction.WstEthToStEth) {
            // slither-disable-next-line unused-return
            wstEth.unwrap(amountIn);
        } else {
            revert LidoV3Executor__InvalidDirection();
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
        LidoV3Direction direction = _decodeData(data);

        if (direction == LidoV3Direction.EthToStEth) {
            transferType = TransferManager.TransferType.TransferNativeInExecutor;
            receiver = msg.sender;
            tokenIn = ETH_ADDRESS;
            tokenOut = address(stEth);
        } else if (direction == LidoV3Direction.StEthToWstEth) {
            transferType = TransferManager.TransferType.ProtocolWillDebit;
            receiver = address(wstEth);
            tokenIn = address(stEth);
            tokenOut = address(wstEth);
        } else if (direction == LidoV3Direction.WstEthToStEth) {
            transferType = TransferManager.TransferType.ProtocolWillDebit;
            receiver = msg.sender;
            tokenIn = address(wstEth);
            tokenOut = address(stEth);
        } else {
            revert LidoV3Executor__InvalidDirection();
        }

        outputToRouter = true;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (LidoV3Direction direction)
    {
        if (data.length != 1) {
            revert LidoV3Executor__InvalidDataLength();
        }

        uint8 rawDirection = uint8(data[0]);
        if (rawDirection > uint8(LidoV3Direction.WstEthToStEth)) {
            revert LidoV3Executor__InvalidDirection();
        }

        direction = LidoV3Direction(rawDirection);
    }
}
