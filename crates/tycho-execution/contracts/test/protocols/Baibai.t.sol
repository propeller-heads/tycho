// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {TychoRouterTestSetup} from "../TychoRouterTestSetup.sol";
import {TychoRouter__NegativeSlippage} from "@src/TychoRouterV3.sol";
import {BaibaiExecutor} from "@src/executors/BaibaiExecutor.sol";
import {TransferManager} from "@src/TransferManager.sol";

/// @dev `quoteFor` is from BaiBai's taker documentation (docs.baibai.cx/takers/quoting-and-swapping).
///      The others were identified on the live, not source-verified implementations behind the
///      proxies: `setTakerFee` by its selector and its effect on `quoteFor`.
interface IBaibaiFees {
    function owner() external view returns (address);
    function setTakerFee(address taker, address base, uint16 bps) external;
    function quoteFor(
        address base,
        address tokenIn,
        uint256 amountIn,
        address taker
    ) external view returns (uint256 amountOut, uint256 fee);
    function prioritySurchargeBps() external view returns (uint16);
}

/// @dev Identified by selector on the live CurveBook implementation, not source-verified.
interface IBaibaiCurveBook {
    function updatePriorityFee(address base) external view returns (uint96);
}

/// @dev The venue's revert for a curve past its expiry, from BaiBai's taker documentation.
error CurveStale();

