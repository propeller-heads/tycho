// SPDX-License-Identifier: BUSL-1.1
// Copyright (c) 2026 Everlong Labs Limited
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {Constants} from "../Constants.sol";
import {TestUtils} from "../TestUtils.sol";
import {TychoRouterTestSetup} from "../TychoRouterTestSetup.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {
    FLAMMExecutor,
    FLAMMExecutor__InvalidDataLength,
    FLAMMExecutor__InvalidLeverPair,
    FLAMMExecutor__LeverDownUnsupported,
    FLAMMExecutor__PartialFill,
    FLAMMExecutor__UnknownPool,
    FLAMMExecutor__UnknownVenue,
    FLAMMExecutor__ZeroFactory,
    IFLAMMPool
} from "@src/executors/FLAMMExecutor.sol";

/// @dev Pool views and curator/keeper entrypoints the tests use to quote and to
/// open the leverage venue (blockend `IFLAMM.sol`, `LeverageSpreadHook.sol`).
interface IFLAMMPoolTest {
    function previewSwap(bool poolAssetIn, uint256 amountIn)
        external
        view
        returns (uint256 amountInUsed, uint256 amountOut, uint256 feeWad);
    function previewLever(bool up, uint256 amountIn)
        external
        view
        returns (
            uint256 amountInUsed,
            uint256 amountOut,
            uint256 spreadPpm,
            uint256 crAfterWad
        );
    function setLevPaused(bool p) external;
}

interface ILeverageSpreadHookTest {
    function setSpread(uint24 newSpread) external;
}

/// @dev The Everlong FLAMM stack on Base (deployment record
/// `script/flamm/c104/deployments/c104.8453.json` in blockend).
abstract contract FLAMMTestBase {
    address internal constant FLAMM_POOL =
        0xc0fdCB1799cCc2CEBaA1fe247157b0dF33D57572;
    address internal constant FLAMM_FACTORY =
        0x1BfcE014774D0DD7e04bC595D46Fa09F7dCCF45f;
    address internal constant FLAMM_SPREAD_HOOK =
        0x04988aF54ec88D2de77b191025EAef2fe488f93b;
    /// @dev `EverlongCore.owner()` and `.keeper()` at the fork block.
    address internal constant FLAMM_CURATOR =
        0xeb765F3184f705F5292679f42beEDBbe5D272c49;
    address internal constant FLAMM_KEEPER =
        0x13935698E03c5b0098693EF5B060c7dcAFb81DbF;

    /// @dev Parent of block 51302916, which holds the pool's first settled swap
    /// (tx 0x46c3cd72a5860b2fe546e5a2130e066314e3777027151661e1e4f19a935901fa:
    /// 15000 sats in, 11301759 USDC out; five more have settled since). Forking
    /// here replays it from the state it was priced on.
    uint256 internal constant FORK_BLOCK = 51302915;
    uint256 internal constant REPLAY_AMOUNT_IN = 15_000;
    uint256 internal constant REPLAY_AMOUNT_OUT = 11_301_759;

    uint256 internal constant BUY_AMOUNT_IN = 10_000_000;
    uint256 internal constant LEVER_UP_AMOUNT_IN = 10_000;
    /// @dev A live spread the keeper posts to open the venue. At FORK_BLOCK the
    /// deployed venue is levPaused and has no live spread: the
    /// LeverageSpreadHook constructor's 17500 ppm post (lastSetTs at the
    /// creation) had aged past maxSpreadAge = 3600 s and was never
    /// re-posted, so `spreadPpm` answers `(false, 0)` and a lever-up reverts
    /// SpreadUnavailable (the curator unpaused the venue on chain at 51433699;
    /// the spread is still stale there). That is the state at FORK_BLOCK, not
    /// the venue today: on 2026-09-22 the curator cleared the staleness window
    /// at block 51649706 (MaxSpreadAgeSet(0)), so the same constructor post no
    /// longer lapses and the deployed venue quotes 17500 ppm unprompted -- the
    /// value this constant re-posts here. Forking past 51649706 to run the
    /// lever tests against a genuinely open venue is a follow-up; FORK_BLOCK
    /// must not move, it is the parent of the swap testReplaySwap replays.
    uint24 internal constant LEVER_SPREAD_PPM = 17_500;

    uint8 internal constant VENUE_SWAP = 0;
    uint8 internal constant VENUE_LEVER_UP = 1;

    function _flammData(address tokenIn, address tokenOut, uint8 venue)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(FLAMM_POOL, tokenIn, tokenOut, venue);
    }
}

