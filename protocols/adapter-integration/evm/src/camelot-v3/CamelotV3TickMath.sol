// SPDX-License-Identifier: GPL-2.0-or-later
pragma solidity ^0.8.13;

import {Math} from "openzeppelin-contracts/contracts/utils/math/Math.sol";
import {IAlgebraPool} from "src/camelot-v3/IAlgebraPool.sol";

/// @title CamelotV3TickMath
/// @notice Read-only ports of the tick math a Camelot V3 (Algebra V1.9) pool
/// uses, taken from the verified pool source on Arbitrum One: `TickMath`,
/// `TokenDeltaMath` and `TickTable` (credit to Uniswap Labs under
/// GPL-2.0-or-later for the first two). They let the adapter walk a pool's
/// liquidity without executing a swap, which would write storage at every
/// initialized tick it crosses.
library CamelotV3TickMath {
    int24 internal constant MIN_TICK = -887272;
    int24 internal constant MAX_TICK = 887272;
    /// @dev getSqrtRatioAtTick(MIN_TICK) and getSqrtRatioAtTick(MAX_TICK).
    uint160 internal constant MIN_SQRT_RATIO = 4295128739;
    uint160 internal constant MAX_SQRT_RATIO =
        1461446703485210103287273052203988822378723970342;
    uint256 internal constant Q96 = 0x1000000000000000000000000;

    /// @notice sqrt(1.0001^tick) * 2^96, the pool's Q64.96 sqrt price at
    /// `tick`. @dev Verbatim constants of the pool's
    /// `TickMath.getSqrtRatioAtTick`;
    /// every product is below 2^256 because `ratio` never exceeds 2^128.
    function getSqrtRatioAtTick(int24 tick)
        internal
        pure
        returns (uint160 price)
    {
        uint256 absTick =
            tick < 0 ? uint256(-int256(tick)) : uint256(int256(tick));
        require(absTick <= uint256(int256(MAX_TICK)), "T");

        uint256 ratio = absTick & 0x1 != 0
            ? 0xfffcb933bd6fad37aa2d162d1a594001
            : 0x100000000000000000000000000000000;
        if (absTick & 0x2 != 0) {
            ratio = (ratio * 0xfff97272373d413259a46990580e213a) >> 128;
        }
        if (absTick & 0x4 != 0) {
            ratio = (ratio * 0xfff2e50f5f656932ef12357cf3c7fdcc) >> 128;
        }
        if (absTick & 0x8 != 0) {
            ratio = (ratio * 0xffe5caca7e10e4e61c3624eaa0941cd0) >> 128;
        }
        if (absTick & 0x10 != 0) {
            ratio = (ratio * 0xffcb9843d60f6159c9db58835c926644) >> 128;
        }
        if (absTick & 0x20 != 0) {
            ratio = (ratio * 0xff973b41fa98c081472e6896dfb254c0) >> 128;
        }
        if (absTick & 0x40 != 0) {
            ratio = (ratio * 0xff2ea16466c96a3843ec78b326b52861) >> 128;
        }
        if (absTick & 0x80 != 0) {
            ratio = (ratio * 0xfe5dee046a99a2a811c461f1969c3053) >> 128;
        }
        if (absTick & 0x100 != 0) {
            ratio = (ratio * 0xfcbe86c7900a88aedcffc83b479aa3a4) >> 128;
        }
        if (absTick & 0x200 != 0) {
            ratio = (ratio * 0xf987a7253ac413176f2b074cf7815e54) >> 128;
        }
        if (absTick & 0x400 != 0) {
            ratio = (ratio * 0xf3392b0822b70005940c7a398e4b70f3) >> 128;
        }
        if (absTick & 0x800 != 0) {
            ratio = (ratio * 0xe7159475a2c29b7443b29c7fa6e889d9) >> 128;
        }
        if (absTick & 0x1000 != 0) {
            ratio = (ratio * 0xd097f3bdfd2022b8845ad8f792aa5825) >> 128;
        }
        if (absTick & 0x2000 != 0) {
            ratio = (ratio * 0xa9f746462d870fdf8a65dc1f90e061e5) >> 128;
        }
        if (absTick & 0x4000 != 0) {
            ratio = (ratio * 0x70d869a156d2a1b890bb3df62baf32f7) >> 128;
        }
        if (absTick & 0x8000 != 0) {
            ratio = (ratio * 0x31be135f97d08fd981231505542fcfa6) >> 128;
        }
        if (absTick & 0x10000 != 0) {
            ratio = (ratio * 0x9aa508b5b7a84e1c677de54f3e99bc9) >> 128;
        }
        if (absTick & 0x20000 != 0) {
            ratio = (ratio * 0x5d6af8dedb81196699c329225ee604) >> 128;
        }
        if (absTick & 0x40000 != 0) {
            ratio = (ratio * 0x2216e584f5fa1ea926041bedfe98) >> 128;
        }
        if (absTick & 0x80000 != 0) {
            ratio = (ratio * 0x48a170391f7dc42444e8fa2) >> 128;
        }

        if (tick > 0) ratio = type(uint256).max / ratio;

        // Q128.128 to Q128.96, rounding up; the tick bound keeps it in 160
        // bits.
        price = uint160((ratio >> 32) + (ratio % (1 << 32) == 0 ? 0 : 1));
    }

    /// @notice Amount of token0 held by `liquidity` between two sqrt prices:
    /// liquidity / sqrt(lower) - liquidity / sqrt(upper). The prices may be
    /// given in either order.
    function getToken0Delta(
        uint160 sqrtPriceA,
        uint160 sqrtPriceB,
        uint128 liquidity,
        bool roundUp
    ) internal pure returns (uint256) {
        (uint160 lower, uint160 upper) = sqrtPriceA < sqrtPriceB
            ? (sqrtPriceA, sqrtPriceB)
            : (sqrtPriceB, sqrtPriceA);
        require(lower > 0, "sqrt price is zero");
        uint256 priceDelta = upper - lower;
        uint256 liquidityShifted = uint256(liquidity) << 96;
        if (roundUp) {
            return Math.ceilDiv(
                Math.mulDiv(
                    priceDelta, liquidityShifted, upper, Math.Rounding.Ceil
                ),
                lower
            );
        }
        return Math.mulDiv(priceDelta, liquidityShifted, upper) / lower;
    }

    /// @notice Amount of token1 held by `liquidity` between two sqrt prices:
    /// liquidity * (sqrt(upper) - sqrt(lower)). The prices may be given in
    /// either order.
    function getToken1Delta(
        uint160 sqrtPriceA,
        uint160 sqrtPriceB,
        uint128 liquidity,
        bool roundUp
    ) internal pure returns (uint256) {
        uint256 priceDelta = sqrtPriceA < sqrtPriceB
            ? sqrtPriceB - sqrtPriceA
            : sqrtPriceA - sqrtPriceB;
        if (roundUp) {
            return Math.mulDiv(priceDelta, liquidity, Q96, Math.Rounding.Ceil);
        }
        return Math.mulDiv(priceDelta, liquidity, Q96);
    }

    /// @notice The pool's `TickTable.nextTickInTheSameRow`, reading the
    /// table through the pool's getter: the next initialized tick within the
    /// same 256-tick word, or the word's last tick when none is initialized.
    /// @param lte Search at or below `tick` (true) or strictly above it.
    function nextTickInTheSameRow(IAlgebraPool pool, int24 tick, bool lte)
        internal
        view
        returns (int24 nextTick, bool initialized)
    {
        if (lte) {
            (int16 row, uint8 bit) = _position(tick);
            // every set bit at or to the right of `bit`
            uint256 word = pool.tickTable(row) << (255 - bit);
            if (word != 0) {
                uint256 msb = Math.log2(word);
                return (_boundTick(tick - int24(uint24(255 - msb))), true);
            }
            return (_boundTick(tick - int24(uint24(bit))), false);
        }
        // the current tick's own state does not matter when going up
        tick += 1;
        (int16 rowAbove, uint8 bitAbove) = _position(tick);
        // every set bit at or to the left of `bitAbove`
        uint256 wordAbove = pool.tickTable(rowAbove) >> bitAbove;
        if (wordAbove != 0) {
            uint256 lsb = Math.log2(wordAbove & (~wordAbove + 1));
            return (_boundTick(tick + int24(uint24(lsb))), true);
        }
        return (_boundTick(tick + int24(uint24(255 - bitAbove))), false);
    }

    function _position(int24 tick) private pure returns (int16 row, uint8 bit) {
        row = int16(tick >> 8);
        bit = uint8(uint24(tick) & 0xff);
    }

    function _boundTick(int24 tick) private pure returns (int24) {
        if (tick < MIN_TICK) return MIN_TICK;
        if (tick > MAX_TICK) return MAX_TICK;
        return tick;
    }
}
