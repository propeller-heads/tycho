// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {IPropAMMRouter} from "@interfaces/IPropAMMRouter.sol";
import {TransferManager} from "../TransferManager.sol";

error PropAMMFallbackExecutor__InvalidDataLength();

/// @title PropAMMFallbackExecutor
/// @notice Swaps against a pAMM through Titan's PropAMMRouter, which falls back to a single-hop
/// Uniswap V3 pool when the venue reverts.
/// @dev Same calldata as `PropAMMExecutor`, different call target. Calling the venue directly lets
/// a stale maker quote revert the whole route, which is why integrator simulations fail on routes
/// that execute fine in a Titan block.
///
/// Only venues whitelisted on the PropAMMRouter work here; others keep using `PropAMMExecutor`.
///
/// `amountOutMin` is 0 on the router call: any non-zero value would make the Uniswap fallback
/// revert on price for the trades it exists to rescue. The TychoRouter's route-level `minAmountOut`
/// is the binding check, so the caller must set it low enough for the Uniswap leg to clear.
contract PropAMMFallbackExecutor is IExecutor {
    /// @notice The PropAMMRouter serving Titan's pAMM ecosystem, on Ethereum mainnet.
    /// @dev Hardcoded rather than a constructor argument. There is one deployment, it sits behind a
    /// UUPS proxy so upgrades keep this address, and a wrong value here would route every swap to an
    /// arbitrary contract. Source: https://github.com/lambdaclass/propamm-router-contracts
    IPropAMMRouter public constant PROPAMM_ROUTER =
        IPropAMMRouter(0x4DdF368080CD7946db5b459aD591c350158175e1);

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
        (address venue, address tokenIn, address tokenOut) = _decodeData(data);

        // slither-disable-next-line unused-return
        PROPAMM_ROUTER.swapViaVenueV1(
            venue, tokenIn, tokenOut, amountIn, 0, receiver, block.timestamp
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
        (, tokenIn, tokenOut) = _decodeData(data);
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        receiver = address(PROPAMM_ROUTER);
        outputToRouter = false;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address venue, address tokenIn, address tokenOut)
    {
        if (data.length != 60) {
            revert PropAMMFallbackExecutor__InvalidDataLength();
        }

        venue = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
    }
}