contract FLAMMExecutorExposed is FLAMMExecutor {
    constructor(address factory_) FLAMMExecutor(factory_) {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (address pool, address tokenIn, address tokenOut, uint8 venue)
    {
        return _decodeData(data);
    }
}

contract FLAMMExecutorTest is Constants, TestUtils, FLAMMTestBase {
    FLAMMExecutorExposed flammExecutor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), FORK_BLOCK);
        flammExecutor = new FLAMMExecutorExposed(FLAMM_FACTORY);
    }

    // ------------------------------------------------------------ construction

    function testConstructorRejectsZeroFactory() public {
        vm.expectRevert(FLAMMExecutor__ZeroFactory.selector);
        new FLAMMExecutor(address(0));
    }

    function testFactoryIsImmutable() public view {
        assertEq(flammExecutor.factory(), FLAMM_FACTORY);
    }

    // ------------------------------------------------------------ decoding

    function testDecodeParams() public view {
        (address pool, address tokenIn, address tokenOut, uint8 venue) = flammExecutor.decodeParams(
            _flammData(BASE_cbBTC, BASE_USDC, VENUE_SWAP)
        );
        assertEq(pool, FLAMM_POOL);
        assertEq(tokenIn, BASE_cbBTC);
        assertEq(tokenOut, BASE_USDC);
        assertEq(venue, VENUE_SWAP);

        (pool, tokenIn, tokenOut, venue) = flammExecutor.decodeParams(
            _flammData(BASE_cbBTC, BASE_USDC, VENUE_LEVER_UP)
        );
        assertEq(pool, FLAMM_POOL);
        assertEq(tokenIn, BASE_cbBTC);
        assertEq(tokenOut, BASE_USDC);
        assertEq(venue, VENUE_LEVER_UP);
    }

    function testDecodeParamsInvalidDataLength() public {
        // No venue byte.
        vm.expectRevert(FLAMMExecutor__InvalidDataLength.selector);
        flammExecutor.decodeParams(
            abi.encodePacked(FLAMM_POOL, BASE_cbBTC, BASE_USDC)
        );
        // A trailing byte.
        vm.expectRevert(FLAMMExecutor__InvalidDataLength.selector);
        flammExecutor.decodeParams(
            abi.encodePacked(
                _flammData(BASE_cbBTC, BASE_USDC, VENUE_SWAP), hex"00"
            )
        );
        vm.expectRevert(FLAMMExecutor__InvalidDataLength.selector);
        flammExecutor.decodeParams("");
    }

    function testDecodeParamsUnknownVenue() public {
        vm.expectRevert(
            abi.encodeWithSelector(FLAMMExecutor__UnknownVenue.selector, 2)
        );
        flammExecutor.decodeParams(_flammData(BASE_cbBTC, BASE_USDC, 2));
        vm.expectRevert(
            abi.encodeWithSelector(FLAMMExecutor__UnknownVenue.selector, 255)
        );
        flammExecutor.decodeParams(_flammData(BASE_cbBTC, BASE_USDC, 255));
    }

    // ------------------------------------------------------------ transfer data

    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = flammExecutor.getTransferData(
            _flammData(BASE_cbBTC, BASE_USDC, VENUE_SWAP)
        );
        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, FLAMM_POOL);
        assertEq(tokenIn, BASE_cbBTC);
        assertEq(tokenOut, BASE_USDC);
        assertEq(outputToRouter, false);

        (transferType, receiver, tokenIn, tokenOut, outputToRouter) =
            flammExecutor.getTransferData(
                _flammData(BASE_USDC, BASE_cbBTC, VENUE_SWAP)
            );
        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, FLAMM_POOL);
        assertEq(tokenIn, BASE_USDC);
        assertEq(tokenOut, BASE_cbBTC);
        assertEq(outputToRouter, false);

        (transferType, receiver, tokenIn, tokenOut, outputToRouter) =
            flammExecutor.getTransferData(
                _flammData(BASE_cbBTC, BASE_USDC, VENUE_LEVER_UP)
            );
        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, FLAMM_POOL);
        assertEq(tokenIn, BASE_cbBTC);
        assertEq(tokenOut, BASE_USDC);
        assertEq(outputToRouter, false);
    }

    function testGetTransferDataRejectsBadData() public {
        vm.expectRevert(FLAMMExecutor__InvalidDataLength.selector);
        flammExecutor.getTransferData(
            abi.encodePacked(FLAMM_POOL, BASE_cbBTC, BASE_USDC)
        );
        vm.expectRevert(
            abi.encodeWithSelector(FLAMMExecutor__UnknownVenue.selector, 7)
        );
        flammExecutor.getTransferData(_flammData(BASE_cbBTC, BASE_USDC, 7));
    }

    function testFundsExpectedAddress() public view {
        assertEq(
            flammExecutor.fundsExpectedAddress(
                _flammData(BASE_cbBTC, BASE_USDC, VENUE_SWAP)
            ),
            address(this)
        );
        assertEq(
            flammExecutor.fundsExpectedAddress(
                _flammData(BASE_cbBTC, BASE_USDC, VENUE_LEVER_UP)
            ),
            address(this)
        );
    }

    // ------------------------------------------------------------ fork fills

    /// @dev Replays the pool's first settled swap from the state it was priced
    /// on: 15000 sats must pay exactly 11301759 USDC.
    function testSwapSellReplaysSettledFill() public {
        (uint256 quotedUsed, uint256 quotedOut,) =
            IFLAMMPoolTest(FLAMM_POOL).previewSwap(true, REPLAY_AMOUNT_IN);
        assertEq(quotedUsed, REPLAY_AMOUNT_IN);
        assertEq(quotedOut, REPLAY_AMOUNT_OUT);

        _fund(BASE_cbBTC, REPLAY_AMOUNT_IN);
        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(BOB);

        uint256 gasBefore = gasleft();
        flammExecutor.swap(
            REPLAY_AMOUNT_IN, _flammData(BASE_cbBTC, BASE_USDC, VENUE_SWAP), BOB
        );
        emit log_named_uint(
            "gas: FLAMMExecutor.swap sell (direct call)", gasBefore - gasleft()
        );

        assertEq(
            IERC20(BASE_USDC).balanceOf(BOB) - balanceBefore, REPLAY_AMOUNT_OUT
        );
        assertEq(IERC20(BASE_cbBTC).balanceOf(address(flammExecutor)), 0);
        assertEq(
            IERC20(BASE_cbBTC).allowance(address(flammExecutor), FLAMM_POOL), 0
        );
    }

    function testSwapBuy() public {
        (uint256 quotedUsed, uint256 quotedOut,) =
            IFLAMMPoolTest(FLAMM_POOL).previewSwap(false, BUY_AMOUNT_IN);
        assertEq(quotedUsed, BUY_AMOUNT_IN);
        assertGt(quotedOut, 0);

        _fund(BASE_USDC, BUY_AMOUNT_IN);
        uint256 balanceBefore = IERC20(BASE_cbBTC).balanceOf(BOB);

        uint256 gasBefore = gasleft();
        flammExecutor.swap(
            BUY_AMOUNT_IN, _flammData(BASE_USDC, BASE_cbBTC, VENUE_SWAP), BOB
        );
        emit log_named_uint(
            "gas: FLAMMExecutor.swap buy (direct call)", gasBefore - gasleft()
        );

        assertEq(IERC20(BASE_cbBTC).balanceOf(BOB) - balanceBefore, quotedOut);
        assertEq(IERC20(BASE_USDC).balanceOf(address(flammExecutor)), 0);
    }

    function testLeverUp() public {
        _openLeverageVenue();
        (uint256 quotedUsed, uint256 quotedOut,,) =
            IFLAMMPoolTest(FLAMM_POOL).previewLever(true, LEVER_UP_AMOUNT_IN);
        assertEq(quotedUsed, LEVER_UP_AMOUNT_IN);
        assertGt(quotedOut, 0);

        _fund(BASE_cbBTC, LEVER_UP_AMOUNT_IN);
        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(BOB);

        uint256 gasBefore = gasleft();
        flammExecutor.swap(
            LEVER_UP_AMOUNT_IN,
            _flammData(BASE_cbBTC, BASE_USDC, VENUE_LEVER_UP),
            BOB
        );
        emit log_named_uint(
            "gas: FLAMMExecutor.swap lever-up (direct call)",
            gasBefore - gasleft()
        );

        assertEq(IERC20(BASE_USDC).balanceOf(BOB) - balanceBefore, quotedOut);
        assertEq(IERC20(BASE_cbBTC).balanceOf(address(flammExecutor)), 0);
    }

    // ------------------------------------------------------------ refusals

    function testLeverDownIsUnsupported() public {
        _openLeverageVenue();
        _fund(BASE_USDC, BUY_AMOUNT_IN);

        vm.expectRevert(FLAMMExecutor__LeverDownUnsupported.selector);
        flammExecutor.swap(
            BUY_AMOUNT_IN,
            _flammData(BASE_USDC, BASE_cbBTC, VENUE_LEVER_UP),
            BOB
        );
    }

    function testLeverUpRejectsForeignPair() public {
        _openLeverageVenue();
        _fund(BASE_cbBTC, LEVER_UP_AMOUNT_IN);

        vm.expectRevert(
            abi.encodeWithSelector(
                FLAMMExecutor__InvalidLeverPair.selector, BASE_cbBTC, BASE_WETH
            )
        );
        flammExecutor.swap(
            LEVER_UP_AMOUNT_IN,
            _flammData(BASE_cbBTC, BASE_WETH, VENUE_LEVER_UP),
            BOB
        );
    }

    function testUnknownPoolReverts() public {
        address impostor = makeAddr("impostor pool");
        vm.expectRevert(
            abi.encodeWithSelector(
                FLAMMExecutor__UnknownPool.selector, impostor
            )
        );
        flammExecutor.swap(
            REPLAY_AMOUNT_IN,
            abi.encodePacked(impostor, BASE_cbBTC, BASE_USDC, VENUE_SWAP),
            BOB
        );
    }

    function testSwapPartialFillReverts() public {
        _fund(BASE_cbBTC, REPLAY_AMOUNT_IN);
        vm.mockCall(
            FLAMM_POOL,
            abi.encodeWithSelector(IFLAMMPool.swap.selector),
            abi.encode(REPLAY_AMOUNT_IN - 1, REPLAY_AMOUNT_OUT)
        );

        vm.expectRevert(
            abi.encodeWithSelector(
                FLAMMExecutor__PartialFill.selector,
                REPLAY_AMOUNT_IN - 1,
                REPLAY_AMOUNT_IN
            )
        );
        flammExecutor.swap(
            REPLAY_AMOUNT_IN, _flammData(BASE_cbBTC, BASE_USDC, VENUE_SWAP), BOB
        );
        vm.clearMockedCalls();
    }

    function testLeverUpPartialFillReverts() public {
        _openLeverageVenue();
        _fund(BASE_cbBTC, LEVER_UP_AMOUNT_IN);
        vm.mockCall(
            FLAMM_POOL,
            abi.encodeWithSelector(IFLAMMPool.leverUp.selector),
            abi.encode(LEVER_UP_AMOUNT_IN / 2, uint256(1))
        );

        vm.expectRevert(
            abi.encodeWithSelector(
                FLAMMExecutor__PartialFill.selector,
                LEVER_UP_AMOUNT_IN / 2,
                LEVER_UP_AMOUNT_IN
            )
        );
        flammExecutor.swap(
            LEVER_UP_AMOUNT_IN,
            _flammData(BASE_cbBTC, BASE_USDC, VENUE_LEVER_UP),
            BOB
        );
        vm.clearMockedCalls();
    }

    // ------------------------------------------------------------ encoder round trip

    /// @dev The Rust encoder's output (`flamm.rs` tests) decodes to the same
    /// fields the tests build by hand.
    function testEncoderCalldataRoundTrip() public view {
        (address pool, address tokenIn, address tokenOut, uint8 venue) = flammExecutor.decodeParams(
            loadCallDataFromFile("test_encode_flamm_sell")
        );
        assertEq(pool, FLAMM_POOL);
        assertEq(tokenIn, BASE_cbBTC);
        assertEq(tokenOut, BASE_USDC);
        assertEq(venue, VENUE_SWAP);

        (pool, tokenIn, tokenOut, venue) = flammExecutor.decodeParams(
            loadCallDataFromFile("test_encode_flamm_buy")
        );
        assertEq(pool, FLAMM_POOL);
        assertEq(tokenIn, BASE_USDC);
        assertEq(tokenOut, BASE_cbBTC);
        assertEq(venue, VENUE_SWAP);

        (pool, tokenIn, tokenOut, venue) = flammExecutor.decodeParams(
            loadCallDataFromFile("test_encode_flamm_lever_up")
        );
        assertEq(pool, FLAMM_POOL);
        assertEq(tokenIn, BASE_cbBTC);
        assertEq(tokenOut, BASE_USDC);
        assertEq(venue, VENUE_LEVER_UP);
    }

    // ------------------------------------------------------------ helpers

    /// @dev Called directly (not through the router), the executor is the
    /// pool's `msg.sender`, so it holds the input and grants the allowance the
    /// Dispatcher would otherwise grant.
    function _fund(address token, uint256 amount) internal {
        deal(token, address(flammExecutor), amount);
        vm.prank(address(flammExecutor));
        IERC20(token).approve(FLAMM_POOL, amount);
    }

    /// @dev At FORK_BLOCK the venue is levPaused and the constructor's spread
    /// has aged past maxSpreadAge (no live spread): the curator unpauses it and
    /// the keeper posts a fresh spread. Both are simulated state at this block
    /// only -- on chain the curator unpaused at 51433699 and cleared the
    /// staleness window at 51649706, from where the venue quotes without either
    /// prank (see LEVER_SPREAD_PPM).
    function _openLeverageVenue() internal {
        vm.prank(FLAMM_CURATOR);
        IFLAMMPoolTest(FLAMM_POOL).setLevPaused(false);
        vm.prank(FLAMM_KEEPER);
        ILeverageSpreadHookTest(FLAMM_SPREAD_HOOK).setSpread(LEVER_SPREAD_PPM);
    }
}

