// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import {BalanceLens} from "./BalanceLens.sol";

/// @title BalanceOwnerBalances
/// @notice For a component whose tokens sit at the address its
/// `balance_owner` attribute names, and only its own: each token's balance
/// of that address. A contract that holds several components' tokens, such
/// as a singleton vault, needs a lens that reads the component's share.
/// Reverts with `MissingAttribute` without a `balance_owner` attribute, and
/// with `NotAnAddress` when its value is not an address.
contract BalanceOwnerBalances is BalanceLens {
    function balances(
        bytes calldata,
        address[] calldata tokens,
        Attribute[] calldata attributes
    ) external view returns (uint256[] memory amounts) {
        return
            balancesOf(
                tokens, toAddress(attribute(attributes, "balance_owner"))
            );
    }
}
