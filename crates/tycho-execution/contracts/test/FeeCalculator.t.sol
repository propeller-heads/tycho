pragma solidity ^0.8.26;

import "@src/FeeCalculator.sol";
import {
    IAccessControl
} from "@openzeppelin/contracts/access/IAccessControl.sol";
import {FeeRecipient} from "../lib/FeeStructs.sol";
import "./Constants.sol";

contract FeeCalculatorTest is Constants {
    FeeCalculator feeCalculator;

    function setUp() public {
        feeCalculator = new FeeCalculator(FEE_SETTER);
    }

    function testCalculateOnlyRouterFeeOnOutput() public {
        // Set router fee on output and receiver
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(ALICE);
        feeCalculator.setRouterFeeOnOutput(100); // 1%
        vm.stopPrank();

        uint256 amountIn = 1 ether;

        // The client is BOB - he doesn't get any router fee discounts.
        (uint256 amountOut, FeeRecipient[] memory feeRecipients) =
            feeCalculator.calculateFee(amountIn, BOB, 0);

        // routerFeeOnOutput = 1 ether * 100 / 10000 = 0.01 ether
        // amountOut = 1 ether - 0.01 ether = 0.99 ether
        assertEq(amountOut, 0.99 ether);
        // Router fee
        assertEq(feeRecipients[0].recipient, ALICE);
        assertEq(feeRecipients[0].feeAmount, 0.01 ether);
        // Client fee
        assertEq(feeRecipients[1].recipient, BOB);
        assertEq(feeRecipients[1].feeAmount, 0);
    }

    function testCalculateOnlyRouterFeeOnClientFee() public {
        // Test with only router fee on client fee set (requires client fee to be set too)
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnClientFee(1000); // 10% of client fee

        uint256 amountIn = 1 ether;
        uint16 clientFeeBps = 200; // 2%

        (uint256 amountOut, FeeRecipient[] memory feeRecipients) =
            feeCalculator.calculateFee(amountIn, BOB, clientFeeBps);

        // clientFee = 1 ether * 200 / 10000 = 0.02 ether
        // routerFeeOnClientFee = 0.02 ether * 1000 / 10000 = 0.002 ether
        // clientPortion = 0.02 - 0.002 = 0.018 ether
        // amountOut = 1 ether - 0.02 ether = 0.98 ether
        assertEq(amountOut, 0.98 ether);
        // Router fee
        assertEq(feeRecipients[0].recipient, address(this));
        assertEq(feeRecipients[0].feeAmount, 0.002 ether);
        // Client fee
        assertEq(feeRecipients[1].recipient, BOB);
        assertEq(feeRecipients[1].feeAmount, 0.018 ether);
    }

    function testCalculateWithCustomUserFee() public {
        // Set default router fee
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(100); // 1%

        // Set custom fee for BOB
        feeCalculator.setCustomRouterFeeOnOutput(BOB, 50); // 0.5%
        vm.stopPrank();

        uint256 amountIn = 1 ether;

        // ALICE should get default fee
        (uint256 amountOutAlice, FeeRecipient[] memory feeRecipientsAlice) =
            feeCalculator.calculateFee(amountIn, ALICE, 0);
        assertEq(amountOutAlice, 0.99 ether);
        // Router fee
        assertEq(feeRecipientsAlice[0].feeAmount, 0.01 ether);

        // BOB should get custom fee
        (uint256 amountOutBob, FeeRecipient[] memory feeRecipientsBob) =
            feeCalculator.calculateFee(amountIn, BOB, 0);
        assertEq(amountOutBob, 0.995 ether); // 0.5% fee
        // Router fee
        assertEq(feeRecipientsBob[0].feeAmount, 0.005 ether);
    }

    function testCalculateNoFeesSet() public view {
        // No fees set, should return full amount
        uint256 amountIn = 1 ether;

        (uint256 amountOut, FeeRecipient[] memory feeRecipients) =
            feeCalculator.calculateFee(amountIn, ALICE, 0);

        assertEq(amountOut, 1 ether);
        // Router fee
        assertEq(feeRecipients[0].recipient, address(this));
        assertEq(feeRecipients[0].feeAmount, 0);
        // Client fee
        assertEq(feeRecipients[1].recipient, ALICE);
        assertEq(feeRecipients[1].feeAmount, 0);
    }

    function testCalculateOnlyClientFee() public view {
        // Test with only client fee set, no router fees
        uint256 amountIn = 1 ether;
        uint16 clientFeeBps = 150; // 1.5%

        // BOB is the client - but there are no router fees to overwrite with custom client fees
        (uint256 amountOut, FeeRecipient[] memory feeRecipients) =
            feeCalculator.calculateFee(amountIn, BOB, clientFeeBps);

        // clientFee = 1 ether * 150 / 10000 = 0.015 ether
        // amountOut = 1 ether - 0.015 ether = 0.985 ether
        assertEq(amountOut, 0.985 ether);
        // Router fee
        assertEq(feeRecipients[0].recipient, address(this));
        assertEq(feeRecipients[0].feeAmount, 0);
        // Client fee
        assertEq(feeRecipients[1].recipient, BOB);
        assertEq(feeRecipients[1].feeAmount, 0.015 ether);
    }

    function testCalculateAllFeesSet() public {
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(50); // 0.5%
        feeCalculator.setRouterFeeOnClientFee(500); // 5% of client fee
        vm.stopPrank();

        uint256 amountIn = 1 ether;
        uint16 clientFeeBps = 200; // 2%

        (uint256 amountOut, FeeRecipient[] memory feeRecipients) =
            feeCalculator.calculateFee(amountIn, BOB, clientFeeBps);

        // 1. clientFee = 1 ether * 200 / 10000 = 0.02 ether
        //    routerFeeOnClientFee = 0.02 ether * 500 / 10000 = 0.001 ether
        //    clientPortion = 0.02 - 0.001 = 0.019 ether
        // 2. routerFeeOnOutput = 1 ether * 50 / 10000 = 0.005 ether (calculated on original amount)
        //    totalRouterFee = 0.001 + 0.005 = 0.006 ether
        //    amountOut = 1 ether - 0.019 ether - 0.006 ether = 0.975 ether
        assertEq(amountOut, 0.975 ether);
        // Router fee
        assertEq(feeRecipients[0].recipient, address(this));
        assertEq(feeRecipients[0].feeAmount, 0.006 ether);
        // Client fee
        assertEq(feeRecipients[1].recipient, BOB);
        assertEq(feeRecipients[1].feeAmount, 0.019 ether);
    }

    function testCalculateCombinedFeeTooHigh() public {
        // Test with client fee + router fee on output > 100%
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(5000); // 50%

        uint256 amountIn = 1 ether;
        uint16 clientFeeBps = 5001; // 50.01% - combined makes 100.01%

        vm.expectRevert(
            abi.encodeWithSelector(FeeCalculator__FeeTooHigh.selector)
        );
        feeCalculator.calculateFee(amountIn, BOB, clientFeeBps);
    }

    function testCalculateRouterFeeOnClientFeeTooHigh() public {
        // Setting router fee on client fee > 100% reverts at the setter
        vm.prank(FEE_SETTER);
        vm.expectRevert(
            abi.encodeWithSelector(FeeCalculator__FeeTooHigh.selector)
        );
        feeCalculator.setRouterFeeOnClientFee(10001); // 100.01%
    }

    function testCalculateWithCustomRouterFeeReceiver() public {
        // Set custom router fee receiver
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(BOB);
        feeCalculator.setRouterFeeOnOutput(100); // 1%
        vm.stopPrank();

        uint256 amountIn = 1 ether;

        (, FeeRecipient[] memory feeRecipients) =
            feeCalculator.calculateFee(amountIn, ALICE, 0);

        // Router fee
        assertEq(feeRecipients[0].recipient, BOB);
    }

    function testCalculateCustomRouterFeeOnClientFee() public {
        // Test that custom router fee on client fee overrides default
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(ADMIN);
        feeCalculator.setRouterFeeOnClientFee(1000); // 10% default
        feeCalculator.setCustomRouterFeeOnClientFee(ALICE, 500); // 5% custom for ALICE
        vm.stopPrank();

        uint256 amountIn = 1 ether;
        uint16 clientFeeBps = 200; // 2%

        // ALICE should get custom router fee on client fee (5%)
        (uint256 amountOutAlice, FeeRecipient[] memory feeRecipientsAlice) =
            feeCalculator.calculateFee(amountIn, ALICE, clientFeeBps);

        // routerFeeOnClientFee = 0.02 * 500 / 10000 = 0.001 ether
        assertEq(amountOutAlice, 0.98 ether); // 1 - 0.02 client fee
        // Router fee
        assertEq(feeRecipientsAlice[0].recipient, ADMIN);
        assertEq(feeRecipientsAlice[0].feeAmount, 0.001 ether);
        // Client fee
        assertEq(feeRecipientsAlice[1].recipient, ALICE);
        assertEq(feeRecipientsAlice[1].feeAmount, 0.019 ether); // 0.02 - 0.001 router cut

        // BOB should get default router fee on client fee (10%)
        (uint256 amountOutBob, FeeRecipient[] memory feeRecipientsBob) =
            feeCalculator.calculateFee(amountIn, BOB, clientFeeBps);

        // routerFeeOnClientFee = 0.02 * 1000 / 10000 = 0.002 ether
        assertEq(amountOutBob, 0.98 ether); // 1 - 0.02 client fee
        // Router fee
        assertEq(feeRecipientsBob[0].recipient, ADMIN);
        assertEq(feeRecipientsBob[0].feeAmount, 0.002 ether);
        // Client fee
        assertEq(feeRecipientsBob[1].recipient, BOB);
        assertEq(feeRecipientsBob[1].feeAmount, 0.018 ether); // 0.02 - 0.002 router cut
    }

    function testCalculateBothCustomFeesSet() public {
        // Test that both custom fees work together
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(100); // 1% default
        feeCalculator.setRouterFeeOnClientFee(1000); // 10% default
        feeCalculator.setCustomRouterFeeOnOutput(ALICE, 50); // 0.5% custom
        feeCalculator.setCustomRouterFeeOnClientFee(ALICE, 500); // 5% custom
        vm.stopPrank();

        uint256 amountIn = 1 ether;
        uint16 clientFeeBps = 200; // 2%

        (uint256 amountOut, FeeRecipient[] memory feeRecipients) =
            feeCalculator.calculateFee(amountIn, ALICE, clientFeeBps);

        // 1. clientFee = 1 ether * 200 / 10000 = 0.02 ether
        //    routerFeeOnClientFee = 0.02 * 500 / 10000 = 0.001 ether (custom 5%)
        //    clientPortion = 0.02 - 0.001 = 0.019 ether
        // 2. routerFeeOnOutput = 1 * 50 / 10000 = 0.005 ether (custom 0.5%, calculated on original amount)
        //    totalRouterFee = 0.001 + 0.005 = 0.006 ether
        //    amountOut = 1 - 0.019 - 0.006 = 0.975 ether
        assertEq(amountOut, 0.975 ether);
        // Router fee
        assertEq(feeRecipients[0].recipient, address(this));
        assertEq(feeRecipients[0].feeAmount, 0.006 ether);
        // Client fee
        assertEq(feeRecipients[1].recipient, ALICE);
        assertEq(feeRecipients[1].feeAmount, 0.019 ether); // 0.02 - 0.001 router cut
    }
}

