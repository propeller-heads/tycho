// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {TychoFallbackRouter} from "./TychoFallbackRouter.sol";

error LiquoriceFallbackRouter__AddressZero();
error LiquoriceFallbackRouter__InvalidDataLength(uint256 length);
error LiquoriceFallbackRouter__InvalidSelector(bytes4 selector);

/// @title LiquoriceFallbackRouter
/// @notice A `TychoFallbackRouter` whose primary is a signed Liquorice quote. The quote MUST name
/// this contract as trader and receiver, and Liquorice MUST allow it as a solver.
/// @dev The price is signed off-chain, so the quote runs first and the fallback runs only when it
/// fails. Partial fills are not supported on purpose: a Liquorice quote partially fills only when
/// its maker allows it, which is rare, so the settlement pulls the signed amount.
contract LiquoriceFallbackRouter is TychoFallbackRouter {
    using SafeERC20 for IERC20;
    using Address for address;

    bytes4 private constant _SETTLE_SINGLE_SELECTOR = 0x9935c868;
    bytes4 private constant _SETTLE_SELECTOR = 0xcba673a7;

    address public immutable liquoriceSettlement;
    /// @notice Pulls the input for the settlement.
    address public immutable liquoriceBalanceManager;

    constructor(
        IPoolManager poolManager_,
        address fluidLiquidity_,
        IUniswapV3StaticQuoter uniswapV3StaticQuoter_,
        address liquoriceSettlement_,
        address liquoriceBalanceManager_
    )
        TychoFallbackRouter(
            poolManager_, fluidLiquidity_, uniswapV3StaticQuoter_
        )
    {
        if (
            liquoriceSettlement_ == address(0)
                || liquoriceBalanceManager_ == address(0)
        ) {
            revert LiquoriceFallbackRouter__AddressZero();
        }
        liquoriceSettlement = liquoriceSettlement_;
        liquoriceBalanceManager = liquoriceBalanceManager_;
    }

    /// @notice Runs the Liquorice quote and, only if it fails, `fallbackSwap`. The quote fills
    /// its signed amount; the rest of `swap_.amountIn` goes back to the caller. When
    /// `swap_.amountIn` is below the signed amount, the settlement cannot pull its input, so the
    /// fallback fills the whole leg.
    /// @dev The caller MUST transfer `swap_.amountIn` of `swap_.tokenIn` here first.
    function swap(
        Swap calldata swap_,
        bytes calldata settlementCalldata,
        bytes calldata fallbackSwap
    ) external {
        if (settlementCalldata.length < 4) {
            revert LiquoriceFallbackRouter__InvalidDataLength(settlementCalldata.length);
        }
        bytes4 selector = bytes4(settlementCalldata[0:4]);
        if (selector != _SETTLE_SINGLE_SELECTOR && selector != _SETTLE_SELECTOR)
        {
            revert LiquoriceFallbackRouter__InvalidSelector(selector);
        }
        _swap(swap_, liquoriceSettlement, settlementCalldata, fallbackSwap);

        // A fallback swaps all of `amountIn`, so only a quote signed for less than `amountIn`
        // leaves a remainder.
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
        address, /* liquoriceSettlement */
        bytes calldata settlementCalldata
    ) internal override {
        IERC20 tokenIn = IERC20(swap_.tokenIn);
        IERC20 tokenOut = IERC20(swap_.tokenOut);
        uint256 balanceBefore = tokenOut.balanceOf(address(this));

        tokenIn.forceApprove(liquoriceBalanceManager, swap_.amountIn);
        // slither-disable-next-line unused-return
        liquoriceSettlement.functionCall(settlementCalldata);
        tokenIn.forceApprove(liquoriceBalanceManager, 0);

        tokenOut.safeTransfer(
            swap_.receiver, tokenOut.balanceOf(address(this)) - balanceBefore
        );
    }
}
