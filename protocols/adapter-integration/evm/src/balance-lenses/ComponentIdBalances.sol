// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import {BalanceLens} from "./BalanceLens.sol";

/// @title ComponentIdBalances
/// @notice For a component whose id is the address holding its tokens, as a
/// pair contract: each token's balance of the component id. Reverts with
/// `NotAnAddress` when the id is not an address, as a 32-byte pool id is not.
contract ComponentIdBalances is BalanceLens {
    function balances(
        bytes calldata id,
        address[] calldata tokens,
        Attribute[] calldata
    ) external view returns (uint256[] memory amounts) {
        return balancesOf(tokens, toAddress(id));
    }
}
