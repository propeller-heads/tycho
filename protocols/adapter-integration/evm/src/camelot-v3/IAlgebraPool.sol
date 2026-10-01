// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

/// @dev The part of the Camelot V3 (Algebra V1.9) pool interface the adapter
/// uses, taken from the verified pool ABI on Arbitrum One.
interface IAlgebraPool {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function liquidity() external view returns (uint128);
    function globalState()
        external
        view
        returns (
            uint160 price,
            int24 tick,
            uint16 feeZto,
            uint16 feeOtz,
            uint16 timepointIndex,
            uint8 communityFeeToken0,
            uint8 communityFeeToken1,
            bool unlocked
        );
    /// @dev One 256-tick word of the initialized-tick bitmap.
    function tickTable(int16 row) external view returns (uint256);
    function ticks(int24 tick)
        external
        view
        returns (
            uint128 liquidityTotal,
            int128 liquidityDelta,
            uint256 outerFeeGrowth0Token,
            uint256 outerFeeGrowth1Token,
            int56 outerTickCumulative,
            uint160 outerSecondsPerLiquidity,
            uint32 outerSecondsSpent,
            bool initialized
        );
    function swap(
        address recipient,
        bool zeroToOne,
        int256 amountRequired,
        uint160 limitSqrtPrice,
        bytes calldata data
    ) external returns (int256 amount0, int256 amount1);
}

/// @dev The part of the Camelot V3 factory interface the adapter uses.
interface IAlgebraFactory {
    function poolByPair(address tokenA, address tokenB)
        external
        view
        returns (address pool);
}
