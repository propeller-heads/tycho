// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import {ISwapAdapter} from "src/interfaces/ISwapAdapter.sol";
import {
    IERC20,
    SafeERC20
} from "openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";
import {Math} from "openzeppelin-contracts/contracts/utils/math/Math.sol";
import {
    SafeCast
} from "openzeppelin-contracts/contracts/utils/math/SafeCast.sol";
import {CamelotV3TickMath} from "src/camelot-v3/CamelotV3TickMath.sol";
import {IAlgebraFactory, IAlgebraPool} from "src/camelot-v3/IAlgebraPool.sol";

/// @title CamelotV3SwapAdapter
/// @notice Simulates swaps on Camelot V3 pools (Algebra V1.9 on Arbitrum One).
/// @dev The pool is executed as deployed: its directional adaptive fee, oracle
/// timepoints and tick spacing are whatever its storage holds. The adapter
/// pays for swaps in the pool's callback, quotes swaps by reverting inside
/// that callback, and derives marginal prices from the pool's sqrt price and
/// the fee it will charge next.
contract CamelotV3SwapAdapter is ISwapAdapter {
    using SafeERC20 for IERC20;

    /// @dev TickMath.MIN_SQRT_RATIO / MAX_SQRT_RATIO of the pool. A swap's
    /// price limit must lie strictly inside this range.
    uint160 internal constant _MIN_SQRT_RATIO = 4295128739;
    uint160 internal constant _MAX_SQRT_RATIO =
        1461446703485210103287273052203988822378723970342;
    /// @dev Pool fees are expressed in hundredths of a bip.
    uint256 internal constant _FEE_DENOMINATOR = 1e6;
    /// @dev Upper bound on the tick-table steps a limits walk takes. A step
    /// reads one `tickTable` word, the `ticks` entry when the tick is
    /// initialized (four cold slots), and runs the price math: at most about
    /// 17.5k gas, and 8.4k to 15.1k measured on the WETH/USDC and WETH/ARB
    /// pools at Arbitrum block 504411371 (3.3M to 6.0M gas for 400 steps).
    /// The cap keeps a walk below 7M gas, inside the 8M gas the simulation
    /// engine grants a call.
    uint256 internal constant _MAX_TICK_WALK_STEPS = 400;
    /// @dev Price fractions are scaled so the squared sqrt price they are
    /// built from stays below 2^148, keeping numerators below 2^168.
    uint256 internal constant _MAX_SQUARED_PRICE_BITS = 148;
    /// @dev Prices of token0 in token1 divide by the squared sqrt price, so
    /// it is shifted by at least this much to keep the numerator below 2^148.
    uint256 internal constant _MIN_INVERSE_SHIFT = 64;
    /// @dev First word of the revert data a quote travels back in, so any
    /// other revert is told apart and re-raised.
    bytes32 internal constant _QUOTE_TAG =
        keccak256("CamelotV3SwapAdapter.Quote");

    IAlgebraFactory public immutable factory;

    error CamelotV3SwapAdapter__QuoteDidNotRevert();
    error CamelotV3SwapAdapter__NotSelf();
    error CamelotV3SwapAdapter__UnknownPool();

    struct CallbackData {
        address token0;
        address token1;
        address payer;
        bool quote;
    }

    struct Quote {
        int256 amount0;
        int256 amount1;
        uint160 sqrtPrice;
        uint16 feeZto;
        uint16 feeOtz;
    }

    constructor(address factory_) {
        factory = IAlgebraFactory(factory_);
    }

    /// @inheritdoc ISwapAdapter
    /// @dev Each price is the marginal price after selling the given amount,
    /// including the fee the pool would charge the next swap. Reverts with
    /// `LimitExceeded` when the pool cannot absorb the whole amount.
    function price(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        uint256[] memory specifiedAmounts
    ) external override returns (Fraction[] memory prices) {
        (IAlgebraPool pool, bool zeroToOne,,) =
            _pool(poolId, sellToken, buyToken);
        prices = new Fraction[](specifiedAmounts.length);
        for (uint256 i = 0; i < specifiedAmounts.length; i++) {
            prices[i] = _priceAfterSell(pool, zeroToOne, specifiedAmounts[i]);
        }
    }

    /// @inheritdoc ISwapAdapter
    /// @dev Output goes to `msg.sender`, input is pulled from `msg.sender` in
    /// the pool's callback. Reverts with `LimitExceeded` when the pool ran out
    /// of liquidity before the specified amount was fully traded.
    function swap(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        OrderSide side,
        uint256 specifiedAmount
    ) external override returns (Trade memory trade) {
        if (specifiedAmount == 0) {
            return trade;
        }
        (IAlgebraPool pool, bool zeroToOne, address token0, address token1) =
            _pool(poolId, sellToken, buyToken);
        int256 amountRequired = side == OrderSide.Sell
            ? SafeCast.toInt256(specifiedAmount)
            : -SafeCast.toInt256(specifiedAmount);
        bytes memory data =
            abi.encode(CallbackData(token0, token1, msg.sender, false));

        uint256 gasBefore = gasleft();
        (int256 amount0, int256 amount1) = pool.swap(
            msg.sender,
            zeroToOne,
            amountRequired,
            _fullRangeLimit(zeroToOne),
            data
        );
        trade.gasUsed = gasBefore - gasleft();

        (uint256 amountIn, uint256 amountOut) = zeroToOne
            ? (uint256(amount0), uint256(-amount1))
            : (uint256(amount1), uint256(-amount0));
        if (side == OrderSide.Sell) {
            if (amountIn < specifiedAmount) {
                revert LimitExceeded(amountIn);
            }
            trade.calculatedAmount = amountOut;
        } else {
            if (amountOut < specifiedAmount) {
                revert LimitExceeded(amountOut);
            }
            trade.calculatedAmount = amountIn;
        }

        (uint160 sqrtPrice,, uint16 feeZto, uint16 feeOtz) = _globalState(pool);
        trade.price =
            _marginalPrice(sqrtPrice, zeroToOne ? feeZto : feeOtz, zeroToOne);
    }

    /// @inheritdoc ISwapAdapter
    /// @dev Walks the pool's tick table read-only, like the native Uniswap V3
    /// implementation in tycho-simulation: sums the token amounts held by the
    /// pool's liquidity between the current price and each next initialized
    /// tick, fees ignored. The sell limit is therefore what the pool absorbs
    /// net of fees, so selling it is always fully executed, and the buy limit
    /// overestimates the output. The walk ends when liquidity runs out, the
    /// tick range ends, or after `_MAX_TICK_WALK_STEPS` steps. Quoting the pool
    /// itself instead costs 8M gas and more on liquid pools, because every
    /// initialized tick a swap crosses writes storage.
    function getLimits(bytes32 poolId, address sellToken, address buyToken)
        external
        view
        override
        returns (uint256[] memory limits)
    {
        (IAlgebraPool pool, bool zeroToOne,,) =
            _pool(poolId, sellToken, buyToken);
        (uint160 sqrtPrice, int24 tick,,) = _globalState(pool);
        (uint256 amountIn, uint256 amountOut) =
            _walkTicks(pool, zeroToOne, sqrtPrice, tick, pool.liquidity());
        limits = new uint256[](2);
        limits[0] = amountIn;
        limits[1] = amountOut;
    }

    /// @inheritdoc ISwapAdapter
    /// @dev Not `HardLimits`: the limits are lower bounds from a capped walk,
    /// and a sell above them still executes while the pool has liquidity.
    /// Only a true shortfall reverts, with `LimitExceeded`.
    function getCapabilities(bytes32, address, address)
        external
        pure
        override
        returns (Capability[] memory capabilities)
    {
        capabilities = new Capability[](4);
        capabilities[0] = Capability.SellOrder;
        capabilities[1] = Capability.BuyOrder;
        capabilities[2] = Capability.PriceFunction;
        capabilities[3] = Capability.MarginalPrice;
    }

    /// @inheritdoc ISwapAdapter
    function getTokens(bytes32 poolId)
        external
        view
        override
        returns (address[] memory tokens)
    {
        IAlgebraPool pool = IAlgebraPool(address(bytes20(poolId)));
        tokens = new address[](2);
        tokens[0] = pool.token0();
        tokens[1] = pool.token1();
    }

    /// @inheritdoc ISwapAdapter
    /// @dev The factory only exposes `poolByPair`; there is no enumeration.
    function getPoolIds(uint256, uint256)
        external
        pure
        override
        returns (bytes32[] memory)
    {
        revert NotImplemented("CamelotV3SwapAdapter.getPoolIds");
    }

    /// @notice Pool callback: pays the swap's input, or reports a quote.
    /// @dev The caller must be the factory's pool for the pair carried in
    /// `data`. In quote mode the callback reverts with the swap's amounts and
    /// the pool's price and fees, which `_quote` decodes.
    function algebraSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external {
        CallbackData memory callback = abi.decode(data, (CallbackData));
        if (msg.sender != factory.poolByPair(callback.token0, callback.token1))
        {
            revert CamelotV3SwapAdapter__UnknownPool();
        }
        if (callback.quote) {
            (uint160 sqrtPrice,, uint16 feeZto, uint16 feeOtz,,,,) =
                IAlgebraPool(msg.sender).globalState();
            bytes memory quote = abi.encode(
                _QUOTE_TAG,
                amount0Delta,
                amount1Delta,
                sqrtPrice,
                feeZto,
                feeOtz
            );
            // slither-disable-next-line assembly
            assembly {
                revert(add(quote, 32), mload(quote))
            }
        }
        if (amount0Delta > 0) {
            IERC20(callback.token0)
                .safeTransferFrom(
                    callback.payer, msg.sender, uint256(amount0Delta)
                );
        } else if (amount1Delta > 0) {
            IERC20(callback.token1)
                .safeTransferFrom(
                    callback.payer, msg.sender, uint256(amount1Delta)
                );
        }
    }

    /// @notice Runs a swap whose callback always reverts, so the pool state is
    /// left untouched and the outcome travels back in the revert data.
    /// @dev Only the adapter itself may call this, through `_quote`.
    function executeQuote(
        IAlgebraPool pool,
        bool zeroToOne,
        int256 amountRequired,
        uint160 limitSqrtPrice
    ) external {
        if (msg.sender != address(this)) {
            revert CamelotV3SwapAdapter__NotSelf();
        }
        pool.swap(
            address(this),
            zeroToOne,
            amountRequired,
            limitSqrtPrice,
            abi.encode(
                CallbackData(pool.token0(), pool.token1(), address(0), true)
            )
        );
        revert CamelotV3SwapAdapter__QuoteDidNotRevert();
    }

    /// @dev Marginal price after selling `amount`. For `amount == 0` the pool
    /// state is unchanged, but the fee the pool charges next is only computed
    /// by the first swap of a block, so a 1 wei swap with a price limit one
    /// unit away is quoted to read that fee.
    function _priceAfterSell(IAlgebraPool pool, bool zeroToOne, uint256 amount)
        internal
        returns (Fraction memory)
    {
        if (amount == 0) {
            (uint160 sqrtPrice,,,) = _globalState(pool);
            uint160 limit = zeroToOne ? sqrtPrice - 1 : sqrtPrice + 1;
            Quote memory current = _quote(pool, zeroToOne, 1, limit);
            return _marginalPrice(
                sqrtPrice,
                zeroToOne ? current.feeZto : current.feeOtz,
                zeroToOne
            );
        }
        Quote memory afterSell = _quote(
            pool,
            zeroToOne,
            SafeCast.toInt256(amount),
            _fullRangeLimit(zeroToOne)
        );
        uint256 consumed =
            uint256(zeroToOne ? afterSell.amount0 : afterSell.amount1);
        if (consumed < amount) {
            revert LimitExceeded(consumed);
        }
        return _marginalPrice(
            afterSell.sqrtPrice,
            zeroToOne ? afterSell.feeZto : afterSell.feeOtz,
            zeroToOne
        );
    }

    /// @dev Runs `executeQuote` and decodes its revert. Any other revert, the
    /// pool's own included, is re-raised unchanged.
    function _quote(
        IAlgebraPool pool,
        bool zeroToOne,
        int256 amountRequired,
        uint160 limitSqrtPrice
    ) internal returns (Quote memory quote) {
        try this.executeQuote(pool, zeroToOne, amountRequired, limitSqrtPrice) {
            revert CamelotV3SwapAdapter__QuoteDidNotRevert();
        } catch (bytes memory reason) {
            if (reason.length != 6 * 32 || bytes32(reason) != _QUOTE_TAG) {
                // slither-disable-next-line assembly
                assembly {
                    revert(add(reason, 32), mload(reason))
                }
            }
            (
                ,
                quote.amount0,
                quote.amount1,
                quote.sqrtPrice,
                quote.feeZto,
                quote.feeOtz
            ) =
                abi.decode(
                    reason, (bytes32, int256, int256, uint160, uint16, uint16)
                );
        }
    }

    /// @dev Marginal price in buy token per sell token at `sqrtPrice`, net of
    /// `fee`. The pool's price of token0 in token1 is `sqrtPrice^2 / 2^192`;
    /// the squared sqrt price is shifted right so that it fits in
    /// `_MAX_SQUARED_PRICE_BITS` bits, and the power of two moves to the other
    /// side of the fraction. Exact whenever no shift is needed.
    function _marginalPrice(uint160 sqrtPrice, uint16 fee, bool zeroToOne)
        internal
        pure
        returns (Fraction memory)
    {
        uint256 squaredBits = 2 * (Math.log2(sqrtPrice) + 1);
        uint256 shift = squaredBits > _MAX_SQUARED_PRICE_BITS
            ? squaredBits - _MAX_SQUARED_PRICE_BITS
            : 0;
        if (!zeroToOne && shift < _MIN_INVERSE_SHIFT) {
            shift = _MIN_INVERSE_SHIFT;
        }
        uint256 squared = Math.mulDiv(sqrtPrice, sqrtPrice, 1 << shift);
        uint256 powerOfTwo = 1 << (192 - shift);
        uint256 feeFactor = _FEE_DENOMINATOR - fee;
        if (zeroToOne) {
            return Fraction(squared * feeFactor, powerOfTwo * _FEE_DENOMINATOR);
        }
        return Fraction(powerOfTwo * feeFactor, squared * _FEE_DENOMINATOR);
    }

    /// @dev Sums the amounts the pool can absorb (`amountIn`) and pay out
    /// (`amountOut`) while its price moves from `sqrtPrice` in the direction
    /// of the swap, one tick-table step at a time. Liquidity is updated with
    /// each initialized tick's delta; the walk stops before a step that would
    /// make it negative. The pool's swap can never move past
    /// `MIN_SQRT_RATIO + 1` or `MAX_SQRT_RATIO - 1`, so the range ends there.
    function _walkTicks(
        IAlgebraPool pool,
        bool zeroToOne,
        uint160 sqrtPrice,
        int24 tick,
        uint128 liquidity
    ) internal view returns (uint256 amountIn, uint256 amountOut) {
        for (uint256 step = 0; step < _MAX_TICK_WALK_STEPS; step++) {
            (int24 nextTick, bool initialized) =
                CamelotV3TickMath.nextTickInTheSameRow(pool, tick, zeroToOne);
            uint160 nextSqrtPrice = _reachableSqrtPrice(nextTick);

            if (zeroToOne) {
                amountIn += CamelotV3TickMath.getToken0Delta(
                    nextSqrtPrice, sqrtPrice, liquidity, true
                );
                amountOut += CamelotV3TickMath.getToken1Delta(
                    nextSqrtPrice, sqrtPrice, liquidity, false
                );
            } else {
                amountIn += CamelotV3TickMath.getToken1Delta(
                    sqrtPrice, nextSqrtPrice, liquidity, true
                );
                amountOut += CamelotV3TickMath.getToken0Delta(
                    sqrtPrice, nextSqrtPrice, liquidity, false
                );
            }

            if (initialized) {
                (, int128 liquidityDelta,,,,,,) = pool.ticks(nextTick);
                int256 delta = zeroToOne
                    ? -int256(liquidityDelta)
                    : int256(liquidityDelta);
                if (delta < 0) {
                    uint256 removed = uint256(-delta);
                    if (removed > liquidity) break;
                    liquidity -= uint128(removed);
                } else {
                    uint256 added = uint256(delta) + liquidity;
                    if (added > type(uint128).max) break;
                    liquidity = uint128(added);
                }
            }

            if (
                nextTick == CamelotV3TickMath.MIN_TICK
                    || nextTick == CamelotV3TickMath.MAX_TICK
            ) {
                break;
            }
            tick = zeroToOne ? nextTick - 1 : nextTick;
            sqrtPrice = nextSqrtPrice;
        }
    }

    /// @dev The sqrt price at `tick`, clamped to the range a swap can reach.
    function _reachableSqrtPrice(int24 tick) internal pure returns (uint160) {
        uint160 sqrtPrice = CamelotV3TickMath.getSqrtRatioAtTick(tick);
        if (sqrtPrice <= _MIN_SQRT_RATIO) return _MIN_SQRT_RATIO + 1;
        if (sqrtPrice >= _MAX_SQRT_RATIO) return _MAX_SQRT_RATIO - 1;
        return sqrtPrice;
    }

    function _fullRangeLimit(bool zeroToOne) internal pure returns (uint160) {
        return zeroToOne ? _MIN_SQRT_RATIO + 1 : _MAX_SQRT_RATIO - 1;
    }

    /// @dev The pool behind `poolId`, its tokens, and whether the order sells
    /// token0 for token1.
    function _pool(bytes32 poolId, address sellToken, address buyToken)
        internal
        view
        returns (
            IAlgebraPool pool,
            bool zeroToOne,
            address token0,
            address token1
        )
    {
        pool = IAlgebraPool(address(bytes20(poolId)));
        token0 = pool.token0();
        token1 = pool.token1();
        if (sellToken == token0 && buyToken == token1) {
            return (pool, true, token0, token1);
        }
        if (sellToken == token1 && buyToken == token0) {
            return (pool, false, token0, token1);
        }
        revert InvalidOrder("tokens are not the pool's pair");
    }

    /// @dev The pool's price, tick and directional fees. A pool that was
    /// created but never initialized still has a zero price and cannot be
    /// quoted or swapped.
    function _globalState(IAlgebraPool pool)
        internal
        view
        returns (uint160 sqrtPrice, int24 tick, uint16 feeZto, uint16 feeOtz)
    {
        (sqrtPrice, tick, feeZto, feeOtz,,,,) = pool.globalState();
        if (sqrtPrice == 0) {
            revert Unavailable("pool is not initialized");
        }
    }
}
