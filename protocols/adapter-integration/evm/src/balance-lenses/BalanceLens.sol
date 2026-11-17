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

    /// @notice `balanceOf(token, holder)` for each of `tokens`, in order.
    function balancesOf(address[] calldata tokens, address holder)
        internal
        view
        returns (uint256[] memory amounts)
    {
        amounts = new uint256[](tokens.length);
        for (uint256 i = 0; i < tokens.length; i++) {
            amounts[i] = balanceOf(tokens[i], holder);
        }
    }

    /// @notice The current value of attribute `name`: the last entry of that
    /// name, so a state attribute wins over a static one. Reverts with
    /// `MissingAttribute` when no entry has that name.
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
    /// address whose high 12 bytes are zero. Reverts with `NotAnAddress`
    /// otherwise.
    function toAddress(bytes calldata value) internal pure returns (address) {
        if (value.length == 20) {
            return address(bytes20(value));
        }
        if (value.length == 32 && bytes12(value[:12]) == bytes12(0)) {
            return address(bytes20(value[12:]));
        }
        revert NotAnAddress(value);
    }
}
