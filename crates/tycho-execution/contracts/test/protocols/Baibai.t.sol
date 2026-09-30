// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {TychoRouterTestSetup} from "../TychoRouterTestSetup.sol";
import {BaibaiExecutor} from "@src/executors/BaibaiExecutor.sol";
import {TransferManager} from "@src/TransferManager.sol";

interface IBaibaiFees {
    function owner() external view returns (address);
    function setTakerFee(address taker, address base, uint16 bps) external;
    function quoteFor(
        address base,
        address tokenIn,
        uint256 amountIn,
        address taker
    ) external view returns (uint256 amountOut, uint256 fee);
}

contract BaibaiTest is TychoRouterTestSetup {
    BaibaiExecutor public baibaiExecutor;
    address internal constant BAIBAI_ENTRYPOINT =
        0x98c1D9E102Eb2806D902b13186BDc7892aC4fFBa;
    address constant CUSTODIAN = 0xAaC48FEB93c5C97E0fb3c7C57E1633922A4ACDa3;
    address constant BASE = 0x4200000000000000000000000000000000000006;
    address constant QUOTE = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;

    function setUp() public override {
        super.setUp();
        baibaiExecutor = new BaibaiExecutor(BAIBAI_ENTRYPOINT);
        address[] memory executors = new address[](1);
        executors[0] = address(baibaiExecutor);
        // Register before the pinned timestamp so activation does not expire the curve.
        vm.warp(forkTimestamp - _SETUP_TIME_OFFSET_NEW_EXECUTOR);
        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.warp(forkTimestamp);
    }

    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 51191196;
    }

    /// @dev Output token and debit destination are derived for both swap directions.
    function testTransferData() public view {
        for (uint8 sell; sell < 2; ++sell) {
            (
                TransferManager.TransferType kind,
                address receiver,
                address tokenIn,
                address tokenOut,
                bool outputToRouter
            ) = baibaiExecutor.getTransferData(abi.encodePacked(BASE, sell));
            assertEq(
                uint8(kind),
                uint8(TransferManager.TransferType.ProtocolWillDebit)
            );
            assertEq(receiver, BAIBAI_ENTRYPOINT);
            assertEq(tokenIn, sell == 1 ? BASE : QUOTE);
            assertEq(tokenOut, sell == 1 ? QUOTE : BASE);
            assertFalse(outputToRouter);
        }
        assertEq(baibaiExecutor.fundsExpectedAddress(""), address(this));
    }

    /// @dev Rejects truncation, extra bytes, nonboolean direction and invalid base tokens.
    function testInvalidData() public {
        bytes[5] memory invalid = [
            bytes(""),
            abi.encodePacked(BASE),
            abi.encodePacked(BASE, uint16(1)),
            abi.encodePacked(BASE, uint8(2)),
            abi.encodePacked(QUOTE, uint8(0))
        ];
        for (uint256 i; i < invalid.length; ++i) {
            vm.expectRevert(BaibaiExecutor.BaibaiExecutor__InvalidData.selector);
            baibaiExecutor.getTransferData(invalid[i]);
        }
        vm.expectRevert(BaibaiExecutor.BaibaiExecutor__InvalidData.selector);
        baibaiExecutor.getTransferData(abi.encodePacked(address(0), uint8(1)));
    }

    function _setFee(address taker, uint16 bps) internal {
        vm.prank(IBaibaiFees(BAIBAI_ENTRYPOINT).owner());
        IBaibaiFees(BAIBAI_ENTRYPOINT).setTakerFee(taker, BASE, bps);
    }

    function _swap(bool sellBase, uint256 amount) internal {
        address input = sellBase ? BASE : QUOTE;
        address output = sellBase ? QUOTE : BASE;
        (uint256 expected,) = IBaibaiFees(BAIBAI_ENTRYPOINT)
            .quoteFor(BASE, input, amount, tychoRouterAddr);
        assertGt(expected, 0);
        deal(input, BOB, amount);
        uint256 beforeBalance = IERC20(output).balanceOf(BOB);
        uint256 custodyBefore = IERC20(input).balanceOf(CUSTODIAN);
        vm.startPrank(BOB);
        IERC20(input).approve(tychoRouterAddr, amount);
        bytes memory callData = loadCallDataFromFile(
            sellBase ? "baibai_sell_base" : "baibai_buy_base"
        );
        (bool success, bytes memory result) = tychoRouterAddr.call(callData);
        assertTrue(success, "Rust-encoded BaiBai swap failed");
        uint256 received = abi.decode(result, (uint256));
        vm.stopPrank();
        assertEq(received, expected);
        assertEq(IERC20(output).balanceOf(BOB) - beforeBalance, expected);
        assertEq(IERC20(input).balanceOf(BOB), 0);
        assertEq(IERC20(input).balanceOf(CUSTODIAN) - custodyBefore, amount);
        assertEq(IERC20(input).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(output).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(input).allowance(tychoRouterAddr, BAIBAI_ENTRYPOINT), 0);
    }

    /// @dev Both directions pass through the real router, entrypoint and custodian.
    function testRouterBothDirections() public {
        _swap(true, 1e16);
        _swap(false, 10e6);
    }

    /// @dev The router's fee applies in both directions; end-user fees do not.
    function testRouterFeeIdentity() public {
        _setFee(BOB, 1000);
        _setFee(tychoRouterAddr, 25);
        _swap(true, 1e16);
        _swap(false, 10e6);
    }

    /// @dev The route's existing minimum output rejects a fee increase after quoting.
    function testFeeIncreaseRespectsMinOutput() public {
        (uint256 beforeFee,) = IBaibaiFees(BAIBAI_ENTRYPOINT)
            .quoteFor(BASE, BASE, 1e16, tychoRouterAddr);
        _setFee(tychoRouterAddr, 25);
        _assertSellReverts(beforeFee);
    }

    /// @dev Expiry reverts before any custody balance or router approval persists.
    function testExpiredSwapReverts() public {
        // The pinned curve has an eight-second TTL.
        vm.warp(block.timestamp + 9);
        _assertSellReverts(1);
    }

    function _assertSellReverts(uint256 minOutput) internal {
        deal(BASE, BOB, 1e16);
        vm.startPrank(BOB);
        IERC20(BASE).approve(tychoRouterAddr, 1e16);
        bytes memory swap = encodeSingleSwap(
            address(baibaiExecutor), abi.encodePacked(BASE, uint8(1))
        );
        uint256 custodyBefore = IERC20(BASE).balanceOf(CUSTODIAN);
        vm.expectRevert();
        tychoRouter.singleSwap(
            1e16, BASE, QUOTE, minOutput, minOutput, BOB, noClientFee(), swap
        );
        vm.stopPrank();
        assertEq(IERC20(BASE).balanceOf(BOB), 1e16);
        assertEq(IERC20(BASE).balanceOf(CUSTODIAN), custodyBefore);
        assertEq(IERC20(BASE).allowance(tychoRouterAddr, BAIBAI_ENTRYPOINT), 0);
    }
}
