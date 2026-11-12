// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import {IBalanceLens} from "./IBalanceLens.sol";

interface IERC20Balance {
    function balanceOf(address account) external view returns (uint256);
}

/// @title BalanceLens
/// @notice Helpers for a balance lens: token balances, the native one
/// included, and decoding Tycho's attribute bytes.
abstract contract BalanceLens is IBalanceLens {
    /// @notice The component has no attribute of this name.
    error MissingAttribute(string name);
    /// @notice The value is neither 20 bytes nor a 32-byte word holding an
    /// address.
    error NotAnAddress(bytes value);
    /// @notice The value is longer than a uint256.
    error NotAUint(bytes value);

    /// @notice `token.balanceOf(holder)`, or the native balance of `holder`
    /// for the zero address.
    function balanceOf(address token, address holder)
        internal
        view
        returns (uint256)
    {
        if (token == address(0)) {
            return holder.balance;
        }
        return IERC20Balance(token).balanceOf(holder);
    }

    /// @notice The current value of attribute `name`: the last entry of that
    /// name, so a state attribute wins over a static one.
    function attribute(Attribute[] calldata attributes, string memory name)
        internal
        pure
        returns (bytes calldata)
    {
        bytes32 wanted = keccak256(bytes(name));
        for (uint256 i = attributes.length; i > 0; i--) {
            if (keccak256(bytes(attributes[i - 1].name)) == wanted) {
                return attributes[i - 1].value;
            }
        }
        revert MissingAttribute(name);
    }

    /// @notice `value` as an address: 20 bytes, or a 32-byte ABI-encoded
    /// address whose high 12 bytes are zero.
    function toAddress(bytes calldata value) internal pure returns (address) {
        if (value.length == 20) {
            return address(bytes20(value));
        }
        if (value.length == 32 && bytes12(value[:12]) == bytes12(0)) {
            return address(bytes20(value[12:]));
        }
        revert NotAnAddress(value);
    }

    /// @notice `value` as a big-endian uint256 of at most 32 bytes.
    function toUint(bytes calldata value) internal pure returns (uint256 n) {
        if (value.length > 32) {
            revert NotAUint(value);
        }
        for (uint256 i = 0; i < value.length; i++) {
            n = (n << 8) | uint8(value[i]);
        }
    }
}
