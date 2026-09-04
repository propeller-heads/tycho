pragma solidity ^0.8.26;

import {
    TychoRouterV3,
    TychoRouter__NoPendingFeeCalculator
} from "@src/TychoRouterV3.sol";
import {FeeCalculator} from "@src/FeeCalculator.sol";
import {
    IAccessControl
} from "@openzeppelin/contracts/access/IAccessControl.sol";
import "@src/TransferManager.sol";
import "./TychoRouterTestSetup.sol";
import {Vault, Vault__Paused} from "@src/Vault.sol";
import {
    Dispatcher__ExecutorAlreadyExists,
    Dispatcher__ExecutorIsTimelocked,
    Dispatcher__NonContractExecutor
} from "@src/Dispatcher.sol";
import {ExecutorDeployment} from "./ExecutorDeployment.sol";
import {ALICE, DAI_ADDR, PERMIT2_ADDRESS, WETH_ADDR} from "./Constants.sol";
import {
    DAI_WETH_UNIV2_POOL,
    UniswapV2Deployment
} from "./deployments/UniswapV2.sol";

contract TychoRouterTest is TychoRouterTestSetup, UniswapV2Deployment {
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

    function testPauseVault() public {
        assertEq(tychoRouter.vaultPaused(), false);

        vm.expectEmit(tychoRouterAddr);
        emit Vault.VaultPaused(PAUSER);
        vm.prank(PAUSER);
        tychoRouter.pauseVault();
        assertEq(tychoRouter.vaultPaused(), true);
        assertEq(tychoRouter.paused(), false);

        vm.expectEmit(tychoRouterAddr);
        emit Vault.VaultUnpaused(UNPAUSER);
        vm.prank(UNPAUSER);
        tychoRouter.unpauseVault();
        assertEq(tychoRouter.vaultPaused(), false);
    }

    function testPauseVaultNonRole() public {
        vm.startPrank(BOB);
        vm.expectRevert();
        tychoRouter.pauseVault();
        vm.expectRevert();
        tychoRouter.unpauseVault();
        vm.stopPrank();

        // Each role only holds its own direction
        vm.prank(UNPAUSER);
        vm.expectRevert();
        tychoRouter.pauseVault();
        vm.prank(PAUSER);
        tychoRouter.pauseVault();
        vm.prank(PAUSER);
        vm.expectRevert();
        tychoRouter.unpauseVault();
    }

    function testEmptySwapsRevert() public {
        uint256 amountIn = 10 ** 18;
        bytes memory swaps = "";
        vm.expectRevert(TychoRouter__EmptySwaps.selector);
        tychoRouter.exposedSplitSwap(amountIn, 2, swaps, address(0), false);
    }

    // FEE CALCULATOR TESTS
    function testSetFeeCalculatorQueuesPending() public {
        FeeCalculator newCalc = new FeeCalculator(FEE_SETTER, routerFeeReceiver);
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
        FeeCalculator newCalc = new FeeCalculator(FEE_SETTER, routerFeeReceiver);
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
        FeeCalculator newCalc = new FeeCalculator(FEE_SETTER, routerFeeReceiver);
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
        FeeCalculator calc1 = new FeeCalculator(FEE_SETTER, routerFeeReceiver);
        FeeCalculator calc2 = new FeeCalculator(FEE_SETTER, routerFeeReceiver);

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
            new address[](0),
            listTestPausers(),
            UNPAUSER,
            EXECUTOR_SETTER,
            FEE_SETTER
        );
    }

    function testConstructorNonContractPermit2() public {
        // Deploy a new FeeCalculator contract
        FeeCalculator newFeeCalculator =
            new FeeCalculator(FEE_SETTER, routerFeeReceiver);
        address nonContract = address(0x999);
        vm.expectRevert(
            abi.encodeWithSelector(
                TransferManager__NotAContract.selector, nonContract
            )
        );
        new TychoRouterExposed(
            nonContract,
            address(newFeeCalculator),
            new address[](0),
            listTestPausers(),
            UNPAUSER,
            EXECUTOR_SETTER,
            FEE_SETTER
        );
    }

    function testConstructorPausesVault() public {
        TychoRouterExposed router = new TychoRouterExposed(
            PERMIT2_ADDRESS,
            address(feeCalculator),
            new address[](0),
            listTestPausers(),
            UNPAUSER,
            EXECUTOR_SETTER,
            FEE_SETTER
        );
        assertTrue(router.vaultPaused());

        vm.deal(ALICE, 1 ether);
        vm.prank(ALICE);
        vm.expectRevert(Vault__Paused.selector);
        router.deposit{value: 1 ether}(ETH_ADDR, 1 ether);

        vm.prank(UNPAUSER);
        router.unpauseVault();
        assertFalse(router.vaultPaused());
    }

    function testConstructorPausersCanPause() public {
        address[] memory pausers = new address[](2);
        pausers[0] = makeAddr("pauserA");
        pausers[1] = makeAddr("pauserB");
        TychoRouterExposed router = new TychoRouterExposed(
            PERMIT2_ADDRESS,
            address(feeCalculator),
            new address[](0),
            pausers,
            UNPAUSER,
            EXECUTOR_SETTER,
            FEE_SETTER
        );

        assertFalse(router.hasRole(PAUSER_ROLE, UNPAUSER));
        assertTrue(router.hasRole(PAUSER_ROLE, pausers[0]));
        assertTrue(router.hasRole(PAUSER_ROLE, pausers[1]));

        vm.prank(pausers[1]);
        router.pause();
        assertTrue(router.paused());
    }

    function testPauserCannotGrantPauserRole() public {
        address newPauser = makeAddr("newPauser");
        vm.prank(PAUSER);
        vm.expectRevert(
            abi.encodeWithSelector(
                IAccessControl.AccessControlUnauthorizedAccount.selector,
                PAUSER,
                keccak256("UNPAUSER_ROLE")
            )
        );
        tychoRouter.grantRole(PAUSER_ROLE, newPauser);
    }

    function testUnpauserManagesPauserRole() public {
        address newPauser = makeAddr("newPauser");

        vm.startPrank(UNPAUSER);
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

    // ==================== Deployment executors ====================
    function _deployRouterWithExecutors(address[] memory executors)
        internal
        returns (TychoRouterExposed)
    {
        return new TychoRouterExposed(
            PERMIT2_ADDRESS,
            address(feeCalculator),
            executors,
            listTestPausers(),
            UNPAUSER,
            EXECUTOR_SETTER,
            FEE_SETTER
        );
    }

    function _swapWethForDai(TychoRouterExposed router)
        internal
        returns (uint256)
    {
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        bytes memory swap = encodeSingleSwap(
            uniswapV2Executor,
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(address(router), amountIn);
        uint256 amountOut = router.singleSwap(
            amountIn, WETH_ADDR, DAI_ADDR, 1, 1, ALICE, noClientFee(), swap
        );
        vm.stopPrank();
        return amountOut;
    }

    function testDeploymentExecutorsAndFeeCalculatorActive() public {
        address[] memory executors = new address[](1);
        executors[0] = uniswapV2Executor;
        TychoRouterExposed router = _deployRouterWithExecutors(executors);

        // No warp: the executor and the fee calculator work at deployment
        assertEq(router.getFeeCalculator(), address(feeCalculator));
        assertGt(_swapWethForDai(router), 0);
    }

    function testExecutorAddedAfterDeploymentIsTimelocked() public {
        TychoRouterExposed router = _deployRouterWithExecutors(new address[](0));
        address[] memory executors = new address[](1);
        executors[0] = uniswapV2Executor;
        vm.prank(EXECUTOR_SETTER);
        router.setExecutors(executors);

        bytes memory swap = encodeSingleSwap(
            uniswapV2Executor,
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );
        vm.expectRevert(
            abi.encodeWithSelector(
                Dispatcher__ExecutorIsTimelocked.selector, uniswapV2Executor
            )
        );
        router.singleSwap(
            1 ether, WETH_ADDR, DAI_ADDR, 1, 1, ALICE, noClientFee(), swap
        );

        vm.warp(vm.getBlockTimestamp() + router.DELAY_EXECUTOR_ACTIVATION());
        assertGt(_swapWethForDai(router), 0);
    }

    function testConstructorNonContractExecutor() public {
        address[] memory executors = new address[](1);
        executors[0] = address(0x999);
        vm.expectRevert(Dispatcher__NonContractExecutor.selector);
        _deployRouterWithExecutors(executors);
    }

    function testConstructorDuplicateExecutor() public {
        address[] memory executors = new address[](2);
        executors[0] = uniswapV2Executor;
        executors[1] = uniswapV2Executor;
        vm.expectRevert(
            abi.encodeWithSelector(
                Dispatcher__ExecutorAlreadyExists.selector, uniswapV2Executor
            )
        );
        _deployRouterWithExecutors(executors);
    }
}
