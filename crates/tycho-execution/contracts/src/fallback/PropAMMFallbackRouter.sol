// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IPropAMM} from "@interfaces/IPropAMM.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {TychoFallbackRouter} from "./TychoFallbackRouter.sol";

/// @title PropAMMFallbackRouter
/// @notice A `TychoFallbackRouter` whose primary is an `IPropAMM`.
contract PropAMMFallbackRouter is TychoFallbackRouter {
    using SafeERC20 for IERC20;

    constructor(
        IPoolManager poolManager_,
        address fluidLiquidity_,
        IUniswapV3StaticQuoter uniswapV3StaticQuoter_
    )
        TychoFallbackRouter(
            poolManager_, fluidLiquidity_, uniswapV3StaticQuoter_
        )
    {}

    /// @notice Quotes `pamm` and `fallbackSwap`, then runs the fallback if it quotes more
    /// `tokenOut`, otherwise `pamm` and, only if that fails, `fallbackSwap`. A failing fallback
    /// reverts the swap; there is no third attempt.
    /// @dev Permissionless: the caller names every parameter, so a balance sitting in this
    /// contract can be taken by anyone and is considered lost. Push-payment: the caller MUST
    /// transfer `swap_.amountIn` of `swap_.tokenIn` here first. Native ETH, fee-on-transfer and
    /// rebasing tokens are not supported.
    /// `fallbackSwap` names one of Uniswap V2, V3 or V4, Curve, Fluid V1 or Aerodrome V1.
    /// A fallback quote that reverts counts as zero, so equal quotes keep the pAMM. A pAMM that
    /// cannot quote skips both the fallback quote and its own swap.
    /// No output is returned: the caller measures its own `swap_.tokenOut` balance diff at
    /// `swap_.receiver`, which is how the Dispatcher verifies every swap.
    function swap(
        Swap calldata swap_,
        address pamm,
        bytes calldata fallbackSwap
    ) external {
        // A pAMM needs no data beyond its address.
        _swap(swap_, pamm, fallbackSwap[0:0], fallbackSwap);
    }

    function _quotePrimary(
        Swap calldata swap_,
        address pamm,
        bytes calldata /* primaryData */
    )
        internal
        override
        returns (uint256 amountOut)
    {
        // Low-level so a `pamm` without code, or one returning nothing decodable, quotes zero
        // instead of reverting `swap`. That covers `pamm == address(0)`, so no zero check.
        // slither-disable-next-line low-level-calls,missing-zero-check
        (bool quoted, bytes memory quote) = pamm.call(
            abi.encodeCall(
                IPropAMM.quote, (swap_.tokenIn, swap_.tokenOut, swap_.amountIn)
            )
        );
        return quoted && quote.length >= 32 ? abi.decode(quote, (uint256)) : 0;
    }

    function _swapPrimary(
        Swap calldata swap_,
        address pamm,
        bytes calldata /* primaryData */
    )
        internal
        override
    {
        IERC20(swap_.tokenIn).safeTransfer(pamm, swap_.amountIn);
        // slither-disable-next-line unused-return
        IPropAMM(pamm)
            .swap(
                swap_.tokenIn,
                swap_.tokenOut,
                swap_.amountIn,
                0,
                swap_.receiver,
                block.timestamp
            );
    }
}
