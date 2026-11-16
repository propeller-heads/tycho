pragma solidity ^0.8.26;

import "@src/Vault.sol";
import {TestUtils} from "./TestUtils.sol";
import {Constants} from "./Constants.sol";

contract VaultExposed is Vault {
    function updateDeltaAccountingFrom(
        address token,
        int256 initialDelta,
        uint256 initialNonZeroCount,
        int256 change
    )
        external
        returns (int256 deltaBefore, int256 deltaAfter, uint256 nonZeroCount)
    {
        _setDelta(token, initialDelta);
        _setNonZeroDeltaCount(initialNonZeroCount);
        deltaBefore = _getDelta(token);
        _updateDeltaAccounting(token, change);
        deltaAfter = _getDelta(token);
        nonZeroCount = _getNonZeroDeltaCount();
    }

    function creditVault(address user, address token, uint256 amount) external {
        _creditVault(user, token, amount);
    }

    function debitVault(address user, address token, uint256 amount) external {
        _debitVault(user, token, amount);
    }

    function finalizeBalancesWith(
        bool useVault,
        uint256 nonZeroCount,
        int256 inputDelta,
        address user,
        address inputToken,
        uint256 inputAmount
    ) external {
        TransientSlot.tstore(_USE_VAULT_SLOT, useVault);
        _setNonZeroDeltaCount(nonZeroCount);
        _setDelta(inputToken, inputDelta);
        _finalizeBalances(user, inputToken, inputAmount);
    }
}

