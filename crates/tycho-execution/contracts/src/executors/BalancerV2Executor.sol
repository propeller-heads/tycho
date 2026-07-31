// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {
    IERC20,
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
// slither-disable-next-line solc-version
import {IAsset} from "@balancer-labs/v2-interfaces/contracts/vault/IAsset.sol";
// slither-disable-next-line solc-version
import {IVault} from "@balancer-labs/v2-interfaces/contracts/vault/IVault.sol";
import {TransferManager} from "../TransferManager.sol";

error BalancerV2Executor__InvalidDataLength();

contract BalancerV2Executor is IExecutor {
    using SafeERC20 for IERC20;

    address private constant _VAULT =
        0xBA12222222228d8Ba445958a75a0704d566BF2C8;

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
        address tokenIn;
        address tokenOut;
        bytes32 poolId;

        (tokenIn, tokenOut, poolId) = _decodeData(data);

        IVault.SingleSwap memory singleSwap = IVault.SingleSwap({
            poolId: poolId,
            kind: IVault.SwapKind.GIVEN_IN,
            assetIn: IAsset(address(tokenIn)),
            assetOut: IAsset(address(tokenOut)),
            amount: amountIn,
            userData: ""
        });

        IVault.FundManagement memory funds = IVault.FundManagement({
            sender: address(this),
            fromInternalBalance: false,
            recipient: payable(receiver),
            toInternalBalance: false
        });

        uint256 limit = 0;

        // slither-disable-next-line unused-return
        IVault(_VAULT).swap(singleSwap, funds, limit, block.timestamp);
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address tokenIn, address tokenOut, bytes32 poolId)
    {
        if (data.length != 72) {
            revert BalancerV2Executor__InvalidDataLength();
        }

        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        poolId = bytes32(data[40:72]);
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
        if (data.length != 72) {
            revert BalancerV2Executor__InvalidDataLength();
        }

        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        receiver = _VAULT;
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        outputToRouter = false;
    }
}
