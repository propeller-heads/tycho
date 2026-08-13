// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import {ISwapAdapter} from "src/interfaces/ISwapAdapter.sol";
import {IERC20} from "openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";

/// @title TempestAdapter
/// @notice Adapter for swapping tokens on Tempest, Flowdesk's propAMM.
/// @dev Tempest prices from a quote "lane" the maker commits to the shared
/// `PrioUpdateRegistry`. The router only reads a lane whose committed
/// `updateTimestamp` sits inside `laneWindow` of `block.timestamp`, and reverts
/// `StaleUpdate` otherwise; simulation pins the timestamp through the
/// `override_block_timestamp` attribute the substreams package emits.
///
/// The adapter is quote-only: it never calls a settlement entrypoint. Every
/// `swap*` function on the router is gated on `allowedTaker[msg.sender]`, and
/// the adapter runs at a synthetic address that is not on that allowlist, so
/// executing settlement here would revert `TakerNotAllowed` regardless of the
/// pool's real state. Nothing is lost by quoting instead: `quote` and
/// `quoteExactOut` are `view`, ungated, and already enforce every condition
/// settlement would — pause state, pair registration, lane freshness, ladder
/// size, and (via `_checkVaultCover`) that the vault holds `amountOut` AND has
/// granted the router a standing allowance for it. A quote that succeeds is a
/// swap that would settle.
contract TempestAdapter is ISwapAdapter {
    /// Bounds the `getLimits` binary search for the largest quotable size.
    /// 8 iterations resolve the limit to within ~0.4% of the vault balance.
    uint256 private constant LIMIT_SEARCH_ITERATIONS = 8;

    /// Fraction of the traded size used to probe the post-trade marginal
    /// price. Tempest prices from a flat-segment spread ladder, so a
    /// small probe reads the current segment's rate rather than the
    /// size-weighted average a full re-quote would return.
    uint256 private constant PRICE_PROBE_DIVISOR = 1000;

    /// Measured cost of a `swapWithAllowances` fill: two ERC20 transfers plus
    /// the registry lane read and ladder walk. Reported instead of this call's
    /// own `gasleft()` delta, which would understate a real fill because the
    /// adapter quotes rather than settles.
    uint256 private constant SETTLEMENT_GAS = 130000;

    ITempest public immutable tempest;

    constructor(address tempest_) {
        tempest = ITempest(tempest_);
    }

    /// @inheritdoc ISwapAdapter
    function price(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        uint256[] memory specifiedAmounts
    ) external view override returns (Fraction[] memory calculatedPrices) {
        _validatePoolTokens(poolId, sellToken, buyToken);
        calculatedPrices = new Fraction[](specifiedAmounts.length);

        for (uint256 i = 0; i < specifiedAmounts.length; i++) {
            calculatedPrices[i] =
                priceAt(sellToken, buyToken, specifiedAmounts[i]);
        }
    }

    /// @notice The Tempest quote price for an exact input amount.
    function priceAt(address sellToken, address buyToken, uint256 sellAmount)
        public
        view
        returns (Fraction memory calculatedPrice)
    {
        if (sellAmount == 0) {
            revert TooSmall(0);
        }

        uint256 amountOut = tempest.quote(sellToken, buyToken, sellAmount);
        if (amountOut == 0) {
            revert TooSmall(0);
        }

        calculatedPrice = Fraction(amountOut, sellAmount);
    }

    /// @inheritdoc ISwapAdapter
    /// @dev See the contract-level note on why this quotes rather than settles.
    /// `gasUsed` is therefore reported as the venue's measured settlement cost
    /// rather than the gas this call consumed, which would understate a real
    /// fill by the two ERC20 transfers settlement performs.
    function swap(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        OrderSide side,
        uint256 specifiedAmount
    ) external view override returns (Trade memory trade) {
        if (specifiedAmount == 0) {
            return trade;
        }
        _validatePoolTokens(poolId, sellToken, buyToken);

        if (side == OrderSide.Sell) {
            trade.calculatedAmount =
                tempest.quote(sellToken, buyToken, specifiedAmount);
        } else {
            trade.calculatedAmount =
                tempest.quoteExactOut(sellToken, buyToken, specifiedAmount);
        }
        if (trade.calculatedAmount == 0) {
            revert TooSmall(0);
        }

        trade.gasUsed = SETTLEMENT_GAS;

        // Marginal price after the trade. The ladder is flat within a segment
        // and Tempest holds no reserves that a fill would move, so probing a
        // small size reads the rate the next trade would get. A probe that
        // falls under the ladder's minimum quotable size reverts; report zero
        // rather than failing a trade that priced fine.
        uint256 tradedAmount =
            side == OrderSide.Sell ? specifiedAmount : trade.calculatedAmount;
        uint256 probeAmount = tradedAmount / PRICE_PROBE_DIVISOR;
        if (probeAmount == 0) {
            probeAmount = tradedAmount;
        }
        try this.priceAt(sellToken, buyToken, probeAmount) returns (
            Fraction memory postTradePrice
        ) {
            trade.price = postTradePrice;
        } catch {
            trade.price = Fraction(0, 1);
        }
    }

    /// @inheritdoc ISwapAdapter
    /// @dev The sell-side limit is whatever the ladder still quotes, found by
    /// probing `quote`: it reverts `InsufficientLiquidity` both above the
    /// committed lane size and above what the vault can actually pay. The
    /// search is seeded from the vault's `buyToken` balance converted at the
    /// marginal rate, which is a hard ceiling on any fill.
    function getLimits(bytes32 poolId, address sellToken, address buyToken)
        external
        view
        override
        returns (uint256[] memory limits)
    {
        _validatePoolTokens(poolId, sellToken, buyToken);
        limits = new uint256[](2);

        if (!tempest.isActive(sellToken, buyToken)) {
            return limits;
        }

        uint256 vaultBalance = IERC20(buyToken).balanceOf(tempest.vault());
        if (vaultBalance == 0) {
            return limits;
        }

        // Convert the payable buy-side inventory into a sell-side ceiling at
        // the marginal rate. `quoteExactOut` reverts once the ladder cannot
        // cover the size, in which case fall back to a binary search.
        uint256 hi;
        try tempest.quoteExactOut(sellToken, buyToken, vaultBalance) returns (
            uint256 amountIn
        ) {
            limits[0] = amountIn;
            limits[1] = vaultBalance;
            return limits;
        } catch {
            hi = vaultBalance;
        }

        uint256 lo = 0;
        for (uint256 i = 0; i < LIMIT_SEARCH_ITERATIONS; i++) {
            uint256 mid = lo + (hi - lo) / 2;
            if (mid == lo) {
                break;
            }
            (bool quoted, uint256 amountIn) =
                _tryQuoteExactOut(sellToken, buyToken, mid);
            if (quoted) {
                limits[0] = amountIn;
                limits[1] = mid;
                lo = mid;
            } else {
                hi = mid;
            }
        }
    }

    function _tryQuoteExactOut(
        address sellToken,
        address buyToken,
        uint256 buyAmount
    ) internal view returns (bool ok, uint256 amountIn) {
        try tempest.quoteExactOut(sellToken, buyToken, buyAmount) returns (
            uint256 quotedIn
        ) {
            (ok, amountIn) = (true, quotedIn);
        } catch {}
    }

    /// @inheritdoc ISwapAdapter
    /// @dev Deliberately omits `ConstantPrice`. `LadderMath.amounts` VWAP-walks
    /// the lane's size bands, each carrying its own `spreadBps`, so an amount
    /// spanning more than one band gets a strictly worse effective rate --
    /// price impact, even though the rate is flat within a band. The live
    /// USDC/WETH lane runs three bands at 1, 2 and 5 bps, so this is not
    /// hypothetical. Declaring `ConstantPrice` would tell solvers to skip
    /// price-impact iteration and misprice anything crossing a breakpoint.
    ///
    /// The adapter test asserts only that the capability is absent, not the
    /// impact itself: at its pinned block the vault's inventory caps
    /// `getLimits` below the lane's first breakpoint, so no crossing is
    /// reachable and `quote` reverts `InsufficientLiquidity` above it.
    function getCapabilities(bytes32, address, address)
        external
        pure
        override
        returns (Capability[] memory capabilities)
    {
        capabilities = new Capability[](3);
        capabilities[0] = Capability.SellOrder;
        capabilities[1] = Capability.BuyOrder;
        capabilities[2] = Capability.PriceFunction;
    }

    /// @inheritdoc ISwapAdapter
    function getTokens(bytes32 poolId)
        external
        view
        override
        returns (address[] memory tokens)
    {
        ITempest.TokenPair[] memory pairs = tempest.getPairs();

        for (uint256 i = 0; i < pairs.length; i++) {
            if (_poolId(pairs[i].token0, pairs[i].token1) == poolId) {
                tokens = new address[](2);
                tokens[0] = pairs[i].token0;
                tokens[1] = pairs[i].token1;
                return tokens;
            }
        }

        revert InvalidOrder("Unknown pool");
    }

    /// @inheritdoc ISwapAdapter
    function getPoolIds(uint256 offset, uint256 limit)
        external
        view
        override
        returns (bytes32[] memory ids)
    {
        ITempest.TokenPair[] memory pairs = tempest.getPairs();
        if (offset >= pairs.length) {
            return new bytes32[](0);
        }

        uint256 endIndex = offset + limit;
        if (endIndex > pairs.length) {
            endIndex = pairs.length;
        }

        ids = new bytes32[](endIndex - offset);
        for (uint256 i = 0; i < ids.length; i++) {
            ITempest.TokenPair memory pair = pairs[offset + i];
            ids[i] = _poolId(pair.token0, pair.token1);
        }
    }

    function _validatePoolTokens(
        bytes32 poolId,
        address sellToken,
        address buyToken
    ) internal pure {
        if (poolId != _poolId(sellToken, buyToken)) {
            revert InvalidOrder("Pool/token mismatch");
        }
    }

    /// @dev Mirrors `Tempest.laneFor`, which the substreams package also uses
    /// as the component id, so a pool id is direction-independent.
    function _poolId(address tokenA, address tokenB)
        internal
        pure
        returns (bytes32)
    {
        (address token0, address token1) =
            tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);
        return keccak256(abi.encodePacked(token0, token1));
    }
}

interface ITempest {
    /// @dev Canonically ordered: `token0 < token1`.
    struct TokenPair {
        address token0;
        address token1;
    }

    function vault() external view returns (address);

    function isActive(address tokenIn, address tokenOut)
        external
        view
        returns (bool active);

    function getPairs() external view returns (TokenPair[] memory pairs);

    function laneFor(address tokenIn, address tokenOut)
        external
        pure
        returns (uint256);

    /// @dev Declared `view`: the `IPropAMM` interface allows an implementation
    /// to tighten this, and Tempest does.
    function quote(address tokenIn, address tokenOut, uint256 amountIn)
        external
        view
        returns (uint256 amountOut);

    function quoteExactOut(address tokenIn, address tokenOut, uint256 amountOut)
        external
        view
        returns (uint256 amountIn);
}
