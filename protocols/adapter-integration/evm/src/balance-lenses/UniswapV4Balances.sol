// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import {BalanceLens} from "./BalanceLens.sol";
import {IPoolManager} from "v4-core/src/interfaces/IPoolManager.sol";
import {BitMath} from "v4-core/src/libraries/BitMath.sol";
import {SqrtPriceMath} from "v4-core/src/libraries/SqrtPriceMath.sol";
import {StateLibrary} from "v4-core/src/libraries/StateLibrary.sol";
import {TickBitmap} from "v4-core/src/libraries/TickBitmap.sol";
import {TickMath} from "v4-core/src/libraries/TickMath.sol";
import {PoolId} from "v4-core/src/types/PoolId.sol";

/// @title UniswapV4Balances
/// @notice For a Uniswap V4 pool: the amount of each currency its
/// liquidity holds at the pool's current price.
/// @notice The `PoolManager` keeps every pool's tokens in one balance and
/// accounts no per-pool amount. The lens derives the amounts from the
/// pool's own accounting. It walks the pool's tick bitmap from the
/// lowest to the highest usable tick. For each range between two
/// initialized ticks it adds what the range's liquidity holds at the
/// current price (`SqrtPriceMath` amounts, rounded down).
/// @notice LP fees owed to positions, protocol fees and the ERC-6909 claims
/// of hooks sit in the same `PoolManager` balance. They are not liquidity,
/// so the lens does not count them.
/// @dev Reads the `PoolManager` named by the component's `balance_owner`
/// attribute through v4-core's `StateLibrary`. Expects the id to be the
/// 32-byte pool id, the `tick_spacing` attribute as a big-endian integer,
/// and the tokens to be the pool's two currencies.
/// @dev Cost: one `extsload` call per bitmap word over the full tick range,
/// each reading a cold slot of the `PoolManager`; the word count grows as
/// the tick spacing shrinks. At tick spacing 1 a call costs tens of millions
/// of gas (`testGasOfAnEmptyPoolAtTickSpacing1` measures it), so an
/// `eth_call` for such a pool needs a node gas cap above that.
contract UniswapV4Balances is BalanceLens {
    using StateLibrary for IPoolManager;

    /// @notice The id is not a 32-byte pool id.
    error UniswapV4Balances__NotAPoolId(bytes id);
    /// @notice The component does not have exactly two tokens.
    error UniswapV4Balances__NotTwoTokens(uint256 count);
    /// @notice `tick_spacing` is outside the bounds `PoolManager.initialize`
    /// accepts (`TickMath.MIN_TICK_SPACING` to `MAX_TICK_SPACING`).
    error UniswapV4Balances__InvalidTickSpacing(uint256 tickSpacing);
    /// @notice The pool's sqrt price is zero: the `PoolManager` holds no
    /// initialized pool under this id.
    error UniswapV4Balances__PoolNotInitialized(bytes32 id);
    /// @notice A tick's net liquidity took the running liquidity below
    /// zero, so the walk did not visit the ticks the pool holds.
    error UniswapV4Balances__NegativeLiquidity(int24 tick);
    /// @notice The walk's liquidity at the current tick differs from the
    /// pool's `liquidity`, so the walk did not visit the ticks the pool
    /// holds.
    error UniswapV4Balances__LiquidityMismatch(uint256 walked, uint256 pool);
    /// @notice The `tick_spacing` attribute is longer than 32 bytes.
    error UniswapV4Balances__NotAUint(bytes value);

    /// @notice The pool the walk reads, with its `slot0` price and tick.
    struct PoolView {
        IPoolManager manager;
        PoolId id;
        int24 tickSpacing;
        uint160 sqrtPriceX96;
        int24 tick;
    }

    /// @notice What the walk adds up.
    struct Totals {
        uint256 amount0;
        uint256 amount1;
        // Liquidity of the range the walk is in.
        int256 liquidity;
        // Liquidity of the range holding the current tick.
        int256 liquidityAtTick;
    }

    function balances(
        bytes calldata id,
        address[] calldata tokens,
        Attribute[] calldata attributes
    ) external view returns (uint256[] memory amounts) {
        if (id.length != 32) {
            revert UniswapV4Balances__NotAPoolId(id);
        }
        if (tokens.length != 2) {
            revert UniswapV4Balances__NotTwoTokens(tokens.length);
        }
        uint256 tickSpacing = toUint(attribute(attributes, "tick_spacing"));
        if (
            tickSpacing < uint24(TickMath.MIN_TICK_SPACING)
                || tickSpacing > uint24(TickMath.MAX_TICK_SPACING)
        ) {
            revert UniswapV4Balances__InvalidTickSpacing(tickSpacing);
        }

        PoolView memory pool;
        pool.manager =
            IPoolManager(toAddress(attribute(attributes, "balance_owner")));
        pool.id = PoolId.wrap(bytes32(id));
        pool.tickSpacing = int24(int256(tickSpacing));
        (pool.sqrtPriceX96, pool.tick,,) = pool.manager.getSlot0(pool.id);
        if (pool.sqrtPriceX96 == 0) {
            revert UniswapV4Balances__PoolNotInitialized(bytes32(id));
        }

        Totals memory totals = walk(pool);

        uint256 poolLiquidity = pool.manager.getLiquidity(pool.id);
        if (uint256(totals.liquidityAtTick) != poolLiquidity) {
            revert UniswapV4Balances__LiquidityMismatch(
                uint256(totals.liquidityAtTick), poolLiquidity
            );
        }

        // v4 orders a key's currencies by address: currency0 is the lower.
        bool firstIsCurrency0 = tokens[0] < tokens[1];
        amounts = new uint256[](2);
        amounts[0] = firstIsCurrency0 ? totals.amount0 : totals.amount1;
        amounts[1] = firstIsCurrency0 ? totals.amount1 : totals.amount0;
    }

    /// @notice The amounts of every range between initialized ticks, from
    /// the lowest initialized tick up; below it and above the highest the
    /// liquidity is zero.
    function walk(PoolView memory pool)
        internal
        view
        returns (Totals memory totals)
    {
        (int16 lowestWord,) = TickBitmap.position(
            TickBitmap.compress(TickMath.MIN_TICK, pool.tickSpacing)
        );
        (int16 highestWord,) = TickBitmap.position(
            TickBitmap.compress(TickMath.MAX_TICK, pool.tickSpacing)
        );
        bool started;
        int24 previousTick;
        for (int16 word = lowestWord; word <= highestWord; word++) {
            uint256 bits = pool.manager.getTickBitmap(pool.id, word);
            while (bits != 0) {
                uint8 bit = BitMath.leastSignificantBit(bits);
                bits &= bits - 1;
                // The inverse of `TickBitmap.position(compress(tick))`.
                int24 tick =
                    (int24(word) * 256 + int24(uint24(bit))) * pool.tickSpacing;
                if (started) {
                    addRange(pool, totals, previousTick, tick);
                }
                (, int128 net) = pool.manager.getTickLiquidity(pool.id, tick);
                totals.liquidity += net;
                if (totals.liquidity < 0) {
                    revert UniswapV4Balances__NegativeLiquidity(tick);
                }
                started = true;
                previousTick = tick;
            }
        }
    }

    /// @notice Adds what `totals.liquidity` holds over [lower, upper) at
    /// the pool's price, and records it as the liquidity at the current
    /// tick when the range holds that tick.
    function addRange(
        PoolView memory pool,
        Totals memory totals,
        int24 lower,
        int24 upper
    ) internal pure {
        if (pool.tick >= lower && pool.tick < upper) {
            totals.liquidityAtTick = totals.liquidity;
        }
        if (totals.liquidity == 0) {
            return;
        }
        uint128 liquidity = uint128(uint256(totals.liquidity));
        uint160 sqrtLower = TickMath.getSqrtPriceAtTick(lower);
        uint160 sqrtUpper = TickMath.getSqrtPriceAtTick(upper);
        uint160 price = pool.sqrtPriceX96;
        if (price <= sqrtLower) {
            totals.amount0 += SqrtPriceMath.getAmount0Delta(
                sqrtLower, sqrtUpper, liquidity, false
            );
        } else if (price >= sqrtUpper) {
            totals.amount1 += SqrtPriceMath.getAmount1Delta(
                sqrtLower, sqrtUpper, liquidity, false
            );
        } else {
            totals.amount0 += SqrtPriceMath.getAmount0Delta(
                price, sqrtUpper, liquidity, false
            );
            totals.amount1 += SqrtPriceMath.getAmount1Delta(
                sqrtLower, price, liquidity, false
            );
        }
    }

    /// @notice `value` as a big-endian uint256 of at most 32 bytes.
    function toUint(bytes calldata value) internal pure returns (uint256 n) {
        if (value.length > 32) {
            revert UniswapV4Balances__NotAUint(value);
        }
        for (uint256 i = 0; i < value.length; i++) {
            n = (n << 8) | uint8(value[i]);
        }
    }
}
