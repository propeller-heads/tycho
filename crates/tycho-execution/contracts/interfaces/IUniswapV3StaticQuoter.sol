// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

/// @title IUniswapV3StaticQuoter
/// @notice A `view` quoter for Uniswap V3 pools: returns what `swap` would, without running it.
/// @dev Source: https://github.com/eden-network/uniswap-v3-static-quoter. Deployments are in
/// `config/protocol_specific_addresses.json` under `fallback_router`.
interface IUniswapV3StaticQuoter {
    /// @notice Quotes `amountSpecified` on `pool` with the same arguments `IUniswapV3Pool.swap`
    /// takes.
    /// @param pool The Uniswap V3 pool.
    /// @param zeroForOne The swap direction.
    /// @param amountSpecified Positive for exact input, negative for exact output.
    /// @param sqrtPriceLimitX96 The price limit, exclusive; must lie between the current price
    /// and the tick bounds.
    /// @return amount0 The pool's token0 delta, negative when the pool pays it out.
    /// @return amount1 The pool's token1 delta, negative when the pool pays it out.
    function quote(
        address pool,
        bool zeroForOne,
        int256 amountSpecified,
        uint160 sqrtPriceLimitX96
    ) external view returns (int256 amount0, int256 amount1);
}