contract VaultTest is Constants, TestUtils {
    VaultExposed public vault;

    function setUp() public {
        uint256 forkBlock = 24218176;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        vault = new VaultExposed();
    }

    function testDepositERC20() public {
        uint256 amount = 1_000_000_000;
        deal(USDC_ADDR, ALICE, amount);
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(address(vault), amount);
        vault.deposit(USDC_ADDR, amount);

        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(vault)), amount);
        assertEq(vault.balanceOf(ALICE, uint256(uint160(USDC_ADDR))), amount);
    }

    function testDepositETH() public {
        uint256 amount = 1 ether;
        deal(ALICE, amount);
        uint256 balanceBefore = address(vault).balance;
        vm.startPrank(ALICE);
        vault.deposit{value: amount}(ETH_ADDR, amount);

        assertEq(ALICE.balance, 0);
        assertEq(address(vault).balance - balanceBefore, amount);
        assertEq(vault.balanceOf(ALICE, uint256(uint160(ETH_ADDR))), amount);
    }

    function testWithdrawERC20() public {
        uint256 amount = 1_000_000_000;
        deal(USDC_ADDR, ALICE, amount);
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(address(vault), amount);
        vault.deposit(USDC_ADDR, amount);

        uint256 amountToWithdraw = amount / 2;
        vault.withdraw(USDC_ADDR, amountToWithdraw);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), amountToWithdraw);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(vault)), amountToWithdraw);
        assertEq(
            vault.balanceOf(ALICE, uint256(uint160(USDC_ADDR))),
            amountToWithdraw
        );
    }

    function testWithdrawETH() public {
        address user = makeAddr("brand-new-user");
        uint256 amount = 1 ether;
        deal(user, amount);
        uint256 balanceBefore = address(vault).balance;
        vm.startPrank(user);
        vault.deposit{value: amount}(ETH_ADDR, amount);

        uint256 amountToWithdraw = amount / 2;
        vault.withdraw(ETH_ADDR, amountToWithdraw);

        assertEq(user.balance, amountToWithdraw);
        assertEq(address(vault).balance - balanceBefore, amountToWithdraw);
        assertEq(
            vault.balanceOf(user, uint256(uint160(ETH_ADDR))), amountToWithdraw
        );
    }

    function testUpdateDeltaIncreasePositiveToPositive() public {
        address token = address(0);
        int256 initialDelta = 100;
        int256 change = 200;
        uint256 nonZeroCountInitial = 1;

        (int256 beforeChange, int256 afterChange, uint256 nonZeroCount) = vault.updateDeltaAccountingFrom(
            token, initialDelta, nonZeroCountInitial, change
        );

        assertEq(beforeChange, 100);
        assertEq(afterChange, 300);
        assertEq(nonZeroCount, 1);
    }

    function testUpdateDeltaIncreaseNegativeToNegative() public {
        address token = address(0);
        int256 initialDelta = -100;
        int256 change = 15;
        uint256 nonZeroDeltaInitial = 1;

        (int256 beforeChange, int256 afterChange, uint256 nonZeroCount) = vault.updateDeltaAccountingFrom(
            token, initialDelta, nonZeroDeltaInitial, change
        );

        assertEq(beforeChange, -100);
        assertEq(afterChange, -85);
        assertEq(nonZeroCount, 1);
    }

    function testUpdateDeltaIncreaseNegativeToPositive() public {
        address token = address(0);
        int256 initialDelta = -100;
        int256 change = 200;
        uint256 nonZeroCountInitial = 1;

        (int256 beforeChange, int256 afterChange, uint256 nonZeroCount) = vault.updateDeltaAccountingFrom(
            token, initialDelta, nonZeroCountInitial, change
        );

        assertEq(beforeChange, -100);
        assertEq(afterChange, 100);
        assertEq(nonZeroCount, 1);
    }

    function testUpdateDeltaDecreasePositiveToPositive() public {
        address token = address(0);
        int256 initialDelta = 300;
        int256 change = -100;
        uint256 nonZeroCountInitial = 1;

        (int256 beforeChange, int256 afterChange, uint256 nonZeroCount) = vault.updateDeltaAccountingFrom(
            token, initialDelta, nonZeroCountInitial, change
        );

        assertEq(beforeChange, 300);
        assertEq(afterChange, 200);
        assertEq(nonZeroCount, 1);
    }

    function testUpdateDeltaPositiveToZero() public {
        address token = address(0);
        int256 initialDelta = 50;
        int256 change = -50;
        uint256 nonZeroCountInitial = 1;

        (int256 beforeChange, int256 afterChange, uint256 nonZeroCount) = vault.updateDeltaAccountingFrom(
            token, initialDelta, nonZeroCountInitial, change
        );

        assertEq(beforeChange, 50);
        assertEq(afterChange, 0);
        assertEq(nonZeroCount, 0);
    }

    function testUpdateDeltaNegativeToZero() public {
        address token = address(0);
        int256 initialDelta = -50;
        int256 change = 50;
        uint256 nonZeroCountInitial = 1;

        (int256 beforeChange, int256 afterChange, uint256 nonZeroCount) = vault.updateDeltaAccountingFrom(
            token, initialDelta, nonZeroCountInitial, change
        );

        assertEq(beforeChange, -50);
        assertEq(afterChange, 0);
        assertEq(nonZeroCount, 0);
    }

    function testCreditVault() public {
        uint256 amount = 1_000_000_000;

        uint256 id = uint256(uint160(USDC_ADDR));

        uint256 balanceBefore = vault.balanceOf(BOB, id);
        vault.creditVault(BOB, USDC_ADDR, amount);

        uint256 balance = vault.balanceOf(BOB, id);

        assertEq(balance, amount);
        assertEq(balance - balanceBefore, amount);
    }

    function testCreditVaultNonEmpty() public {
        uint256 amount = 1_000_000_000;

        uint256 id = uint256(uint160(USDC_ADDR));

        vault.creditVault(BOB, USDC_ADDR, amount);

        uint256 balanceBefore = vault.balanceOf(BOB, id);
        vault.creditVault(BOB, USDC_ADDR, amount);

        uint256 balance = vault.balanceOf(BOB, id);

        assertEq(balance, 2_000_000_000);
        assertEq(balance - balanceBefore, amount);
    }

    function testDebitVault() public {
        uint256 amount = 1_000_000_000;
        uint256 amount_to_debit = 2_000_000;

        uint256 id = uint256(uint160(USDC_ADDR));

        vault.creditVault(BOB, USDC_ADDR, amount);

        uint256 balanceBefore = vault.balanceOf(BOB, id);
        vault.debitVault(BOB, USDC_ADDR, amount_to_debit);

        uint256 balance = vault.balanceOf(BOB, id);

        assertEq(balance, 998_000_000);
        assertEq(balanceBefore - balance, amount_to_debit);
    }

    function testDebitVaultTooHigh() public {
        uint256 amount = 900_000_000;
        uint256 amount_to_debit = 1_000_000_000;

        uint256 id = uint256(uint160(USDC_ADDR));

        vault.creditVault(BOB, USDC_ADDR, amount);

        uint256 balanceBefore = vault.balanceOf(BOB, id);

        vm.expectRevert(
            abi.encodeWithSelector(
                Vault__InsufficientBalance.selector,
                BOB,
                USDC_ADDR,
                amount_to_debit,
                balanceBefore
            )
        );

        vault.debitVault(BOB, USDC_ADDR, amount_to_debit);
    }

    function testFinalizeBalancesNonZeroCountTooHigh() public {
        uint256 inputAmount = 1_000_000;
        uint256 nonZeroCount = 3;

        vm.expectRevert(
            abi.encodeWithSelector(
                Vault__UnexpectedNonZeroCount.selector, nonZeroCount
            )
        );

        vault.finalizeBalancesWith(
            false, nonZeroCount, 0, BOB, USDC_ADDR, inputAmount
        );
    }

    function testFinalizeBalancesNonZeroCountZero() public {
        uint256 inputAmount = 1_000_000;
        uint256 nonZeroCount = 0;

        vault.creditVault(BOB, USDC_ADDR, 3_000_000);

        uint256 balanceStart = vault.balanceOf(BOB, uint256(uint160(USDC_ADDR)));

        vault.finalizeBalancesWith(
            false, nonZeroCount, 0, BOB, USDC_ADDR, inputAmount
        );

        uint256 balanceEnd = vault.balanceOf(BOB, uint256(uint160(USDC_ADDR)));

        assertEq(balanceStart, balanceEnd);
    }

    function testFinalizeBalancesInputAmountDoesNotMatchDelta() public {
        uint256 inputAmount = 1_000_000;
        int256 inputDelta = -2_000_000;
        uint256 nonZeroCount = 1;

        vault.creditVault(BOB, USDC_ADDR, 3_000_000);

        vm.expectRevert(
            abi.encodeWithSelector(
                Vault__UnexpectedInputDelta.selector, inputDelta
            )
        );

        vault.finalizeBalancesWith(
            true, nonZeroCount, inputDelta, BOB, USDC_ADDR, inputAmount
        );
    }

    function testFinalizeBalancesZeroDeltaReverts() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                Vault__UnexpectedInputDelta.selector, int256(0)
            )
        );

        vault.finalizeBalancesWith(true, 1, 0, BOB, USDC_ADDR, 0);
    }

    function testFinalizeBalancesSuccess() public {
        uint256 inputAmount = 2_000_000;
        int256 inputDelta = -2_000_000;
        uint256 nonZeroCount = 1;

        vault.creditVault(BOB, USDC_ADDR, 3_000_000);
        uint256 balanceStart = vault.balanceOf(BOB, uint256(uint160(USDC_ADDR)));

        vault.finalizeBalancesWith(
            true, nonZeroCount, inputDelta, BOB, USDC_ADDR, inputAmount
        );

        uint256 balanceEnd = vault.balanceOf(BOB, uint256(uint160(USDC_ADDR)));

        assertEq(balanceStart - balanceEnd, inputAmount);
        assertEq(balanceEnd, 1_000_000);
    }

    function testDepositFeeOnTransferToken() public {
        // TWIF charges 6% on every transfer
        address TWIF = 0x2Dd636C514Bb4705c756D161585Ff9ec665f18A2;

        uint256 amount = 1_000_000;
        uint256 expectedReceived = amount - (amount * 6) / 100;

        deal(TWIF, ALICE, amount);
        vm.startPrank(ALICE);
        IERC20(TWIF).approve(address(vault), amount);
        vault.deposit(TWIF, amount);
        vm.stopPrank();

        // Vault credits only the actual received amount
        assertEq(
            vault.balanceOf(ALICE, uint256(uint160(TWIF))), expectedReceived
        );
        // Router holds exactly the credited amount
        assertEq(IERC20(TWIF).balanceOf(address(vault)), expectedReceived);
    }
}