// Tests relating to setting the fee values themselves with proper access control,
// but not performing calculations using these values.
contract FeeCalculatorConfigTest is Constants {
    FeeCalculator feeCalculator;

    function setUp() public {
        feeCalculator = new FeeCalculator(FEE_SETTER);
    }

    // ROUTER FEE ON OUTPUT TESTS
    function testSetRouterFeeOnOutputUnauthorized() public {
        vm.prank(BOB);
        vm.expectRevert();
        feeCalculator.setRouterFeeOnOutput(100);
    }

    function testSetRouterFeeOnOutput() public {
        // Set initial fee
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(100);
        assertEq(feeCalculator.getRouterFeeOnOutput(), 100);

        // Update fee
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(200);
        assertEq(feeCalculator.getRouterFeeOnOutput(), 200);
    }

    function testSetCustomRouterFeeOnOutput() public {
        // Set default fee
        vm.prank(FEE_SETTER);
        uint16 defaultFee = 100;
        feeCalculator.setRouterFeeOnOutput(defaultFee);

        // Set custom fee for user
        uint16 userFee = 50;
        vm.prank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnOutput(BOB, userFee);

        // Check user gets custom fee
        assertEq(feeCalculator.getEffectiveRouterFeeOnOutput(BOB), userFee);

        // Check other users still get default fee
        assertEq(feeCalculator.getEffectiveRouterFeeOnOutput(ALICE), defaultFee);
    }

    function testSetCustomRouterFeeOnOutputUnauthorized() public {
        vm.prank(BOB);
        vm.expectRevert();
        feeCalculator.setCustomRouterFeeOnOutput(BOB, 50);
    }

    function testSetCustomRouterFeeOnOutputWithoutDefault() public {
        // Set custom fee for user without setting default first
        uint16 userFee = 75;
        vm.prank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnOutput(ALICE, userFee);
        assertEq(feeCalculator.getEffectiveRouterFeeOnOutput(ALICE), userFee);
    }

    function testRemoveCustomRouterFeeOnOutput() public {
        // Set default and custom fee
        vm.startPrank(FEE_SETTER);
        uint16 defaultFee = 100;
        uint16 customFee = 50;
        feeCalculator.setRouterFeeOnOutput(defaultFee);
        feeCalculator.setCustomRouterFeeOnOutput(ALICE, customFee);
        vm.stopPrank();

        assertEq(feeCalculator.getEffectiveRouterFeeOnOutput(ALICE), customFee);

        // Remove custom fee
        vm.prank(FEE_SETTER);
        feeCalculator.removeCustomRouterFeeOnOutput(ALICE);

        // Should now return default fee
        assertEq(feeCalculator.getEffectiveRouterFeeOnOutput(ALICE), defaultFee);
    }

    function testRemoveCustomRouterFeeOnOutputUnauthorized() public {
        vm.prank(BOB);
        vm.expectRevert();
        feeCalculator.removeCustomRouterFeeOnOutput(ALICE);
    }

    // ROUTER FEE ON CLIENT FEE TESTS
    function testSetRouterFeeOnClientFeeUnauthorized() public {
        vm.prank(BOB);
        vm.expectRevert();
        feeCalculator.setRouterFeeOnClientFee(1000);
    }

    function testSetRouterFeeOnClientFee() public {
        uint16 defaultFee = 500;
        uint16 updatedFee = 1000;

        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnClientFee(defaultFee);
        assertEq(feeCalculator.getRouterFeeOnClientFee(), defaultFee);

        // Update fee
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnClientFee(updatedFee);
        assertEq(feeCalculator.getRouterFeeOnClientFee(), updatedFee);
    }

    function testSetCustomRouterFeeOnClientFee() public {
        // Set default fee
        vm.prank(FEE_SETTER);
        uint16 defaultFee = 1000;
        feeCalculator.setRouterFeeOnClientFee(defaultFee);

        // Set custom fee for user
        uint16 customFee = 500;

        vm.prank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnClientFee(BOB, customFee);

        // Check user gets custom fee
        assertEq(feeCalculator.getEffectiveRouterFeeOnClientFee(BOB), customFee);

        // Check other users still get default fee
        assertEq(
            feeCalculator.getEffectiveRouterFeeOnClientFee(ALICE), defaultFee
        );
    }

    function testSetCustomRouterFeeOnClientFeeUnauthorized() public {
        vm.prank(BOB);
        vm.expectRevert();
        feeCalculator.setCustomRouterFeeOnClientFee(ALICE, 500);
    }

    function testSetCustomRouterFeeOnClientFeeWithoutDefault() public {
        // Set custom fee for user without setting default first
        uint16 userFee = 750;

        vm.prank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnClientFee(ALICE, userFee);

        assertEq(feeCalculator.getEffectiveRouterFeeOnClientFee(ALICE), userFee);
    }

    function testRemoveCustomRouterFeeOnClientFee() public {
        // Set default and custom fee
        vm.startPrank(FEE_SETTER);
        uint16 defaultFee = 1000;
        uint16 customFee = 500;
        feeCalculator.setRouterFeeOnClientFee(defaultFee);
        feeCalculator.setCustomRouterFeeOnClientFee(ALICE, customFee);
        vm.stopPrank();

        assertEq(
            feeCalculator.getEffectiveRouterFeeOnClientFee(ALICE), customFee
        );

        // Remove custom fee
        vm.prank(FEE_SETTER);
        feeCalculator.removeCustomRouterFeeOnClientFee(ALICE);

        // Should now return default fee
        assertEq(
            feeCalculator.getEffectiveRouterFeeOnClientFee(ALICE), defaultFee
        );
    }

    function testRemoveCustomRouterFeeOnClientFeeUnauthorized() public {
        vm.prank(BOB);
        vm.expectRevert();
        feeCalculator.removeCustomRouterFeeOnClientFee(ALICE);
    }

    // FEE RECEIVER TESTS
    function testSetRouterFeeReceiver() public {
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(BOB);
        assertEq(feeCalculator.getRouterFeeReceiver(), BOB);
    }

    function testSetRouterFeeReceiverZeroAddressReverts() public {
        vm.prank(FEE_SETTER);
        vm.expectRevert(FeeCalculator__AddressZero.selector);
        feeCalculator.setRouterFeeReceiver(address(0));
    }

    function testSetRouterFeeReceiverUnauthorized() public {
        vm.prank(BOB);
        vm.expectRevert();
        feeCalculator.setRouterFeeReceiver(BOB);
    }

    function testSetRouterFeeReceiverUpdatesCorrectly() public {
        address newReceiver = address(0x888);

        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(BOB);
        assertEq(feeCalculator.getRouterFeeReceiver(), BOB);

        feeCalculator.setRouterFeeReceiver(newReceiver);
        vm.stopPrank();

        assertEq(feeCalculator.getRouterFeeReceiver(), newReceiver);
    }

    // MULTIPLE USER AND EDGE CASE TESTS
    function testMultipleUserCustomFees() public {
        address user1 = DUMMY;
        address user2 = DUMMY2;
        address user3 = DUMMY3;

        uint16 defaultRouterFeeOnOutput = 100;
        uint16 defaultRouterFeeOnClientFee = 1000;

        uint16 user1FeeOnOutput = 50;
        uint16 user1FeeOnClientFee = 500;

        uint16 user2FeeOnOutput = 150;
        uint16 user2FeeOnClientFee = 1500;

        // Set default fees
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(defaultRouterFeeOnOutput);
        feeCalculator.setRouterFeeOnClientFee(defaultRouterFeeOnClientFee);

        // Set custom fees for different users
        feeCalculator.setCustomRouterFeeOnOutput(user1, user1FeeOnOutput);
        feeCalculator.setCustomRouterFeeOnClientFee(user1, user1FeeOnClientFee);

        feeCalculator.setCustomRouterFeeOnOutput(user2, user2FeeOnOutput);
        feeCalculator.setCustomRouterFeeOnClientFee(user2, user2FeeOnClientFee);
        vm.stopPrank();

        // Verify each user has correct fees
        assertEq(
            feeCalculator.getEffectiveRouterFeeOnOutput(user1), user1FeeOnOutput
        );
        assertEq(
            feeCalculator.getEffectiveRouterFeeOnClientFee(user1),
            user1FeeOnClientFee
        );

        assertEq(
            feeCalculator.getEffectiveRouterFeeOnOutput(user2), user2FeeOnOutput
        );
        assertEq(
            feeCalculator.getEffectiveRouterFeeOnClientFee(user2),
            user2FeeOnClientFee
        );

        // User3 should get default fees
        assertEq(
            feeCalculator.getEffectiveRouterFeeOnOutput(user3),
            defaultRouterFeeOnOutput
        );
        assertEq(
            feeCalculator.getEffectiveRouterFeeOnClientFee(user3),
            defaultRouterFeeOnClientFee
        );
    }

    function testUpdateDefaultFeeDoesNotAffectCustomFees() public {
        // Set initial default fee
        vm.startPrank(FEE_SETTER);
        uint16 defaultFee = 100;
        uint16 bobFee = 50;
        uint16 updatedDefaultFee = 200;

        feeCalculator.setRouterFeeOnOutput(defaultFee);

        // Set custom fee for user
        feeCalculator.setCustomRouterFeeOnOutput(BOB, bobFee);

        // Update default fee
        feeCalculator.setRouterFeeOnOutput(updatedDefaultFee);
        vm.stopPrank();

        // User should still have custom fee
        assertEq(feeCalculator.getEffectiveRouterFeeOnOutput(BOB), bobFee);

        // Other users should get new default
        assertEq(
            feeCalculator.getEffectiveRouterFeeOnOutput(ALICE),
            updatedDefaultFee
        );
    }

    function testDefaultValues() public view {
        // Default fees should be zero
        assertEq(feeCalculator.getRouterFeeOnOutput(), 0);
        assertEq(feeCalculator.getRouterFeeOnClientFee(), 0);
        assertEq(feeCalculator.getEffectiveRouterFeeOnOutput(ALICE), 0);
        assertEq(feeCalculator.getEffectiveRouterFeeOnClientFee(ALICE), 0);
        // Default fee receiver should be the contract deployer
        assertEq(feeCalculator.getRouterFeeReceiver(), address(this));
    }

    function testMaximumFee() public {
        uint16 maxFee = type(uint16).max;

        vm.startPrank(FEE_SETTER);
        vm.expectRevert(
            abi.encodeWithSelector(FeeCalculator__FeeTooHigh.selector)
        );
        feeCalculator.setRouterFeeOnOutput(maxFee);
        vm.expectRevert(
            abi.encodeWithSelector(FeeCalculator__FeeTooHigh.selector)
        );
        feeCalculator.setRouterFeeOnClientFee(maxFee);
        vm.stopPrank();
    }

    function testMaximumValidFee() public {
        uint16 maxValidFee = 10000;

        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(maxValidFee);
        feeCalculator.setRouterFeeOnClientFee(maxValidFee);
        vm.stopPrank();

        assertEq(feeCalculator.getRouterFeeOnOutput(), maxValidFee);
        assertEq(feeCalculator.getRouterFeeOnClientFee(), maxValidFee);
    }

    function testRoleHolderCanTransferOwnRole() public {
        address newFeeSetter = makeAddr("newFeeSetter");

        vm.startPrank(FEE_SETTER);
        feeCalculator.grantRole(ROUTER_FEE_SETTER_ROLE, newFeeSetter);
        feeCalculator.revokeRole(ROUTER_FEE_SETTER_ROLE, FEE_SETTER);
        vm.stopPrank();

        // Old fee setter can no longer set fees
        vm.prank(FEE_SETTER);
        vm.expectRevert(
            abi.encodeWithSelector(
                IAccessControl.AccessControlUnauthorizedAccount.selector,
                FEE_SETTER,
                ROUTER_FEE_SETTER_ROLE
            )
        );
        feeCalculator.setRouterFeeOnOutput(100);

        // New fee setter can
        vm.prank(newFeeSetter);
        feeCalculator.setRouterFeeOnOutput(200);
        assertEq(feeCalculator.getRouterFeeOnOutput(), 200);
    }

    function testDefaultAdminRoleDoesNotExist() public view {
        bytes32 DEFAULT_ADMIN_ROLE = 0x00;

        assertFalse(feeCalculator.hasRole(DEFAULT_ADMIN_ROLE, address(this)));

        assertNotEq(
            feeCalculator.getRoleAdmin(ROUTER_FEE_SETTER_ROLE),
            DEFAULT_ADMIN_ROLE
        );
    }
}
