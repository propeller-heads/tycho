// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {IHashflowRouter} from "../executors/HashflowExecutor.sol";
import {TychoFallbackRouter} from "./TychoFallbackRouter.sol";

error HashflowFallbackRouter__AddressZero();
error HashflowFallbackRouter__InvalidDataLength(uint256 length);

/// @title HashflowFallbackRouter
/// @notice A `TychoFallbackRouter` whose primary is a signed Hashflow quote. The quote MUST name
/// this contract as trader.
/// @dev The price is signed off-chain, so the quote runs first and the fallback runs only when it
/// fails.
contract HashflowFallbackRouter is TychoFallbackRouter {
    using SafeERC20 for IERC20;

    uint256 private constant _QUOTE_LENGTH = 345;

    address public immutable hashflowRouter;

    constructor(
        IPoolManager poolManager_,
        address fluidLiquidity_,
        IUniswapV3StaticQuoter uniswapV3StaticQuoter_,
        address hashflowRouter_
    )
        TychoFallbackRouter(
            poolManager_, fluidLiquidity_, uniswapV3StaticQuoter_
        )
    {
        if (hashflowRouter_ == address(0)) {
            revert HashflowFallbackRouter__AddressZero();
        }
        hashflowRouter = hashflowRouter_;
    }

    /// @notice Runs the Hashflow quote and, only if it fails, `fallbackSwap`. The quote fills at
    /// most its signed amount; the rest of `swap_.amountIn` goes back to the caller.
    /// @dev The caller MUST transfer `swap_.amountIn` of `swap_.tokenIn` here first.
    function swap(
        Swap calldata swap_,
        bytes calldata hashflowQuote,
        bytes calldata fallbackSwap
    ) external {
        if (hashflowQuote.length != _QUOTE_LENGTH) {
            revert HashflowFallbackRouter__InvalidDataLength(hashflowQuote.length);
        }
        // The quote's pool names the primary in `FallbackSwap`.
        _swap(
            swap_,
            address(bytes20(hashflowQuote[0:20])),
            hashflowQuote,
            fallbackSwap
        );

        // A fallback swaps all of `amountIn`, so only a capped quote leaves a remainder.
        IERC20 tokenIn = IERC20(swap_.tokenIn);
        uint256 remainder = tokenIn.balanceOf(address(this));
        if (remainder > 0) {
            tokenIn.safeTransfer(msg.sender, remainder);
        }
    }

    function _runsPrimaryFirst() internal pure override returns (bool) {
        return true;
    }

    function _swapPrimary(
        Swap calldata swap_,
        address, /* pool */
        bytes calldata hashflowQuote
    ) internal override {
        IHashflowRouter.RFQTQuote memory quote = _decodeQuote(hashflowQuote);
        quote.effectiveBaseTokenAmount = swap_.amountIn < quote.baseTokenAmount
            ? swap_.amountIn
            : quote.baseTokenAmount;
        IERC20 tokenIn = IERC20(swap_.tokenIn);
        IERC20 tokenOut = IERC20(swap_.tokenOut);
        uint256 balanceBefore = tokenOut.balanceOf(address(this));

        tokenIn.forceApprove(hashflowRouter, quote.effectiveBaseTokenAmount);
        IHashflowRouter(hashflowRouter).tradeRFQT(quote);
        tokenIn.forceApprove(hashflowRouter, 0);

        tokenOut.safeTransfer(
            swap_.receiver, tokenOut.balanceOf(address(this)) - balanceBefore
        );
    }

    function _decodeQuote(bytes calldata data)
        internal
        pure
        returns (IHashflowRouter.RFQTQuote memory quote)
    {
        quote.pool = address(bytes20(data[0:20]));
        quote.externalAccount = address(bytes20(data[20:40]));
        quote.trader = address(bytes20(data[40:60]));
        quote.effectiveTrader = address(bytes20(data[60:80]));
        quote.baseToken = address(bytes20(data[80:100]));
        quote.quoteToken = address(bytes20(data[100:120]));
        quote.baseTokenAmount = uint256(bytes32(data[120:152]));
        quote.quoteTokenAmount = uint256(bytes32(data[152:184]));
        quote.quoteExpiry = uint256(bytes32(data[184:216]));
        quote.nonce = uint256(bytes32(data[216:248]));
        quote.txid = bytes32(data[248:280]);
        quote.signature = data[280:345];
    }
}
