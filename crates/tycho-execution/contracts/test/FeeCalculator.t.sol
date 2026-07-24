pragma solidity ^0.8.26;

import "@src/FeeCalculator.sol";
import {
    IAccessControl
} from "@openzeppelin/contracts/access/IAccessControl.sol";
import {FeeRecipient, FeeInput} from "../lib/FeeStructs.sol";
import "./Constants.sol";

// Fee constants in the 8-decimal fee unit scale (100_000_000 = 100%).
uint32 constant _HALF_PCT = 500_000; // 0.5%
uint32 constant _1_PCT = 1_000_000; // 1%
uint32 constant _5_PCT = 5_000_000; // 5%
uint32 constant _10_PCT = 10_000_000; // 10%
uint32 constant _50_PCT = 50_000_000; // 50%
uint32 constant _100_PCT = 100_000_000; // 100%

/// @dev Helper to sum all fee amounts from the returned array
function _sumFees(FeeRecipient[] memory fees) pure returns (uint256 total) {
    for (uint256 i = 0; i < fees.length; i++) {
        total += fees[i].feeAmount;
    }
}

contract FeeCalculatorTest is Constants {
    FeeCalculator feeCalculator;

    function setUp() public {
        feeCalculator = new FeeCalculator(FEE_SETTER);
    }

    function testCalculateOnlyRouterFeeOnOutput() public {
        // Set router fee on output and receiver
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(ALICE);
        feeCalculator.setRouterFeeOnOutput(_1_PCT); // 1%
        vm.stopPrank();

        uint256 amountIn = 0.5 ether;
        uint256 actualAmountOut = 1 ether;
        uint256 expectedAmountOut = 1 ether;

        // The client is BOB - he doesn't get any router fee discounts.
        FeeRecipient[] memory feeRecipients = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 0,
                client: BOB
            })
        );
        uint256 amountOut = actualAmountOut - _sumFees(feeRecipients);

        // routerFeeOnOutput = 1 ether * 1_000_000 / 100_000_000 = 0.01 ether
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
        // Test with only router fee on client fee set
        // (requires client fee to be set too)
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnClientFee(_10_PCT); // 10% of client fee

        uint256 amountIn = 0.5 ether;
        uint256 actualAmountOut = 1 ether;
        uint256 expectedAmountOut = 1 ether;
        uint32 clientFeeBps = 2_000_000; // 2%

        FeeRecipient[] memory feeRecipients = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: clientFeeBps,
                client: BOB
            })
        );
        uint256 amountOut = actualAmountOut - _sumFees(feeRecipients);

        // clientFee = 1 ether * 2_000_000 / 100_000_000 = 0.02 ether
        // routerFeeOnClientFee = 0.02 ether * 10% = 0.002 ether
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
        feeCalculator.setRouterFeeOnOutput(_1_PCT); // 1%
        // Set custom fee for BOB
        feeCalculator.setCustomRouterFeeOnOutput(BOB, _HALF_PCT); // 0.5%
        vm.stopPrank();

        uint256 amountIn = 0.5 ether;
        uint256 actualAmountOut = 1 ether;
        uint256 expectedAmountOut = 1 ether;

        // ALICE should get default fee
        FeeRecipient[] memory feeRecipientsAlice = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 0,
                client: ALICE
            })
        );
        uint256 amountOutAlice = actualAmountOut - _sumFees(feeRecipientsAlice);
        assertEq(amountOutAlice, 0.99 ether);
        // Router fee
        assertEq(feeRecipientsAlice[0].feeAmount, 0.01 ether);

        // BOB should get custom fee
        FeeRecipient[] memory feeRecipientsBob = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 0,
                client: BOB
            })
        );
        uint256 amountOutBob = actualAmountOut - _sumFees(feeRecipientsBob);
        assertEq(amountOutBob, 0.995 ether); // 0.5% fee
        // Router fee
        assertEq(feeRecipientsBob[0].feeAmount, 0.005 ether);
    }

    function testCalculateAfterCustomFeeRemovalUsesDefault() public {
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(_1_PCT);
        feeCalculator.setCustomRouterFeeOnOutput(BOB, _HALF_PCT);
        feeCalculator.removeCustomRouterFeeOnOutput(BOB);
        vm.stopPrank();

        FeeRecipient[] memory feeRecipients = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: 1 ether,
                expectedAmountOut: 1 ether,
                amountIn: 0.5 ether,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 0,
                client: BOB
            })
        );

        // With the override removed, BOB pays the 1% default again
        assertEq(feeRecipients[0].feeAmount, 0.01 ether);
    }

    function testCalculateAfterCustomClientFeeRemovalUsesDefault() public {
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnClientFee(_10_PCT);
        feeCalculator.setCustomRouterFeeOnClientFee(BOB, _5_PCT);
        feeCalculator.removeCustomRouterFeeOnClientFee(BOB);
        vm.stopPrank();

        FeeRecipient[] memory feeRecipients = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: 1 ether,
                expectedAmountOut: 1 ether,
                amountIn: 0.5 ether,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 2_000_000, // 2%
                client: BOB
            })
        );

        // clientFee = 0.02 ether; with the override removed the router's cut
        // is the 10% default again: 0.002 ether (custom 5% would give 0.001)
        assertEq(feeRecipients[0].feeAmount, 0.002 ether);
        assertEq(feeRecipients[1].feeAmount, 0.018 ether);
    }

    function testCalculateNoFeesSet() public view {
        // No fees set, should return full amount
        uint256 amountIn = 0.5 ether;
        uint256 actualAmountOut = 1 ether;
        uint256 expectedAmountOut = 1 ether;

        FeeRecipient[] memory feeRecipients = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 0,
                client: ALICE
            })
        );
        uint256 amountOut = actualAmountOut - _sumFees(feeRecipients);

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
        uint256 amountIn = 0.5 ether;
        uint256 actualAmountOut = 1 ether;
        uint256 expectedAmountOut = 1 ether;
        uint32 clientFeeBps = 1_500_000; // 1.5%

        // BOB is the client - but there are no router fees to overwrite with custom client fees
        FeeRecipient[] memory feeRecipients = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: clientFeeBps,
                client: BOB
            })
        );
        uint256 amountOut = actualAmountOut - _sumFees(feeRecipients);

        // clientFee = 1 ether * 1_500_000 / 100_000_000 = 0.015 ether
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
        feeCalculator.setRouterFeeOnOutput(_HALF_PCT); // 0.5%
        feeCalculator.setRouterFeeOnClientFee(_5_PCT); // 5% of client fee
        vm.stopPrank();

        uint256 amountIn = 0.5 ether;
        uint256 actualAmountOut = 1 ether;
        uint256 expectedAmountOut = 1 ether;
        uint32 clientFeeBps = 2_000_000; // 2%

        FeeRecipient[] memory feeRecipients = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: clientFeeBps,
                client: BOB
            })
        );
        uint256 amountOut = actualAmountOut - _sumFees(feeRecipients);

        // 1. clientFee = 1 ether * 2_000_000 / 100_000_000 = 0.02 ether
        //    routerFeeOnClientFee = 0.02 ether * 5% = 0.001 ether
        //    clientPortion = 0.02 - 0.001 = 0.019 ether
        // 2. routerFeeOnOutput = 1 ether * 0.5% = 0.005 ether (calculated on original amount)
        //    totalRouterFee = 0.001 + 0.005 = 0.006 ether
        //    amountOut = 1 ether - 0.019 ether - 0.006 ether= 0.975 ether
        assertEq(amountOut, 0.975 ether);
        // Router fee
        assertEq(feeRecipients[0].recipient, address(this));
        assertEq(feeRecipients[0].feeAmount, 0.006 ether);
        // Client fee
        assertEq(feeRecipients[1].recipient, BOB);
        assertEq(feeRecipients[1].feeAmount, 0.019 ether); // 0.02 - 0.001 router cut
    }

    function testCalculateCombinedFeeTooHigh() public {
        // Test with client fee + router fee on output > 100%
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(_50_PCT); // 50%

        uint256 amountIn = 0.5 ether;
        uint256 actualAmountOut = 1 ether;
        uint256 expectedAmountOut = 1 ether;
        uint32 clientFeeBps = 50_010_000; // 50.01% — combined makes 100.01%

        vm.expectRevert(
            abi.encodeWithSelector(FeeCalculator__FeeTooHigh.selector)
        );
        feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: clientFeeBps,
                client: BOB
            })
        );
    }

    function testCalculateRouterFeeOnClientFeeTooHigh() public {
        // Setting router fee on client fee > 100% reverts at the setter
        vm.prank(FEE_SETTER);
        vm.expectRevert(
            abi.encodeWithSelector(FeeCalculator__FeeTooHigh.selector)
        );
        feeCalculator.setRouterFeeOnClientFee(100_000_001); // 100.000001%
    }

    function testCalculateWithCustomRouterFeeReceiver() public {
        // Set custom router fee receiver
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(BOB);
        feeCalculator.setRouterFeeOnOutput(_1_PCT); // 1%
        vm.stopPrank();

        uint256 amountIn = 0.5 ether;
        uint256 actualAmountOut = 1 ether;
        uint256 expectedAmountOut = 1 ether;

        FeeRecipient[] memory feeRecipients = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 0,
                client: ALICE
            })
        );

        // Router fee
        assertEq(feeRecipients[0].recipient, BOB);
    }

    function testCalculateCustomRouterFeeOnClientFee() public {
        // Test that custom router fee on client fee overrides default
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(ADMIN);
        feeCalculator.setRouterFeeOnClientFee(_10_PCT); // 10% default
        feeCalculator.setCustomRouterFeeOnClientFee(ALICE, _5_PCT); // 5% custom for ALICE
        vm.stopPrank();

        uint256 amountIn = 0.5 ether;
        uint256 actualAmountOut = 1 ether;
        uint256 expectedAmountOut = 1 ether;
        uint32 clientFeeBps = 2_000_000; // 2%

        // ALICE should get custom router fee on client fee (5%)
        FeeRecipient[] memory feeRecipientsAlice = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: clientFeeBps,
                client: ALICE
            })
        );
        uint256 amountOutAlice = actualAmountOut - _sumFees(feeRecipientsAlice);

        // routerFeeOnClientFee = 0.02 * 5% = 0.001 ether
        assertEq(amountOutAlice, 0.98 ether); // 1 - 0.02 client fee
        // Router fee
        assertEq(feeRecipientsAlice[0].recipient, ADMIN);
        assertEq(feeRecipientsAlice[0].feeAmount, 0.001 ether);
        // Client fee
        assertEq(feeRecipientsAlice[1].recipient, ALICE);
        assertEq(
            feeRecipientsAlice[1].feeAmount,
            0.019 ether // 0.02 - 0.001 router cut
        );

        // BOB should get default router fee on client fee (10%)
        FeeRecipient[] memory feeRecipientsBob = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: clientFeeBps,
                client: BOB
            })
        );
        uint256 amountOutBob = actualAmountOut - _sumFees(feeRecipientsBob);

        // routerFeeOnClientFee = 0.02 * 10% = 0.002 ether
        assertEq(amountOutBob, 0.98 ether); // 1 - 0.02 client fee
        // Router fee
        assertEq(feeRecipientsBob[0].recipient, ADMIN);
        assertEq(feeRecipientsBob[0].feeAmount, 0.002 ether);
        // Client fee
        assertEq(feeRecipientsBob[1].recipient, BOB);
        assertEq(
            feeRecipientsBob[1].feeAmount,
            0.018 ether // 0.02 - 0.002 router cut
        );
    }

    function testCalculateBothCustomFeesSet() public {
        // Test that both custom fees work together
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(_1_PCT); // 1% default
        feeCalculator.setRouterFeeOnClientFee(_10_PCT); // 10% default
        feeCalculator.setCustomRouterFeeOnOutput(ALICE, _HALF_PCT); // 0.5% custom
        feeCalculator.setCustomRouterFeeOnClientFee(ALICE, _5_PCT); // 5% custom
        vm.stopPrank();

        uint256 amountIn = 0.5 ether;
        uint256 actualAmountOut = 1 ether;
        uint256 expectedAmountOut = 1 ether;
        uint32 clientFeeBps = 2_000_000; // 2%

        FeeRecipient[] memory feeRecipients = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: clientFeeBps,
                client: ALICE
            })
        );
        uint256 amountOut = actualAmountOut - _sumFees(feeRecipients);

        // 1. clientFee = 1 ether * 2_000_000 / 100_000_000 = 0.02 ether
        //    routerFeeOnClientFee = 0.02 * 5% (custom) = 0.001 ether
        //    clientPortion = 0.02 - 0.001 = 0.019 ether
        // 2. routerFeeOnOutput = 1 * 0.5% (custom) = 0.005 ether (calculated on original amount)
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

    function testCalculateFractionalRouterFee() public {
        // Router fees support sub-BPS precision (e.g. 1.5 BPS = 15_000 internal units)
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(15_000); // 1.5 BPS = 0.015%

        uint256 amountIn = 0.5 ether;
        uint256 actualAmountOut = 1 ether;
        uint256 expectedAmountOut = 1 ether;

        FeeRecipient[] memory feeRecipients = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 0,
                client: BOB
            })
        );
        uint256 amountOut = actualAmountOut - _sumFees(feeRecipients);

        // routerFeeOnOutput = 1 ether * 15_000 / 100_000_000 = 0.00015 ether
        assertEq(amountOut, 1 ether - 0.00015 ether);
        assertEq(feeRecipients[0].feeAmount, 0.00015 ether);
    }

    function testCalculateFeeUsesOrigin() public {
        // When client == address(0) (no signature), custom fees for tx.origin are applied.
        vm.prank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnOutput(ALICE, _1_PCT);

        uint256 amountIn = 0.5 ether;
        uint256 actualAmountOut = 1 ether;
        uint256 expectedAmountOut = 1 ether;

        // Call with client=address(0) and tx.origin=ALICE
        vm.prank(address(this), ALICE);
        FeeRecipient[] memory fees = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 0,
                client: address(0)
            })
        );
        uint256 amountOut = actualAmountOut - _sumFees(fees);

        // ALICE's 1% custom fee should be applied
        assertEq(fees[0].feeAmount, 0.01 ether);
        assertEq(amountOut, 0.99 ether);
    }

    function testCalculateFeeWithClientAddress() public {
        // When client != address(0), tx.origin fees are ignored — client address takes precedence.
        // ALICE (tx.origin) has a custom 1% fee, BOB (client) uses the 5% default
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(_5_PCT);
        feeCalculator.setCustomRouterFeeOnOutput(ALICE, _1_PCT);
        vm.stopPrank();

        uint256 amountIn = 0.5 ether;
        uint256 actualAmountOut = 1 ether;
        uint256 expectedAmountOut = 1 ether;

        // Call with client=BOB (no custom fee), tx.origin=ALICE
        vm.prank(address(this), ALICE);
        FeeRecipient[] memory fees = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 0,
                client: BOB
            })
        );
        uint256 amountOut = actualAmountOut - _sumFees(fees);

        // Default 5% fee is applied for BOB — ALICE's 1% custom fee is ignored
        // fee = 1 ether * 5_000_000 / 100_000_000 = 0.05 ether
        assertEq(fees[0].feeAmount, 0.05 ether);
        assertEq(amountOut, 0.95 ether);
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
        feeCalculator.setRouterFeeOnOutput(_1_PCT);
    }

    function testSetRouterFeeOnOutput() public {
        // Set initial fee
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(_1_PCT);
        assertEq(feeCalculator.getRouterFeeOnOutput(), _1_PCT);

        // Update fee
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(2_000_000); // 2%
        assertEq(feeCalculator.getRouterFeeOnOutput(), 2_000_000);
    }

    function testSetCustomRouterFeeOnOutput() public {
        uint32 defaultFee = _1_PCT;
        uint32 userFee = _HALF_PCT;

        // Set default fee
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(defaultFee);

        // Set custom fee for user
        vm.prank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnOutput(BOB, userFee);

        // BOB has the override; no one else does
        (address[] memory clients, CustomFees[] memory fees) =
            feeCalculator.getAllClientFees(0, 10);
        assertEq(clients.length, 1);
        assertEq(clients[0], BOB);
        assertTrue(fees[0].hasCustomFeeOnOutput);
        assertEq(fees[0].feeBpsOnOutput, userFee);
        // The default is unchanged
        assertEq(feeCalculator.getRouterFeeOnOutput(), defaultFee);
    }

    function testSetCustomRouterFeeOnOutputUnauthorized() public {
        vm.prank(BOB);
        vm.expectRevert();
        feeCalculator.setCustomRouterFeeOnOutput(BOB, _HALF_PCT);
    }

    function testSetCustomRouterFeeOnOutputWithoutDefault() public {
        vm.prank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnOutput(ALICE, 750_000); // 0.75%

        (, CustomFees[] memory fees) = feeCalculator.getAllClientFees(0, 10);
        assertTrue(fees[0].hasCustomFeeOnOutput);
        assertEq(fees[0].feeBpsOnOutput, 750_000);
        assertEq(feeCalculator.getRouterFeeOnOutput(), 0);
    }

    function testRemoveCustomRouterFeeOnOutput() public {
        uint32 defaultFee = _1_PCT;
        uint32 userFee = _HALF_PCT;

        // Set default and custom fee
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(defaultFee);
        feeCalculator.setCustomRouterFeeOnOutput(ALICE, userFee);
        vm.stopPrank();

        (address[] memory clients,) = feeCalculator.getAllClientFees(0, 10);
        assertEq(clients.length, 1);

        // Remove custom fee
        vm.prank(FEE_SETTER);
        feeCalculator.removeCustomRouterFeeOnOutput(ALICE);

        // Override cleared; default untouched
        (clients,) = feeCalculator.getAllClientFees(0, 10);
        assertEq(clients.length, 0);
        assertEq(feeCalculator.getRouterFeeOnOutput(), defaultFee);
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
        feeCalculator.setRouterFeeOnClientFee(_10_PCT);
    }

    function testSetRouterFeeOnClientFee() public {
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnClientFee(_5_PCT);
        assertEq(feeCalculator.getRouterFeeOnClientFee(), _5_PCT);

        // Update fee
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnClientFee(_10_PCT);
        assertEq(feeCalculator.getRouterFeeOnClientFee(), _10_PCT);
    }

    function testSetCustomRouterFeeOnClientFee() public {
        uint32 defaultFee = _10_PCT;
        uint32 userFee = _5_PCT;

        // Set default fee
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnClientFee(defaultFee);

        // Set custom fee for user
        vm.prank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnClientFee(BOB, userFee);

        // BOB has the override; no one else does
        (address[] memory clients, CustomFees[] memory fees) =
            feeCalculator.getAllClientFees(0, 10);
        assertEq(clients.length, 1);
        assertEq(clients[0], BOB);
        assertTrue(fees[0].hasCustomFeeOnClientFee);
        assertEq(fees[0].feeBpsOnClientFee, userFee);
        // The default is unchanged
        assertEq(feeCalculator.getRouterFeeOnClientFee(), defaultFee);
    }

    function testSetCustomRouterFeeOnClientFeeUnauthorized() public {
        vm.prank(BOB);
        vm.expectRevert();
        feeCalculator.setCustomRouterFeeOnClientFee(ALICE, _5_PCT);
    }

    function testSetCustomRouterFeeOnClientFeeWithoutDefault() public {
        vm.prank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnClientFee(ALICE, 7_500_000); // 7.5%

        (, CustomFees[] memory fees) = feeCalculator.getAllClientFees(0, 10);
        assertTrue(fees[0].hasCustomFeeOnClientFee);
        assertEq(fees[0].feeBpsOnClientFee, 7_500_000);
        assertEq(feeCalculator.getRouterFeeOnClientFee(), 0);
    }

    function testRemoveCustomRouterFeeOnClientFee() public {
        uint32 defaultFee = _10_PCT;
        uint32 userFee = _5_PCT;

        // Set default and custom fee
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnClientFee(defaultFee);
        feeCalculator.setCustomRouterFeeOnClientFee(ALICE, userFee);
        vm.stopPrank();

        (address[] memory clients,) = feeCalculator.getAllClientFees(0, 10);
        assertEq(clients.length, 1);

        // Remove custom fee
        vm.prank(FEE_SETTER);
        feeCalculator.removeCustomRouterFeeOnClientFee(ALICE);

        // Override cleared; default untouched
        (clients,) = feeCalculator.getAllClientFees(0, 10);
        assertEq(clients.length, 0);
        assertEq(feeCalculator.getRouterFeeOnClientFee(), defaultFee);
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

        // Set default fees
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(_1_PCT);
        feeCalculator.setRouterFeeOnClientFee(_10_PCT);

        // Set custom fees for different users
        feeCalculator.setCustomRouterFeeOnOutput(user1, _HALF_PCT);
        feeCalculator.setCustomRouterFeeOnClientFee(user1, _5_PCT);

        feeCalculator.setCustomRouterFeeOnOutput(user2, 1_500_000); // 1.5%
        feeCalculator.setCustomRouterFeeOnClientFee(user2, 15_000_000); // 15%
        vm.stopPrank();

        // Verify each user's override via getAllClientFees (insertion order)
        (address[] memory clients, CustomFees[] memory fees) =
            feeCalculator.getAllClientFees(0, 10);
        assertEq(clients.length, 2);
        assertEq(clients[0], user1);
        assertEq(fees[0].feeBpsOnOutput, _HALF_PCT);
        assertEq(fees[0].feeBpsOnClientFee, _5_PCT);

        assertEq(clients[1], user2);
        assertEq(fees[1].feeBpsOnOutput, 1_500_000);
        assertEq(fees[1].feeBpsOnClientFee, 15_000_000);

        // User3 has no overrides and falls back to the defaults
        assertEq(feeCalculator.getRouterFeeOnOutput(), _1_PCT);
        assertEq(feeCalculator.getRouterFeeOnClientFee(), _10_PCT);
    }

    function testUpdateDefaultFeeDoesNotAffectCustomFees() public {
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(_1_PCT);
        feeCalculator.setCustomRouterFeeOnOutput(BOB, _HALF_PCT);
        feeCalculator.setRouterFeeOnOutput(2_000_000); // 2%
        vm.stopPrank();

        // User should still have the custom fee
        (address[] memory clients, CustomFees[] memory fees) =
            feeCalculator.getAllClientFees(0, 10);
        assertEq(clients[0], BOB);
        assertEq(fees[0].feeBpsOnOutput, _HALF_PCT);
        // The default was updated
        assertEq(feeCalculator.getRouterFeeOnOutput(), 2_000_000);
    }

    function testDefaultValues() public view {
        // Default fees should be zero
        assertEq(feeCalculator.getRouterFeeOnOutput(), 0);
        assertEq(feeCalculator.getRouterFeeOnClientFee(), 0);
        // Default fee receiver should be the contract deployer
        assertEq(feeCalculator.getRouterFeeReceiver(), address(this));
        assertFalse(feeCalculator.getPositiveSlippageEnabled());
    }

    function testMaximumFee() public {
        uint32 maxFee = type(uint32).max;

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
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(_100_PCT);
        feeCalculator.setRouterFeeOnClientFee(_100_PCT);
        vm.stopPrank();

        assertEq(feeCalculator.getRouterFeeOnOutput(), _100_PCT);
        assertEq(feeCalculator.getRouterFeeOnClientFee(), _100_PCT);
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
        feeCalculator.setRouterFeeOnOutput(_1_PCT);

        // New fee setter can
        vm.prank(newFeeSetter);
        feeCalculator.setRouterFeeOnOutput(2_000_000);
        assertEq(feeCalculator.getRouterFeeOnOutput(), 2_000_000);
    }

    function testDefaultAdminRoleDoesNotExist() public view {
        bytes32 DEFAULT_ADMIN_ROLE = 0x00;

        assertFalse(feeCalculator.hasRole(DEFAULT_ADMIN_ROLE, address(this)));

        assertNotEq(
            feeCalculator.getRoleAdmin(ROUTER_FEE_SETTER_ROLE),
            DEFAULT_ADMIN_ROLE
        );
    }

    function testGetAllClientFeesEmpty() public view {
        (address[] memory clients, CustomFees[] memory fees) =
            feeCalculator.getAllClientFees(0, type(uint256).max);
        assertEq(clients.length, 0);
        assertEq(fees.length, 0);
    }

    function testGetAllClientFees() public {
        vm.startPrank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnOutput(ALICE, _HALF_PCT); // 0.5%
        feeCalculator.setCustomRouterFeeOnOutput(BOB, _1_PCT); // 1%
        feeCalculator.setCustomRouterFeeOnClientFee(BOB, _5_PCT); // 5%
        vm.stopPrank();

        (address[] memory clients, CustomFees[] memory fees) =
            feeCalculator.getAllClientFees(0, type(uint256).max);

        assertEq(clients.length, 2);

        bool foundAlice = false;
        bool foundBob = false;
        for (uint256 i = 0; i < clients.length; i++) {
            if (clients[i] == ALICE) {
                foundAlice = true;
                assertTrue(fees[i].hasCustomFeeOnOutput);
                assertEq(fees[i].feeBpsOnOutput, _HALF_PCT);
                assertFalse(fees[i].hasCustomFeeOnClientFee);
                assertEq(fees[i].feeBpsOnClientFee, 0);
            } else if (clients[i] == BOB) {
                foundBob = true;
                assertTrue(fees[i].hasCustomFeeOnOutput);
                assertEq(fees[i].feeBpsOnOutput, _1_PCT);
                assertTrue(fees[i].hasCustomFeeOnClientFee);
                assertEq(fees[i].feeBpsOnClientFee, _5_PCT);
            }
        }
        assertTrue(foundAlice);
        assertTrue(foundBob);
    }

    function testGetAllClientFeesPagination() public {
        vm.startPrank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnOutput(ALICE, _HALF_PCT);
        feeCalculator.setCustomRouterFeeOnOutput(BOB, _1_PCT);
        address CAROL = makeAddr("carol");
        feeCalculator.setCustomRouterFeeOnOutput(CAROL, _5_PCT);
        vm.stopPrank();

        // First page: 2 entries
        (address[] memory page1,) = feeCalculator.getAllClientFees(0, 2);
        assertEq(page1.length, 2);

        // Second page: remaining 1 entry
        (address[] memory page2,) = feeCalculator.getAllClientFees(2, 2);
        assertEq(page2.length, 1);

        // Out-of-bounds start returns empty
        (address[] memory empty,) = feeCalculator.getAllClientFees(10, 2);
        assertEq(empty.length, 0);
    }

    function testGetAllClientFeesAfterRemovingClient() public {
        vm.startPrank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnOutput(ALICE, _HALF_PCT);
        feeCalculator.setCustomRouterFeeOnOutput(BOB, _1_PCT);
        vm.stopPrank();

        vm.prank(FEE_SETTER);
        feeCalculator.removeCustomRouterFeeOnOutput(ALICE);

        (address[] memory clients,) = feeCalculator.getAllClientFees(0, 10);

        assertEq(clients.length, 1);
        assertEq(clients[0], BOB);
    }

    function testGetAllClientFeesClientStaysWhenOneFeeRemoved() public {
        vm.startPrank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnOutput(BOB, _1_PCT);
        feeCalculator.setCustomRouterFeeOnClientFee(BOB, _5_PCT);
        vm.stopPrank();

        vm.prank(FEE_SETTER);
        feeCalculator.removeCustomRouterFeeOnOutput(BOB);

        (address[] memory clients, CustomFees[] memory fees) =
            feeCalculator.getAllClientFees(0, 10);

        assertEq(clients.length, 1);
        assertEq(clients[0], BOB);
        assertFalse(fees[0].hasCustomFeeOnOutput);
        assertTrue(fees[0].hasCustomFeeOnClientFee);
        assertEq(fees[0].feeBpsOnClientFee, _5_PCT);
    }

    function testClientRemovedOnlyWhenAllCustomFeesCleared() public {
        vm.startPrank(FEE_SETTER);
        feeCalculator.setCustomRouterFeeOnOutput(BOB, _1_PCT);
        feeCalculator.setCustomRouterFeeOnClientFee(BOB, _5_PCT);

        // Remove output fee — client stays (client fee remains)
        feeCalculator.removeCustomRouterFeeOnOutput(BOB);
        (address[] memory clients,) = feeCalculator.getAllClientFees(0, 10);
        assertEq(clients.length, 1);

        // Remove client fee — client removed (no custom fees left)
        feeCalculator.removeCustomRouterFeeOnClientFee(BOB);
        vm.stopPrank();

        (clients,) = feeCalculator.getAllClientFees(0, 10);
        assertEq(clients.length, 0);
    }
}

