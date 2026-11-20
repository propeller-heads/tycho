// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {IBaibaiEntrypoint} from "../executors/BaibaiExecutor.sol";
import {TychoFallbackRouter} from "./TychoFallbackRouter.sol";

error BaibaiFallbackRouter__AddressZero();
error BaibaiFallbackRouter__ZeroQuoteToken();
error BaibaiFallbackRouter__NotAPair(address tokenIn, address tokenOut);

/// @dev From BaiBai's taker documentation (docs.baibai.cx/takers/quoting-and-swapping).
interface IBaibaiQuoter {
    function quoteFor(
        address base,
        address tokenIn,
        uint256 amountIn,
        address taker
    ) external view returns (uint256 amountOut, uint256 fee);
}

/// @title BaibaiFallbackRouter
/// @notice A `TychoFallbackRouter` whose primary is one BaiBai entrypoint. Every pair trades a
/// base token against the entrypoint's quote token, so a swap's tokens name the pair.
/// @dev BaiBai charges each taker its own fee, and this router is the taker, so it quotes
/// `quoteFor` for itself.
contract BaibaiFallbackRouter is TychoFallbackRouter {
    using SafeERC20 for IERC20;

    /// `quoteFor` returns two words.
    uint256 private constant _QUOTE_LENGTH = 64;

    IBaibaiEntrypoint public immutable entrypoint;
    address public immutable quoteToken;

    constructor(
        IPoolManager poolManager_,
        address fluidLiquidity_,
        IUniswapV3StaticQuoter uniswapV3StaticQuoter_,
        address entrypoint_
    )
        TychoFallbackRouter(
            poolManager_, fluidLiquidity_, uniswapV3StaticQuoter_
        )
    {
        if (entrypoint_ == address(0)) {
            revert BaibaiFallbackRouter__AddressZero();
        }
        entrypoint = IBaibaiEntrypoint(entrypoint_);
        quoteToken = entrypoint.quoteToken();
        if (quoteToken == address(0)) {
            revert BaibaiFallbackRouter__ZeroQuoteToken();
        }
    }

    /// @notice Quotes BaiBai and `fallbackSwap` and runs whichever quotes more; a BaiBai swap
    /// that fails falls through to `fallbackSwap`.
    /// @dev The caller MUST transfer `swap_.amountIn` of `swap_.tokenIn` here first.
    function swap(Swap calldata swap_, bytes calldata fallbackSwap) external {
        // BaiBai needs no data beyond the swap's tokens.
        _swap(swap_, address(entrypoint), fallbackSwap[0:0], fallbackSwap);
    }

    /// @dev Low-level so an entrypoint that reverts, or returns nothing decodable after an
    /// upgrade, quotes zero instead of reverting `swap`.
    function _quotePrimary(
        Swap calldata swap_,
        address, /* primary */
        bytes calldata /* primaryData */
    )
        internal
        view
        override
        returns (uint256 amountOut)
    {
        address base = _base(swap_);
        if (base == address(0)) {
            return 0;
        }
        // forge-lint: disable-next-item(low-level-calls) An entrypoint that cannot quote must quote zero, not revert, see above.
        (bool quoted, bytes memory quote) = address(entrypoint)
            .staticcall(
                abi.encodeCall(
                    IBaibaiQuoter.quoteFor,
                    (base, swap_.tokenIn, swap_.amountIn, address(this))
                )
            );
        if (!quoted || quote.length < _QUOTE_LENGTH) {
            return 0;
        }
        (amountOut,) = abi.decode(quote, (uint256, uint256));
    }

    function _swapPrimary(
        Swap calldata swap_,
        address, /* primary */
        bytes calldata /* primaryData */
    )
        internal
        override
    {
        address base = _base(swap_);
        if (base == address(0)) {
            revert BaibaiFallbackRouter__NotAPair(swap_.tokenIn, swap_.tokenOut);
        }
        IERC20 tokenIn = IERC20(swap_.tokenIn);
        tokenIn.forceApprove(address(entrypoint), swap_.amountIn);
        // slither-disable-next-line unused-return The fallback router measures the output at the receiver.
        entrypoint.swapExactAmountIn(
            base, swap_.tokenIn, swap_.amountIn, 0, swap_.receiver
        );
        tokenIn.forceApprove(address(entrypoint), 0);
    }

    /// @dev The swap's non-quote token, or zero when exactly one of its tokens is not the quote
    /// token.
    function _base(Swap calldata swap_) internal view returns (address) {
        if (swap_.tokenIn == quoteToken && swap_.tokenOut != quoteToken) {
            return swap_.tokenOut;
        }
        if (swap_.tokenOut == quoteToken && swap_.tokenIn != quoteToken) {
            return swap_.tokenIn;
        }
        return address(0);
    }
}
