// SPDX-License-Identifier: BUSL-1.1
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

/// @notice The Curve pool type byte the encoder emits. Stable (1) and stable_ng (10) pools take
/// the `int128` index signatures; crypto and llamma pools take `uint256`.
function isCurveStablePool(uint8 poolType) pure returns (bool) {
    return poolType == 1 || poolType == 10;
}
