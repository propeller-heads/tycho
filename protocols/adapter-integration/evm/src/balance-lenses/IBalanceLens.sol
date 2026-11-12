// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

/// @title IBalanceLens
/// @notice Reads, at the snapshot block, the on-chain amount of each of a
/// component's tokens that its indexed balance must equal. A package names
/// its lens with `balance_lens` in its integration test yaml, and the test
/// runner calls it once per expected component, with the lens's code
/// placed by a state override.
interface IBalanceLens {
    /// @notice One attribute of a component, as Tycho indexes it.
    struct Attribute {
        string name;
        bytes value;
    }

    /// @notice The amount of each token the component holds or accounts for.
    /// @param id The component id: its bytes when it is hex, as an address
    /// or a pool id, else its UTF-8 bytes.
    /// @param tokens The component's tokens; the zero address is the chain's
    /// native token.
    /// @param attributes The component's static attributes, then its state
    /// attributes at the snapshot block, so a later entry of the same name
    /// is the current one.
    /// @return amounts One amount per token, in the order of `tokens`. A
    /// token the protocol does not account for per component is 0, and the
    /// indexer must then report no balance for it.
    function balances(
        bytes calldata id,
        address[] calldata tokens,
        Attribute[] calldata attributes
    ) external view returns (uint256[] memory amounts);
}
