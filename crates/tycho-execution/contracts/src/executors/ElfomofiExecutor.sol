// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

interface IElfomofiRouter {
    function swapWithContractBalance(
        address fromToken,
        address toToken,
        uint256 minAmountOut,
        address receiver,
        uint256 partnerId
    ) external;
}

error ElfomofiExecutor__InvalidData();
error ElfomofiExecutor__InvalidRouter();
error ElfomofiExecutor__QuoteExpired();
error ElfomofiExecutor__UnsupportedPartner();
error ElfomofiExecutor__InvalidAmount();

/// @notice Pre-funded ERC20 swaps against a constructor-pinned ElFomoFi router.
/// @dev TransferManager sends input directly to ElFomoFi without a protocol approval.
///      ElFomoFi consumes its entire tokenIn balance, including pre-existing dust, and sends
///      output to receiver. The unsigned quote reserves no inventory; TychoRouter enforces
///      the route's minimum output and this executor enforces the quote's expiry.
contract ElfomofiExecutor is IExecutor {
    address public immutable elfomofiRouter;
    uint256 public immutable partnerId;

    constructor(address router, uint256 partner) {
        if (router == address(0)) revert ElfomofiExecutor__InvalidRouter();
        if (partner != 299) revert ElfomofiExecutor__UnsupportedPartner();
        elfomofiRouter = router;
        partnerId = partner;
    }

    function fundsExpectedAddress(bytes calldata)
        external
        view
        returns (address)
    {
        return elfomofiRouter;
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address tokenIn, address tokenOut, uint64 validUntil) =
            _decodeData(data);
        // Reject empty route legs rather than letting them consume pre-existing venue dust.
        if (amountIn == 0) revert ElfomofiExecutor__InvalidAmount();
        if (block.timestamp >= validUntil) {
            revert ElfomofiExecutor__QuoteExpired();
        }
        IElfomofiRouter(elfomofiRouter)
            .swapWithContractBalance({
            fromToken: tokenIn,
            toToken: tokenOut,
            minAmountOut: 0,
            receiver: receiver,
            partnerId: partnerId
        });
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
        (tokenIn, tokenOut,) = _decodeData(data);
        return (
            TransferManager.TransferType.Transfer,
            elfomofiRouter,
            tokenIn,
            tokenOut,
            false
        );
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address tokenIn, address tokenOut, uint64 validUntil)
    {
        if (data.length != 48) revert ElfomofiExecutor__InvalidData();
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        validUntil = uint64(bytes8(data[40:48]));
        if (
            tokenIn == tokenOut || tokenIn == address(0)
                || tokenOut == address(0) || tokenIn == ETH_ADDRESS
                || tokenOut == ETH_ADDRESS
        ) {
            revert ElfomofiExecutor__InvalidData();
        }
    }
}
