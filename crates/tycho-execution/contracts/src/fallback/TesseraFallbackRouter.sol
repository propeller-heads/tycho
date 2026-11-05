// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {TransientSlot} from "@openzeppelin/contracts/utils/TransientSlot.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {ITesseraSwap} from "../executors/TesseraExecutor.sol";
import {TychoFallbackRouter} from "./TychoFallbackRouter.sol";

error TesseraFallbackRouter__AddressZero();

/// @title TesseraFallbackRouter
/// @notice A `TychoFallbackRouter` whose primary is Tessera, reached through its `TesseraSwap`
/// router.
/// @dev `TesseraSwap` finds the pair by its two tokens, so the primary needs no data beyond the
/// swap. Its view quote is priced for this contract as the caller, as the swap is. The view leaves
/// out two things only the swap reads: the treasury's balance and the pair's fee module. The swap
/// must pay at least the quote, so a treasury that cannot pay it, or a fee module that charges,
/// reverts the swap and the fallback fills. The pair reads its caller's code on both the quote
/// and the swap, which costs about 1.3M gas each for a caller this contract's size.
contract TesseraFallbackRouter is TychoFallbackRouter {
    using SafeERC20 for IERC20;
    using TransientSlot for *;

    /// @dev `_quotePrimary`'s quote, which `_swapPrimary` requires the swap to pay.
    bytes32 private constant _TESSERA_QUOTE_SLOT =
        keccak256("TesseraFallbackRouter.tesseraQuote");

    ITesseraSwap public immutable tesseraSwap;

    constructor(
        IPoolManager poolManager_,
        address fluidLiquidity_,
        IUniswapV3StaticQuoter uniswapV3StaticQuoter_,
        address tesseraSwap_
    )
        TychoFallbackRouter(
            poolManager_, fluidLiquidity_, uniswapV3StaticQuoter_
        )
    {
        if (tesseraSwap_ == address(0)) {
            revert TesseraFallbackRouter__AddressZero();
        }
        tesseraSwap = ITesseraSwap(tesseraSwap_);
    }

    /// @notice Quotes Tessera and `fallbackSwap` and runs whichever quotes more; a Tessera swap
    /// that fails, or would pay less than Tessera quoted, falls through to `fallbackSwap`.
    /// @dev The caller MUST transfer `swap_.amountIn` of `swap_.tokenIn` here first.
    function swap(Swap calldata swap_, bytes calldata fallbackSwap) external {
        // Tessera needs no data beyond the swap.
        _swap(swap_, address(tesseraSwap), fallbackSwap[0:0], fallbackSwap);
    }

    function _quotePrimary(
        Swap calldata swap_,
        address, /* primary */
        bytes calldata /* primaryData */
    )
        internal
        override
        returns (uint256 amountOut)
    {
        amountOut = _viewQuote(swap_);
        _TESSERA_QUOTE_SLOT.asUint256().tstore(amountOut);
    }

    /// @dev Zero when Tessera cannot quote.
    function _viewQuote(Swap calldata swap_) private view returns (uint256) {
        // A negative amount would quote exact-out.
        // forge-lint: disable-next-line(unsafe-typecast)
        if (swap_.amountIn > uint256(type(int256).max)) {
            return 0;
        }
        // Low-level so a pair that cannot quote, such as one past its price cutoff, quotes zero
        // instead of reverting `swap`.
        // slither-disable-next-line low-level-calls
        (bool quoted, bytes memory quote) = address(tesseraSwap)
            .staticcall(
                abi.encodeCall(
                    ITesseraSwap.tesseraSwapViewAmounts,
                    (
                        swap_.tokenIn,
                        swap_.tokenOut,
                        // amountIn is checked above against int256.max.
                        // forge-lint: disable-next-line(unsafe-typecast)
                        int256(swap_.amountIn)
                    )
                )
            );
        if (!quoted || quote.length < 64) {
            return 0;
        }
        (, uint256 amountOut) = abi.decode(quote, (uint256, uint256));
        return amountOut;
    }

    /// @dev `TesseraSwap` pulls the input from this contract into Tessera's treasury and has the
    /// treasury pay `swap_.receiver`, at least Tessera's quote or it reverts. The approval is
    /// revoked once the swap is done.
    function _swapPrimary(
        Swap calldata swap_,
        address, /* primary */
        bytes calldata /* primaryData */
    )
        internal
        override
    {
        IERC20 tokenIn = IERC20(swap_.tokenIn);
        tokenIn.forceApprove(address(tesseraSwap), swap_.amountIn);
        // The pair reads swapData's first byte as a fee tag; empty swapData selects tag 0, as
        // `TesseraExecutor` swaps. `_quotePrimary` quotes zero above int256's maximum, so no swap
        // runs there.
        tesseraSwap.tesseraSwapWithAllowances(
            swap_.tokenIn,
            swap_.tokenOut,
            // forge-lint: disable-next-line(unsafe-typecast)
            int256(swap_.amountIn),
            _TESSERA_QUOTE_SLOT.asUint256().tload(),
            swap_.receiver,
            ""
        );
        tokenIn.forceApprove(address(tesseraSwap), 0);
    }
}