contract BaibaiTest is TychoRouterTestSetup {
    BaibaiExecutor public baibaiExecutor;
    address internal constant BAIBAI_ENTRYPOINT =
        0x98c1D9E102Eb2806D902b13186BDc7892aC4fFBa;
    address constant CURVE_BOOK = 0x604d9b9eB1e1571C78661a6C1088427EC9c8c6E5;
    address constant CUSTODIAN = 0xAaC48FEB93c5C97E0fb3c7C57E1633922A4ACDa3;
    address constant BASE = 0x4200000000000000000000000000000000000006;
    address constant QUOTE = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant CBBTC = 0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf;
    /// @dev Where the executor is placed: derived from its name, so deployments the shared
    ///      setup adds do not move it, and the Rust-encoded calldata
    ///      (config/test_executor_addresses.json) keeps addressing it.
    address constant BAIBAI_EXECUTOR =
        address(uint160(uint256(keccak256("tycho.test.BaibaiExecutor"))));

    function setUp() public override {
        super.setUp();
        deployCodeTo(
            "BaibaiExecutor.sol:BaibaiExecutor",
            abi.encode(BAIBAI_ENTRYPOINT),
            BAIBAI_EXECUTOR
        );
        baibaiExecutor = BaibaiExecutor(BAIBAI_EXECUTOR);
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
        // After the CurveBook (52963996) and entrypoint (53137146) upgrades; the block the
        // simulator's fixture was recorded at.
        return 53234728;
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

    /// @dev An entrypoint that names no quote token cannot route a swap either way.
    function testEntrypointWithoutQuoteTokenIsRefused() public {
        vm.mockCall(
            BAIBAI_ENTRYPOINT,
            abi.encodeWithSignature("quoteToken()"),
            abi.encode(address(0))
        );
        vm.expectRevert(BaibaiExecutor.BaibaiExecutor__ZeroQuoteToken.selector);
        new BaibaiExecutor(BAIBAI_ENTRYPOINT);
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

    /// @dev A router swap that fills at exactly the entrypoint's gas-free quote.
    function _swap(bool sellBase, uint256 amount)
        internal
        returns (uint256 received)
    {
        (uint256 expected,) = IBaibaiFees(BAIBAI_ENTRYPOINT)
            .quoteFor(BASE, sellBase ? BASE : QUOTE, amount, tychoRouterAddr);
        assertGt(expected, 0);
        received = _swapUnchecked(sellBase, amount);
        assertEq(received, expected);
    }

    /// @dev A router swap through the Rust-encoded calldata, checking that the receiver gets
    ///      what the router reports and nothing is left at the router, whatever the price.
    function _swapUnchecked(bool sellBase, uint256 amount)
        internal
        returns (uint256 received)
    {
        address input = sellBase ? BASE : QUOTE;
        address output = sellBase ? QUOTE : BASE;
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
        received = abi.decode(result, (uint256));
        vm.stopPrank();
        assertEq(IERC20(output).balanceOf(BOB) - beforeBalance, received);
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

    /// @dev The fee charged is the router's, whatever the venue sets it to; end-user fees do
    ///      not apply.
    function testRouterFeeIdentity() public {
        _setFee(BOB, 1000);
        _setFee(tychoRouterAddr, 0);
        _swap(true, 1e16);
        _swap(false, 10e6);
        _setFee(tychoRouterAddr, 1000);
        _swap(true, 1e16);
        _swap(false, 10e6);
    }

    /// @dev The route's existing minimum output rejects a fee increase after quoting.
    function testFeeIncreaseRespectsMinOutput() public {
        (uint256 beforeFee,) = IBaibaiFees(BAIBAI_ENTRYPOINT)
            .quoteFor(BASE, BASE, 1e16, tychoRouterAddr);
        _setFee(tychoRouterAddr, 1000);
        _assertSellReverts(beforeFee, TychoRouter__NegativeSlippage.selector);
    }

    /// @dev BaiBai as the second hop: the router approves the entrypoint for the USDC the first
    ///      BaiBai hop paid it, and each hop fills at its gas-free quote.
    function testSequentialRouteThroughTwoPairs() public {
        uint256 amount = 1e16;
        (uint256 usdc,) = IBaibaiFees(BAIBAI_ENTRYPOINT)
            .quoteFor(BASE, BASE, amount, tychoRouterAddr);
        (uint256 expected,) = IBaibaiFees(BAIBAI_ENTRYPOINT)
            .quoteFor(CBBTC, QUOTE, usdc, tychoRouterAddr);
        assertGt(expected, 0);
        deal(BASE, BOB, amount);
        uint256 beforeBalance = IERC20(CBBTC).balanceOf(BOB);
        vm.startPrank(BOB);
        IERC20(BASE).approve(tychoRouterAddr, amount);
        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile("baibai_sequential_weth_to_cbbtc")
        );
        vm.stopPrank();
        assertTrue(success, "Rust-encoded BaiBai route failed");
        assertEq(IERC20(CBBTC).balanceOf(BOB) - beforeBalance, expected);
        assertEq(IERC20(BASE).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(QUOTE).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(CBBTC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE).allowance(tychoRouterAddr, BAIBAI_ENTRYPOINT), 0);
        assertEq(IERC20(QUOTE).allowance(tychoRouterAddr, BAIBAI_ENTRYPOINT), 0);
    }

    /// @dev A swap whose gas price exceeds the greater of the entrypoint's surcharge floor and
    ///      the pair's last update fee pays the priority surcharge; at that price or below it
    ///      fills at the gas-free quote the simulator reproduces. The rule was measured on the
    ///      unverified implementation, not read from source.
    function testPrioritySurchargeAboveTheGasPriceThreshold() public {
        uint256 updateFee = IBaibaiCurveBook(CURVE_BOOK).updatePriorityFee(BASE);
        // An unnamed entrypoint view (selector 0xc256865c), 8e6 wei at this block.
        (bool ok, bytes memory raw) =
            BAIBAI_ENTRYPOINT.staticcall(abi.encodeWithSelector(0xc256865c));
        assertTrue(ok);
        uint256 floor = abi.decode(raw, (uint256));
        uint256 threshold = floor > updateFee ? floor : updateFee;
        uint256 bps = IBaibaiFees(BAIBAI_ENTRYPOINT).prioritySurchargeBps();
        assertGt(bps, 0);
        for (uint8 sell; sell < 2; ++sell) {
            bool sellBase = sell == 1;
            uint256 amount = sellBase ? 1e16 : 10e6;
            address output = sellBase ? QUOTE : BASE;
            (uint256 gasFree, uint256 fee) = IBaibaiFees(BAIBAI_ENTRYPOINT)
                .quoteFor(
                    BASE, sellBase ? BASE : QUOTE, amount, tychoRouterAddr
                );
            uint256 gross = gasFree + fee;
            uint256 surcharged = gasFree - (gross * bps + 9_999) / 10_000;
            assertLt(surcharged, gasFree);

            uint256 snapshot = vm.snapshotState();
            vm.txGasPrice(threshold);
            assertEq(_swap(sellBase, amount), gasFree);
            vm.revertToState(snapshot);

            vm.txGasPrice(threshold + 1);
            uint256 custodyBefore = IERC20(output).balanceOf(CUSTODIAN);
            assertEq(_swapUnchecked(sellBase, amount), surcharged);
            // The surcharge stays in custody.
            assertEq(
                custodyBefore - IERC20(output).balanceOf(CUSTODIAN), surcharged
            );
            vm.revertToState(snapshot);
        }
        // A route quoted at the gas-free price reverts instead of filling short.
        (uint256 sellQuote,) = IBaibaiFees(BAIBAI_ENTRYPOINT)
            .quoteFor(BASE, BASE, 1e16, tychoRouterAddr);
        vm.txGasPrice(threshold + 1);
        _assertSellReverts(sellQuote, TychoRouter__NegativeSlippage.selector);
    }

    /// @dev Expiry reverts before any custody balance or router approval persists.
    function testExpiredSwapReverts() public {
        // The pinned curve has an eight-second TTL.
        vm.warp(block.timestamp + 9);
        _assertSellReverts(1, CurveStale.selector);
    }

    function _assertSellReverts(uint256 minOutput, bytes4 reason) internal {
        deal(BASE, BOB, 1e16);
        vm.startPrank(BOB);
        IERC20(BASE).approve(tychoRouterAddr, 1e16);
        bytes memory swap = encodeSingleSwap(
            address(baibaiExecutor), abi.encodePacked(BASE, uint8(1))
        );
        uint256 custodyBefore = IERC20(BASE).balanceOf(CUSTODIAN);
        vm.expectPartialRevert(reason);
        tychoRouter.singleSwap(
            1e16, BASE, QUOTE, minOutput, minOutput, BOB, noClientFee(), swap
        );
        vm.stopPrank();
        assertEq(IERC20(BASE).balanceOf(BOB), 1e16);
        assertEq(IERC20(BASE).balanceOf(CUSTODIAN), custodyBefore);
        assertEq(IERC20(BASE).allowance(tychoRouterAddr, BAIBAI_ENTRYPOINT), 0);
    }
}
