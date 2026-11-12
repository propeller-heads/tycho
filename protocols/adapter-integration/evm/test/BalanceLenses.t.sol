// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import "forge-std/Test.sol";
import {IBalanceLens} from "src/balance-lenses/IBalanceLens.sol";
import {BalanceLens} from "src/balance-lenses/BalanceLens.sol";
import {ComponentIdBalances} from "src/balance-lenses/ComponentIdBalances.sol";
import {
    BalanceOwnerBalances
} from "src/balance-lenses/BalanceOwnerBalances.sol";

contract FakeToken {
    mapping(address => uint256) public balanceOf;

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
    }
}

contract BalanceLensesTest is Test {
    FakeToken token;
    address pool = address(0xBEEF);
    address vault = address(0xCAFE);

    function setUp() public {
        token = new FakeToken();
        token.mint(pool, 100);
        token.mint(vault, 300);
        vm.deal(pool, 7);
    }

    function tokens() internal view returns (address[] memory list) {
        list = new address[](2);
        list[0] = address(token);
        list[1] = address(0);
    }

    function testComponentIdBalancesReadsTheComponentId() public {
        IBalanceLens.Attribute[] memory none;
        uint256[] memory amounts = new ComponentIdBalances()
            .balances(abi.encodePacked(pool), tokens(), none);
        assertEq(amounts.length, 2);
        assertEq(amounts[0], 100);
        assertEq(amounts[1], 7);
    }

    function testComponentIdBalancesRejectsAnIdThatIsNotAnAddress() public {
        IBalanceLens.Attribute[] memory none;
        ComponentIdBalances lens = new ComponentIdBalances();
        bytes memory poolId = abi.encodePacked(bytes32(type(uint256).max));
        vm.expectRevert(
            abi.encodeWithSelector(BalanceLens.NotAnAddress.selector, poolId)
        );
        lens.balances(poolId, tokens(), none);
    }

    function testBalanceOwnerBalancesReadsTheCurrentAttribute() public {
        IBalanceLens.Attribute[] memory attributes =
            new IBalanceLens.Attribute[](2);
        // The static attribute, then the state one, which wins.
        attributes[0] =
            IBalanceLens.Attribute("balance_owner", abi.encodePacked(pool));
        attributes[1] =
            IBalanceLens.Attribute("balance_owner", abi.encode(vault));
        uint256[] memory amounts = new BalanceOwnerBalances()
            .balances(abi.encodePacked(pool), tokens(), attributes);
        assertEq(amounts[0], 300);
        assertEq(amounts[1], 0);
    }

    function testBalanceOwnerBalancesNeedsTheAttribute() public {
        IBalanceLens.Attribute[] memory none;
        BalanceOwnerBalances lens = new BalanceOwnerBalances();
        vm.expectRevert(
            abi.encodeWithSelector(
                BalanceLens.MissingAttribute.selector, "balance_owner"
            )
        );
        lens.balances(abi.encodePacked(pool), tokens(), none);
    }
}