contract TychoRouterForFLAMMTest is TychoRouterTestSetup, FLAMMTestBase {
    /// @dev The executor's address in this setup, pinned with `deployCodeTo`
    /// so that no deployment the shared setup adds or removes moves it. It is
    /// deliberately an address no deployment can produce: the shared setup's
    /// own `deployFeeCalculator` once landed on the sequential address this
    /// constant used to hold, and etching over it left the router calling
    /// `mustOutputThroughRouter` on this executor. Three
    /// places carry it and must move together: this constant, the
    /// `base.flamm` entry of `config/test_executor_addresses.json` (which the
    /// Rust `test_single_encoding_strategy_flamm` encodes into) and the
    /// `test_single_encoding_strategy_flamm` line of `test/assets/calldata.txt`
    /// (its output, which `testSingleFLAMMIntegration` replays).
    address internal constant FLAMM_TEST_EXECUTOR =
        0xf1A33000000000000000000000000000000F1A33;

    FLAMMExecutor flammExecutor;

    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return FORK_BLOCK;
    }

    function setUp() public override {
        super.setUp();

        // Placed at the pinned address after the shared executor set, so
        // neither this deployment nor a later one in `deployExecutors` moves
        // any address the Rust-generated calldata hardcodes. The activation
        // timelock is served in the past, the way the shared setup does it, so
        // the fork's Chainlink rounds stay fresh for the pool's price feed.
        vm.warp(forkTimestamp - _SETUP_TIME_OFFSET_NEW_EXECUTOR);
        deployCodeTo(
            "FLAMMExecutor.sol:FLAMMExecutor",
            abi.encode(FLAMM_FACTORY),
            FLAMM_TEST_EXECUTOR
        );
        flammExecutor = FLAMMExecutor(FLAMM_TEST_EXECUTOR);
        address[] memory executors = new address[](1);
        executors[0] = address(flammExecutor);
        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.warp(forkTimestamp);
    }

    /// @dev The pinned executor is live at the address the calldata targets,
    /// with its factory wired.
    function testExecutorAddressMatchesTestConfig() public view {
        assertEq(address(flammExecutor), FLAMM_TEST_EXECUTOR);
        assertGt(FLAMM_TEST_EXECUTOR.code.length, 0);
        assertEq(flammExecutor.factory(), FLAMM_FACTORY);
    }

    /// @dev Router calldata from `test_single_encoding_strategy_flamm`
    /// (Rust): ALICE sells 15000 sats for at least 11301759 USDC.
    function testSingleFLAMMIntegration() public {
        deal(BASE_cbBTC, ALICE, REPLAY_AMOUNT_IN);
        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(BASE_cbBTC).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_flamm");
        uint256 gasBefore = gasleft();
        (bool success,) = tychoRouterAddr.call(callData);
        emit log_named_uint(
            "gas: TychoRouterV3.singleSwap FLAMM sell", gasBefore - gasleft()
        );
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(
            IERC20(BASE_USDC).balanceOf(ALICE) - balanceBefore,
            REPLAY_AMOUNT_OUT
        );
        assertEq(IERC20(BASE_cbBTC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_USDC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_cbBTC).allowance(tychoRouterAddr, FLAMM_POOL), 0);
    }

    function testSingleSwapBuyThroughRouter() public {
        (, uint256 expectedAmountOut,) =
            IFLAMMPoolTest(FLAMM_POOL).previewSwap(false, BUY_AMOUNT_IN);
        bytes memory swap = encodeSingleSwap(
            address(flammExecutor),
            _flammData(BASE_USDC, BASE_cbBTC, VENUE_SWAP)
        );

        deal(BASE_USDC, BOB, BUY_AMOUNT_IN);
        uint256 balanceBefore = IERC20(BASE_cbBTC).balanceOf(BOB);

        vm.startPrank(BOB);
        IERC20(BASE_USDC).approve(tychoRouterAddr, BUY_AMOUNT_IN);
        uint256 amountOut = tychoRouter.singleSwap(
            BUY_AMOUNT_IN,
            BASE_USDC,
            BASE_cbBTC,
            expectedAmountOut,
            expectedAmountOut,
            BOB,
            noClientFee(),
            swap
        );
        vm.stopPrank();

        assertEq(amountOut, expectedAmountOut);
        assertEq(
            IERC20(BASE_cbBTC).balanceOf(BOB) - balanceBefore, expectedAmountOut
        );
        assertEq(IERC20(BASE_USDC).balanceOf(tychoRouterAddr), 0);
    }

    function testLeverUpThroughRouter() public {
        vm.prank(FLAMM_CURATOR);
        IFLAMMPoolTest(FLAMM_POOL).setLevPaused(false);
        vm.prank(FLAMM_KEEPER);
        ILeverageSpreadHookTest(FLAMM_SPREAD_HOOK).setSpread(LEVER_SPREAD_PPM);
        (, uint256 expectedAmountOut,,) =
            IFLAMMPoolTest(FLAMM_POOL).previewLever(true, LEVER_UP_AMOUNT_IN);
        bytes memory swap = encodeSingleSwap(
            address(flammExecutor),
            _flammData(BASE_cbBTC, BASE_USDC, VENUE_LEVER_UP)
        );

        deal(BASE_cbBTC, BOB, LEVER_UP_AMOUNT_IN);
        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(BOB);

        vm.startPrank(BOB);
        IERC20(BASE_cbBTC).approve(tychoRouterAddr, LEVER_UP_AMOUNT_IN);
        uint256 gasBefore = gasleft();
        uint256 amountOut = tychoRouter.singleSwap(
            LEVER_UP_AMOUNT_IN,
            BASE_cbBTC,
            BASE_USDC,
            expectedAmountOut,
            expectedAmountOut,
            BOB,
            noClientFee(),
            swap
        );
        emit log_named_uint(
            "gas: TychoRouterV3.singleSwap FLAMM lever-up",
            gasBefore - gasleft()
        );
        vm.stopPrank();

        assertEq(amountOut, expectedAmountOut);
        assertEq(
            IERC20(BASE_USDC).balanceOf(BOB) - balanceBefore, expectedAmountOut
        );
        assertEq(IERC20(BASE_cbBTC).balanceOf(tychoRouterAddr), 0);
    }

    /// @dev Two FLAMM legs back to back: the second leg's
    /// `fundsExpectedAddress` is the router, so the first leg pays the router
    /// and the Dispatcher approves the pool for the second pull.
    function testSequentialBuyThenSellThroughRouter() public {
        // The expected output is the sell quote on the post-buy state, taken
        // on a snapshot that is then rolled back.
        uint256 snapshot = vm.snapshotState();
        address quoter = makeAddr("quoter");
        deal(BASE_USDC, quoter, BUY_AMOUNT_IN);
        vm.startPrank(quoter);
        IERC20(BASE_USDC).approve(FLAMM_POOL, BUY_AMOUNT_IN);
        (, uint256 satsOut) = IFLAMMPool(FLAMM_POOL)
            .swap(
                BASE_USDC, BASE_cbBTC, BUY_AMOUNT_IN, 0, quoter, block.timestamp
            );
        vm.stopPrank();
        (uint256 sellUsed, uint256 expectedAmountOut,) =
            IFLAMMPoolTest(FLAMM_POOL).previewSwap(true, satsOut);
        assertEq(sellUsed, satsOut);
        assertTrue(vm.revertToState(snapshot));

        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(
            address(flammExecutor),
            _flammData(BASE_USDC, BASE_cbBTC, VENUE_SWAP)
        );
        swaps[1] = encodeSequentialSwap(
            address(flammExecutor),
            _flammData(BASE_cbBTC, BASE_USDC, VENUE_SWAP)
        );

        deal(BASE_USDC, BOB, BUY_AMOUNT_IN);
        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(BOB);

        vm.startPrank(BOB);
        IERC20(BASE_USDC).approve(tychoRouterAddr, BUY_AMOUNT_IN);
        uint256 amountOut = tychoRouter.sequentialSwap(
            BUY_AMOUNT_IN,
            BASE_USDC,
            BASE_USDC,
            expectedAmountOut,
            expectedAmountOut,
            BOB,
            noClientFee(),
            pleEncode(swaps)
        );
        vm.stopPrank();

        assertEq(amountOut, expectedAmountOut);
        assertEq(
            IERC20(BASE_USDC).balanceOf(BOB) + BUY_AMOUNT_IN - balanceBefore,
            expectedAmountOut
        );
        assertEq(IERC20(BASE_cbBTC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_USDC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_cbBTC).allowance(tychoRouterAddr, FLAMM_POOL), 0);
    }

    /// @dev A partial fill surfaces as the executor's own error through the
    /// router, and nothing is left behind.
    function testPartialFillRevertsThroughRouter() public {
        bytes memory swap = encodeSingleSwap(
            address(flammExecutor),
            _flammData(BASE_cbBTC, BASE_USDC, VENUE_SWAP)
        );
        deal(BASE_cbBTC, BOB, REPLAY_AMOUNT_IN);
        vm.mockCall(
            FLAMM_POOL,
            abi.encodeWithSelector(IFLAMMPool.swap.selector),
            abi.encode(REPLAY_AMOUNT_IN - 1, REPLAY_AMOUNT_OUT)
        );

        vm.startPrank(BOB);
        IERC20(BASE_cbBTC).approve(tychoRouterAddr, REPLAY_AMOUNT_IN);
        vm.expectRevert(
            abi.encodeWithSelector(
                FLAMMExecutor__PartialFill.selector,
                REPLAY_AMOUNT_IN - 1,
                REPLAY_AMOUNT_IN
            )
        );
        tychoRouter.singleSwap(
            REPLAY_AMOUNT_IN,
            BASE_cbBTC,
            BASE_USDC,
            1,
            1,
            BOB,
            noClientFee(),
            swap
        );
        vm.stopPrank();
        vm.clearMockedCalls();

        assertEq(IERC20(BASE_cbBTC).balanceOf(BOB), REPLAY_AMOUNT_IN);
        assertEq(IERC20(BASE_cbBTC).balanceOf(tychoRouterAddr), 0);
    }
}
