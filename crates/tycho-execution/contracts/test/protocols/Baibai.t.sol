// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {TychoRouterTestSetup} from "../TychoRouterTestSetup.sol";
import {TychoRouter__NegativeSlippage} from "@src/TychoRouterV3.sol";
import {BaibaiExecutor} from "@src/executors/BaibaiExecutor.sol";
import {TransferManager} from "@src/TransferManager.sol";

/// @dev `quoteFor` is from BaiBai's taker documentation
///      (docs.baibai.cx/takers/quoting-and-swapping). The others were identified on the live,
///      not source-verified implementations behind the proxies: `owner` and
///      `prioritySurchargeBps` by their selectors, `setTakerFee` by its selector and its effect
///      on `quoteFor`. Fees are in millionths of the gross output.
interface IBaibaiFees {
    function owner() external view returns (address);
    function setTakerFee(address taker, address base, uint16 pips) external;
    function quoteFor(
        address base,
        address tokenIn,
        uint256 amountIn,
        address taker
    ) external view returns (uint256 amountOut, uint256 fee);
    function prioritySurchargeBps() external view returns (uint16);
}

/// @dev The CurveBook getters, from BaiBai's taker documentation.
interface IBaibaiCurveBook {
    struct SideView {
        int16 spreadBps;
        uint16 depthBps;
        uint256 filled;
        uint8 knotCount;
    }

    struct PairView {
        uint64 seq;
        uint64 fillSeq;
        uint64 lastUpdateAt;
        uint80 mid;
        uint256 qUnit;
        SideView ask;
        SideView bid;
    }

    function pair(address base) external view returns (PairView memory);
    function validUntil(address base) external view returns (uint64);
    function ttl() external view returns (uint64);
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
        // After the Entrypoint upgrade at 53492231, and the block the simulator's fixture
        // live_53545000.json was recorded at. The router under test is a fresh deployment, so
        // the zero fee BaiBai set for Tycho's router (53359421) does not apply to it: it pays the
        // default 50 millionths unless a test sets its fee.
        return 53545000;
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

    function _setFee(address taker, uint16 pips) internal {
        vm.prank(IBaibaiFees(BAIBAI_ENTRYPOINT).owner());
        IBaibaiFees(BAIBAI_ENTRYPOINT).setTakerFee(taker, BASE, pips);
    }

