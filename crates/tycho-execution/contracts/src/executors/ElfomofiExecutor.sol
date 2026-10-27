// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {Math} from "@openzeppelin/contracts/utils/math/Math.sol";
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
error ElfomofiExecutor__InvalidAmount();

/// @notice Directly funded ERC20 swaps with a snapshot deadline and minimum output rate.
contract ElfomofiExecutor is IExecutor {
    address public immutable elfomofiRouter;
    uint256 public constant partnerId = 299;

    constructor(address router) {
        if (router == address(0)) revert ElfomofiExecutor__InvalidRouter();
        elfomofiRouter = router;
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
        (
            address tokenIn,
            address tokenOut,
            uint64 validUntil,
            uint256 referenceIn,
            uint256 referenceMinOut
        ) = _decodeData(data);
        if (amountIn == 0) revert ElfomofiExecutor__InvalidAmount();
        // Enforce the snapshot deadline.
        // slither-disable-next-line timestamp
        if (block.timestamp >= validUntil) {
            revert ElfomofiExecutor__QuoteExpired();
        }
        // Scale to the actual preceding leg's output, retaining a positive minimum after rounding.
        uint256 minimum = Math.mulDiv(
            amountIn, referenceMinOut, referenceIn, Math.Rounding.Ceil
        );
        IElfomofiRouter(elfomofiRouter).swapWithContractBalance({
            fromToken: tokenIn,
            toToken: tokenOut,
            minAmountOut: minimum,
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
        (tokenIn, tokenOut,,,) = _decodeData(data);
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
        returns (
            address tokenIn,
            address tokenOut,
            uint64 validUntil,
            uint256 referenceIn,
            uint256 referenceMinOut
        )
    {
        if (data.length != 112) {
            revert ElfomofiExecutor__InvalidData();
        }
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        validUntil = uint64(bytes8(data[40:48]));
        referenceIn = uint256(bytes32(data[48:80]));
        referenceMinOut = uint256(bytes32(data[80:112]));
        if (
            tokenIn == tokenOut || tokenIn == address(0)
                || tokenOut == address(0) || tokenIn == ETH_ADDRESS
                || tokenOut == ETH_ADDRESS || referenceIn == 0
                || referenceMinOut == 0
        ) revert ElfomofiExecutor__InvalidData();
    }
}