// Tests for positive slippage surplus distribution
contract FeeCalculatorSlippageTest is Constants {
    FeeCalculator feeCalculator;

    function setUp() public {
        feeCalculator = new FeeCalculator(FEE_SETTER);
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(ADMIN);
        feeCalculator.setPositiveSlippageEnabled(true);
        vm.stopPrank();
    }

    function testRouterKeepsAllPositiveSlippage() public view {
        // The full surplus always goes to the router
        uint256 actualAmountOut = 1.1 ether;
        uint256 expectedAmountOut = 1 ether;

        FeeRecipient[] memory fees = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: 0,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 0,
                client: BOB
            })
        );

        // surplus = 0.1 ether, all to router
        assertEq(fees[0].recipient, ADMIN);
        assertEq(fees[0].feeAmount, 0.1 ether);
        assertEq(fees[1].recipient, BOB);
        assertEq(fees[1].feeAmount, 0);
    }

    function testNegativeSlippageNoSurplus() public {
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(_1_PCT);

        uint256 actualAmountOut = 0.9 ether;
        uint256 expectedAmountOut = 1 ether;

        FeeRecipient[] memory fees = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: 0,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 0,
                client: BOB
            })
        );

        // No surplus, but router fee on output still applies
        // routerFee = 0.9 ether * 1% = 0.009 ether (based on actualAmountOut)
        assertEq(fees[0].feeAmount, 0.009 ether);
        assertEq(fees[1].feeAmount, 0);
    }

    function testPositiveSlippageWithRouterFeeOnOutput() public {
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(_1_PCT);

        uint256 actualAmountOut = 1.1 ether;
        uint256 expectedAmountOut = 1 ether;

        FeeRecipient[] memory fees = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: 0,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 0,
                client: BOB
            })
        );

        // surplus = 0.1 ether, all to router
        // feeBase = 1.1 - 0.1 = 1 ether (expectedAmountOut)
        // routerFee on output = 1 ether * 1% = 0.01 ether
        // total router = 0.1 + 0.01 = 0.11 ether
        assertEq(fees[0].recipient, ADMIN);
        assertEq(fees[0].feeAmount, 0.11 ether);
        assertEq(fees[1].recipient, BOB);
        assertEq(fees[1].feeAmount, 0);
    }

    function testZeroSlippageNoSurplus() public {
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnOutput(_1_PCT);

        uint256 amount = 1 ether;

        FeeRecipient[] memory fees = feeCalculator.calculateFee(
            FeeInput({
                actualAmountOut: amount,
                expectedAmountOut: amount,
                amountIn: 0,
                tokenIn: address(0),
                tokenOut: address(0),
                clientFeeBps: 0,
                client: BOB
            })
        );

        // No surplus, but router fee on output still applies
        assertEq(fees[0].feeAmount, 0.01 ether);
        assertEq(fees[1].feeAmount, 0);
    }
}
