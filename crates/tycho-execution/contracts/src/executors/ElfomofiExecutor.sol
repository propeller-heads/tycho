// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {Math} from "@openzeppelin/contracts/utils/math/Math.sol";
import {TransferManager} from "../TransferManager.sol";

/// @dev ElFomoFi's router (`ElfomoFi`, the same non-upgradeable deployment
///      on Base, BSC and Robinhood). The function swaps the router's whole
///      balance of `fromToken`, pays `toToken` from the venue's vault to
///      `receiver`, and reverts when that falls below `minAmountOut`.
interface IElfomofiRouter {
    function swapWithContractBalance(
        address fromToken,
        address toToken,
        uint256 minAmountOut,
        address receiver,
        uint256 partnerId
    ) external;
}

/// @title ElfomofiExecutor
/// @notice Swaps through ElFomoFi's router, which the TychoRouter funds with
/// the input before this executor runs.
/// @dev The venue caps its output at the end of its book and keeps the whole
/// input when a swap exceeds the book, so the protocol-level minimum is not
/// neutral: the swap carries the floor the encoder priced for
/// `referenceIn`, and this executor scales it to the actual `amountIn`,
/// rounding up, when the input is smaller (a split or an earlier hop that paid
/// less than estimated). An input at or above `referenceIn` keeps the absolute
/// floor.
///
/// Data, 104 bytes: tokenIn (20) | tokenOut (20) | referenceIn (32) |
/// referenceMinOut (32).
contract ElfomofiExecutor is IExecutor {
    error ElfomofiExecutor__InvalidDataLength();
    error ElfomofiExecutor__InvalidToken();
    error ElfomofiExecutor__ZeroAmount();
    error ElfomofiExecutor__ZeroAddress();

    /// @dev The partner id the router logs with each swap. The venue does not
    /// check it; it only attributes the swap in the router's events.
    uint256 public constant PARTNER_ID = 0;

    /// @dev The native-token marker of the TychoRouter. ElFomoFi trades ERC20s
    /// only.
    address private constant _NATIVE_MARKER =
        0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE;

    IElfomofiRouter public immutable elfomofiRouter;

    /// @param elfomofiRouter_ ElFomoFi's router on the deployment chain.
    constructor(address elfomofiRouter_) {
        if (elfomofiRouter_ == address(0)) {
            revert ElfomofiExecutor__ZeroAddress();
        }
        elfomofiRouter = IElfomofiRouter(elfomofiRouter_);
    }

    /// @inheritdoc IExecutor
    function fundsExpectedAddress(bytes calldata)
        external
        view
        returns (address)
    {
        return address(elfomofiRouter);
    }

    /// @inheritdoc IExecutor
    // IExecutor is payable for delegatecall from the router; ElFomoFi swaps use
    // ERC20s only.
    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        if (amountIn == 0) revert ElfomofiExecutor__ZeroAmount();
        (
            address tokenIn,
            address tokenOut,
            uint256 referenceIn,
            uint256 referenceMinOut
        ) = _decode(data);
        uint256 floor = amountIn >= referenceIn
            ? referenceMinOut
            : Math.mulDiv(
                amountIn, referenceMinOut, referenceIn, Math.Rounding.Ceil
            );
        elfomofiRouter.swapWithContractBalance(
            tokenIn, tokenOut, floor, receiver, PARTNER_ID
        );
    }

    /// @inheritdoc IExecutor
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
        (tokenIn, tokenOut,,) = _decode(data);
        return (
            TransferManager.TransferType.Transfer,
            address(elfomofiRouter),
            tokenIn,
            tokenOut,
            false
        );
    }

    function _decode(bytes calldata data)
        internal
        pure
        returns (
            address tokenIn,
            address tokenOut,
            uint256 referenceIn,
            uint256 referenceMinOut
        )
    {
        if (data.length != 104) {
            revert ElfomofiExecutor__InvalidDataLength();
        }
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        referenceIn = uint256(bytes32(data[40:72]));
        referenceMinOut = uint256(bytes32(data[72:104]));
        if (
            tokenIn == tokenOut || tokenIn == address(0)
                || tokenOut == address(0) || tokenIn == _NATIVE_MARKER
                || tokenOut == _NATIVE_MARKER
        ) {
            revert ElfomofiExecutor__InvalidToken();
        }
        if (referenceIn == 0 || referenceMinOut == 0) {
            revert ElfomofiExecutor__ZeroAmount();
        }
    }
}