    /// @dev A router swap that fills at exactly the entrypoint's surcharge-free quote.
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
            sellBase
                ? "test_single_encoding_strategy_baibai_sell_base"
                : "test_single_encoding_strategy_baibai_buy_base"
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
        _assertSellReverts(
            BASE, 1e16, beforeFee, TychoRouter__NegativeSlippage.selector
        );
    }

    /// @dev BaiBai as the second hop: the router approves the entrypoint for the USDC the first
    ///      BaiBai hop paid it, and each hop fills at its surcharge-free quote.
    function testSequentialRouteThroughTwoPairs() public {
        uint256 amount = 1e16;
        (uint256 usdc,) = IBaibaiFees(BAIBAI_ENTRYPOINT)
            .quoteFor(BASE, BASE, amount, tychoRouterAddr);
        (uint256 expected,) = IBaibaiFees(BAIBAI_ENTRYPOINT)
            .quoteFor(CBBTC, QUOTE, usdc, tychoRouterAddr);
        assertGt(expected, 0);
        deal(BASE, BOB, amount);
        uint256 beforeBalance = IERC20(CBBTC).balanceOf(BOB);
        uint256 wethCustodyBefore = IERC20(BASE).balanceOf(CUSTODIAN);
        uint256 usdcCustodyBefore = IERC20(QUOTE).balanceOf(CUSTODIAN);
        uint256 cbbtcCustodyBefore = IERC20(CBBTC).balanceOf(CUSTODIAN);
        vm.startPrank(BOB);
        IERC20(BASE).approve(tychoRouterAddr, amount);
        (bool success, bytes memory result) = tychoRouterAddr.call(
            loadCallDataFromFile("test_sequential_encoding_strategy_baibai")
        );
        vm.stopPrank();
        assertTrue(success, "Rust-encoded BaiBai route failed");
        assertEq(abi.decode(result, (uint256)), expected);
        assertEq(IERC20(CBBTC).balanceOf(BOB) - beforeBalance, expected);
        assertEq(IERC20(BASE).balanceOf(BOB), 0);
        // The first hop's USDC leaves custody and the second hop's comes straight back.
        assertEq(IERC20(BASE).balanceOf(CUSTODIAN) - wethCustodyBefore, amount);
        assertEq(IERC20(QUOTE).balanceOf(CUSTODIAN), usdcCustodyBefore);
        assertEq(
            cbbtcCustodyBefore - IERC20(CBBTC).balanceOf(CUSTODIAN), expected
        );
        assertEq(IERC20(BASE).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(QUOTE).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(CBBTC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE).allowance(tychoRouterAddr, BAIBAI_ENTRYPOINT), 0);
        assertEq(IERC20(QUOTE).allowance(tychoRouterAddr, BAIBAI_ENTRYPOINT), 0);
    }

    /// @dev An unnamed entrypoint view (selector 0xc256865c): the priority fee above which the
    ///      surcharge applies.
    function _surchargeThreshold() internal view returns (uint256) {
        (bool ok, bytes memory raw) =
            BAIBAI_ENTRYPOINT.staticcall(abi.encodeWithSelector(0xc256865c));
        assertTrue(ok);
        return abi.decode(raw, (uint256));
    }

    /// @dev A swap whose priority fee exceeds the threshold has a hundredth of a basis point of
    ///      its gross output withheld per 1e4 wei above it, up to `prioritySurchargeBps`, the
    ///      withheld amount rounded up; the basefee plays no part. At the threshold it fills at
    ///      the surcharge-free quote the simulator reproduces. The rule was measured on the
    ///      unverified implementation, not read from source. The threshold and cap are pinned to
    ///      their values at the fork block, which the simulator's module docs state.
    function testPrioritySurchargeScalesWithThePriorityFee() public {
        uint256 threshold = _surchargeThreshold();
        assertEq(threshold, 2e6, "surcharge threshold");
        uint256 capBps = IBaibaiFees(BAIBAI_ENTRYPOINT).prioritySurchargeBps();
        assertEq(capBps, 50, "surcharge cap");
        uint256 maxHundredths = capBps * 100;
        uint256[4] memory tips = [
            threshold,
            threshold + 1e6 + 9_999,
            threshold + maxHundredths * 1e4,
            threshold + maxHundredths * 1e4 + 1e9
        ];
        uint256[4] memory hundredths =
            [uint256(0), 100, maxHundredths, maxHundredths];
        uint256[2] memory basefees = [uint256(0), 5_000_000];
        for (uint8 sell; sell < 2; ++sell) {
            for (uint256 b; b < basefees.length; ++b) {
                for (uint256 t; t < tips.length; ++t) {
                    _assertSurcharge(
                        sell == 1, basefees[b], tips[t], hundredths[t]
                    );
                }
            }
        }
        // A route quoted at the surcharge-free price reverts instead of filling short.
        (uint256 sellQuote,) = IBaibaiFees(BAIBAI_ENTRYPOINT)
            .quoteFor(BASE, BASE, 1e16, tychoRouterAddr);
        vm.fee(5_000_000);
        vm.txGasPrice(5_000_000 + threshold + 1e6);
        _assertSellReverts(
            BASE, 1e16, sellQuote, TychoRouter__NegativeSlippage.selector
        );
    }

    /// @dev A swap sent at `basefee` plus `tip` has `hundredths` of a basis point of its gross
    ///      output withheld, rounded up, and the withheld amount stays in custody.
    function _assertSurcharge(
        bool sellBase,
        uint256 basefee,
        uint256 tip,
        uint256 hundredths
    ) internal {
        uint256 amount = sellBase ? 1e16 : 10e6;
        address output = sellBase ? QUOTE : BASE;
        (uint256 gasFree, uint256 fee) = IBaibaiFees(BAIBAI_ENTRYPOINT)
            .quoteFor(BASE, sellBase ? BASE : QUOTE, amount, tychoRouterAddr);
        uint256 expected =
            gasFree - ((gasFree + fee) * hundredths + 999_999) / 1e6;
        uint256 snapshot = vm.snapshotState();
        vm.fee(basefee);
        vm.txGasPrice(basefee + tip);
        uint256 custodyBefore = IERC20(output).balanceOf(CUSTODIAN);
        assertEq(_swapUnchecked(sellBase, amount), expected);
        assertEq(custodyBefore - IERC20(output).balanceOf(CUSTODIAN), expected);
        vm.revertToState(snapshot);
    }

    /// @dev A curve fills at its `validUntil` and is stale one second later. On cbBTC's curve at
    ///      the fork block, `validUntil` falls two seconds before the book's TTL ends
    ///      (`lastUpdateAt` + `ttl`), so the revert one second past it comes from `validUntil`,
    ///      not the TTL.
    function testCurveFillsUntilValidUntilInclusive() public {
        IBaibaiCurveBook book = IBaibaiCurveBook(CURVE_BOOK);
        uint256 validUntil = book.validUntil(CBBTC);
        uint256 ttlEnd = uint256(book.pair(CBBTC).lastUpdateAt) + book.ttl();
        assertLt(validUntil + 1, ttlEnd, "the TTL must outlast validUntil + 1");

        uint256 snapshot = vm.snapshotState();
        vm.warp(validUntil);
        (uint256 expected,) = IBaibaiFees(BAIBAI_ENTRYPOINT)
            .quoteFor(CBBTC, CBBTC, 1e6, tychoRouterAddr);
        assertGt(expected, 0);
        uint256 beforeBalance = IERC20(QUOTE).balanceOf(BOB);
        _sell(CBBTC, 1e6, expected);
        assertEq(IERC20(QUOTE).balanceOf(BOB) - beforeBalance, expected);
        vm.revertToState(snapshot);

        vm.warp(validUntil + 1);
        _assertSellReverts(CBBTC, 1e6, 1, CurveStale.selector);
    }

    /// @dev Sells `amount` of `base` for the quote token through the router, at no less than
    ///      `minOutput`.
    function _sell(address base, uint256 amount, uint256 minOutput) internal {
        deal(base, BOB, amount);
        vm.startPrank(BOB);
        IERC20(base).approve(tychoRouterAddr, amount);
        bytes memory swap = encodeSingleSwap(
            address(baibaiExecutor), abi.encodePacked(base, uint8(1))
        );
        tychoRouter.singleSwap(
            amount, base, QUOTE, minOutput, minOutput, BOB, noClientFee(), swap
        );
        vm.stopPrank();
    }

    function _assertSellReverts(
        address base,
        uint256 amount,
        uint256 minOutput,
        bytes4 reason
    ) internal {
        deal(base, BOB, amount);
        vm.startPrank(BOB);
        IERC20(base).approve(tychoRouterAddr, amount);
        bytes memory swap = encodeSingleSwap(
            address(baibaiExecutor), abi.encodePacked(base, uint8(1))
        );
        uint256 custodyBefore = IERC20(base).balanceOf(CUSTODIAN);
        vm.expectPartialRevert(reason);
        tychoRouter.singleSwap(
            amount, base, QUOTE, minOutput, minOutput, BOB, noClientFee(), swap
        );
        vm.stopPrank();
        assertEq(IERC20(base).balanceOf(BOB), amount);
        assertEq(IERC20(base).balanceOf(CUSTODIAN), custodyBefore);
        assertEq(IERC20(base).allowance(tychoRouterAddr, BAIBAI_ENTRYPOINT), 0);
    }
}
