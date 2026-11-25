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

/// @notice The Curve pool type byte the encoder emits. Stable (1) and stable_ng (10) pools take
/// the `int128` index signatures; crypto and llamma pools take `uint256`.
function isCurveStablePool(uint8 poolType) pure returns (bool) {
    return poolType == 1 || poolType == 10;
}

/// @notice Twocrypto pools with a policy slot (Twocrypto v3).
interface ITwocryptoPolicyPool {
    // slither-disable-next-line naming-convention
    function POLICY() external view returns (address);
}

/// @notice The Twocrypto policy that charges a swap the fee of a grant signed
/// by Fynd's controller key. Only the router may submit a grant, in the same
/// transaction as the swap.
interface ICurveFyndPolicy {
    // forge-lint: disable-next-item(mixed-case-variable) Field names are the
    // Vyper struct's, which define the EIP-712 type hash.
    struct Grant {
        address taker;
        uint8 coin_in;
        uint256 min_amount_in;
        uint256 max_amount_in;
        uint256 fee;
        uint64 nonce;
        uint32 deadline;
    }

    // forge-lint: disable-next-line(mixed-case-function) Vyper function name.
    // slither-disable-next-line naming-convention
    function submit_grant(
        Grant calldata grant,
        bytes calldata signature,
        address taker
    ) external;
}
