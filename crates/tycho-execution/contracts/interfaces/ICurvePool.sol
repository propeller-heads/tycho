// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

/// @notice Curve crypto and llamma pools: `uint256` coin indices.
interface ICurveCryptoPool {
    function exchange(uint256 i, uint256 j, uint256 dx, uint256 minDy)
        external
        payable;

    // slither-disable-next-line naming-convention
    function get_dy(uint256 i, uint256 j, uint256 dx)
        external
        view
        returns (uint256 dy);
}

/// @notice Curve crypto pools that take or pay native ETH.
interface ICurveCryptoPoolETH {
    function exchange(
        uint256 i,
        uint256 j,
        uint256 dx,
        uint256 minDy,
        bool useEth
    ) external payable;
}

/// @notice Curve stable and stable_ng pools: `int128` coin indices.
interface ICurveStablePool {
    function exchange(int128 i, int128 j, uint256 dx, uint256 minDy)
        external
        payable;

    // slither-disable-next-line naming-convention
    function get_dy(int128 i, int128 j, uint256 dx)
        external
        view
        returns (uint256 dy);
}

/// @notice Curve stable metapools: swaps between the pool's coin and the coins of its base pool.
/// Index 0 is the metapool's coin; the base pool's coins follow in the base pool's order.
interface ICurveStableMetaPool {
    // slither-disable-next-line naming-convention
    function exchange_underlying(
        int128 i,
        int128 j,
        uint256 dx,
        uint256 minDy
    ) external;
}

// The pool type byte the encoder emits for a stable metapool's underlying swap.
uint8 constant CURVE_STABLE_UNDERLYING_POOL_TYPE = 11;

/// @notice Whether `poolType` asks for `exchange_underlying` on a stable metapool.
function isCurveStableUnderlying(uint8 poolType) pure returns (bool) {
    return poolType == CURVE_STABLE_UNDERLYING_POOL_TYPE;
}

/// @notice The Curve pool type byte the encoder emits. Stable (1) and stable_ng (10) pools take
/// the `int128` index signatures; crypto and llamma pools take `uint256`.
function isCurveStablePool(uint8 poolType) pure returns (bool) {
    return poolType == 1 || poolType == 10;
}
