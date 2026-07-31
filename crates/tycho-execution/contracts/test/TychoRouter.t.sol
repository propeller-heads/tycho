pragma solidity ^0.8.26;

import {
    TychoRouter,
    TychoRouter__NoPendingFeeCalculator
} from "@src/TychoRouter.sol";
import {FeeCalculator} from "@src/FeeCalculator.sol";
import {
    IAccessControl
} from "@openzeppelin/contracts/access/IAccessControl.sol";
import "@src/TransferManager.sol";
import "./TychoRouterTestSetup.sol";

contract TychoRouterTest is TychoRouterTestSetup {
    bytes32 public constant EXECUTOR_SETTER_ROLE =
        0x6a1dd52dcad5bd732e45b6af4e7344fa284e2d7d4b23b5b09cb55d36b0685c87;
    bytes32 public constant PAUSER_ROLE =
        0x65d7a28e3265b37a6474929f336521b332c1681b933f6cb9f3376673440d862a;

    event CallbackVerifierSet(address indexed callbackVerifier);
    event Withdrawal(
        address indexed token, uint256 amount, address indexed receiver
    );

    function testSetExecutorsValidRole() public {
        // Set single executor
        address[] memory executors = new address[](1);
        executors[0] = DUMMY;
        vm.startPrank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.stopPrank();
        assertGt(tychoRouter.executorsActivationTimestamp(DUMMY), 0);

        // Set multiple executors
        address[] memory executors2 = new address[](2);
        executors2[0] = DUMMY2;
        executors2[1] = DUMMY3;
        vm.startPrank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors2);
        vm.stopPrank();
        assertGt(tychoRouter.executorsActivationTimestamp(DUMMY2), 0);
        assertGt(tychoRouter.executorsActivationTimestamp(DUMMY3), 0);
    }

    function testRemoveExecutorValidRole() public {
        vm.startPrank(EXECUTOR_SETTER);
        address[] memory executors = new address[](1);
        executors[0] = DUMMY;
        tychoRouter.setExecutors(executors);
        tychoRouter.removeExecutor(DUMMY);
        vm.stopPrank();
        assertEq(tychoRouter.executorsActivationTimestamp(DUMMY), 0);
    }

    function testRemoveExecutorMissingSetterRole() public {
        vm.expectRevert();
        tychoRouter.removeExecutor(BOB);
    }

    function testSetExecutorsMissingSetterRole() public {
        vm.expectRevert();
        address[] memory executors = new address[](1);
        executors[0] = DUMMY;
        tychoRouter.setExecutors(executors);
    }

    function testPause() public {
        vm.startPrank(PAUSER);
        assertEq(tychoRouter.paused(), false);
        tychoRouter.pause();
        assertEq(tychoRouter.paused(), true);
        vm.stopPrank();

        vm.startPrank(UNPAUSER);
        tychoRouter.unpause();
        assertEq(tychoRouter.paused(), false);
        vm.stopPrank();

        vm.startPrank(UNPAUSER);
        vm.expectRevert();
        tychoRouter.unpause();
        vm.stopPrank();
    }

    function testPauseNonRole() public {
        vm.startPrank(BOB);
        vm.expectRevert();
        tychoRouter.pause();
        vm.stopPrank();
    }

    function testEmptySwapsRevert() public {
        uint256 amountIn = 10 ** 18;
        bytes memory swaps = "";
        vm.expectRevert(TychoRouter__EmptySwaps.selector);
        tychoRouter.exposedSplitSwap(amountIn, 2, swaps, address(0), false);
    }

    // FEE CALCULATOR TESTS
    function testSetFeeCalculatorQueuesPending() public {
        FeeCalculator newCalc = new FeeCalculator(FEE_SETTER);
        vm.prank(FEE_SETTER);
        tychoRouter.setFeeCalculator(address(newCalc));

        (address pending, uint256 activationTs) =
            tychoRouter.getPendingFeeCalculator();
        assertEq(pending, address(newCalc));
        assertEq(
            activationTs,
            block.timestamp + tychoRouter.DELAY_FEE_CALCULATOR_ACTIVATION()
        );
        // Active calculator unchanged
        assertEq(tychoRouter.getFeeCalculator(), address(feeCalculator));
    }

    function testActivationAfterTimelock() public {
        FeeCalculator newCalc = new FeeCalculator(FEE_SETTER);
        vm.prank(FEE_SETTER);
        tychoRouter.setFeeCalculator(address(newCalc));

        vm.warp(block.timestamp + tychoRouter.DELAY_FEE_CALCULATOR_ACTIVATION());
        vm.prank(FEE_SETTER);
        tychoRouter.activateFeeCalculator();

        assertEq(tychoRouter.getFeeCalculator(), address(newCalc));
        (address pending,) = tychoRouter.getPendingFeeCalculator();
        assertEq(pending, address(0));
    }

    function testActivationRevertsWhenTimelocked() public {
        FeeCalculator newCalc = new FeeCalculator(FEE_SETTER);
        vm.prank(FEE_SETTER);
        tychoRouter.setFeeCalculator(address(newCalc));

        vm.prank(FEE_SETTER);
        vm.expectRevert();
        tychoRouter.activateFeeCalculator();
        assertEq(tychoRouter.getFeeCalculator(), address(feeCalculator));
    }

    function testActivationRevertsWhenNoPending() public {
        vm.prank(FEE_SETTER);
        vm.expectRevert(TychoRouter__NoPendingFeeCalculator.selector);
        tychoRouter.activateFeeCalculator();
    }

    function testSetFeeCalculatorNonContract() public {
        vm.prank(FEE_SETTER);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__NotAContract.selector, address(0)
            )
        );
        tychoRouter.setFeeCalculator(address(0));
    }

    function testSetFeeCalculatorUnauthorized() public {
        vm.prank(ALICE);
        vm.expectRevert();
        tychoRouter.setFeeCalculator(address(feeCalculator));
    }

    function testSetFeeCalculatorOverwritesPending() public {
        FeeCalculator calc1 = new FeeCalculator(FEE_SETTER);
        FeeCalculator calc2 = new FeeCalculator(FEE_SETTER);

        vm.startPrank(FEE_SETTER);
        tychoRouter.setFeeCalculator(address(calc1));
        tychoRouter.setFeeCalculator(address(calc2));
        vm.stopPrank();

        (address pending,) = tychoRouter.getPendingFeeCalculator();
        assertEq(pending, address(calc2));
    }

    function testConstructorNonContractFeeCalculator() public {
        address nonContract = address(0x999);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__NotAContract.selector, nonContract
            )
        );
        new TychoRouterExposed(
            PERMIT2_ADDRESS,
            nonContract,
            PAUSER,
            UNPAUSER,
            EXECUTOR_SETTER,
            FEE_SETTER
        );
    }

    function testConstructorNonContractPermit2() public {
        // Deploy a new FeeCalculator contract
        FeeCalculator newFeeCalculator = new FeeCalculator(FEE_SETTER);
        address nonContract = address(0x999);
        vm.expectRevert(
            abi.encodeWithSelector(
                TransferManager__NotAContract.selector, nonContract
            )
        );
        new TychoRouterExposed(
            nonContract,
            address(newFeeCalculator),
            PAUSER,
            UNPAUSER,
            EXECUTOR_SETTER,
            FEE_SETTER
        );
    }

    function testRoleHolderCanTransferOwnRole() public {
        address newPauser = makeAddr("newPauser");

        vm.startPrank(PAUSER);
        tychoRouter.grantRole(PAUSER_ROLE, newPauser);
        tychoRouter.revokeRole(PAUSER_ROLE, PAUSER);
        vm.stopPrank();

        // Old pauser can no longer pause
        vm.prank(PAUSER);
        vm.expectRevert(
            abi.encodeWithSelector(
                IAccessControl.AccessControlUnauthorizedAccount.selector,
                PAUSER,
                PAUSER_ROLE
            )
        );
        tychoRouter.pause();

        // New pauser can
        vm.prank(newPauser);
        tychoRouter.pause();
        assertTrue(tychoRouter.paused());
    }

    function testDefaultAdminRoleDoesNotExist() public view {
        bytes32 DEFAULT_ADMIN_ROLE = 0x00;

        assertFalse(tychoRouter.hasRole(DEFAULT_ADMIN_ROLE, ADMIN));
        assertFalse(tychoRouter.hasRole(DEFAULT_ADMIN_ROLE, address(this)));

        assertNotEq(tychoRouter.getRoleAdmin(PAUSER_ROLE), DEFAULT_ADMIN_ROLE);
        assertNotEq(
            tychoRouter.getRoleAdmin(keccak256("UNPAUSER_ROLE")),
            DEFAULT_ADMIN_ROLE
        );
        assertNotEq(
            tychoRouter.getRoleAdmin(EXECUTOR_SETTER_ROLE), DEFAULT_ADMIN_ROLE
        );
        assertNotEq(
            tychoRouter.getRoleAdmin(keccak256("ROUTER_FEE_SETTER_ROLE")),
            DEFAULT_ADMIN_ROLE
        );
    }
}
