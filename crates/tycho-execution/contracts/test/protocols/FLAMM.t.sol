// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {MockERC20} from "forge-std/mocks/MockERC20.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {PropAMMExecutor} from "../../src/executors/PropAMMExecutor.sol";

/// @dev An Efficient pool's refusals: a found input it would not use whole, and
/// a payout to itself.
error InputUnused();
error InvalidAmount();

/// @dev Prices every pair at a fixed rate and settles the way an Efficient pool
/// does. Input past its custody is found, and used whole or not at all; anything
/// less it pulls from the caller with `transferFrom`. `fillCap` clips the input
/// it uses, as a pool's notional cap or its venues' funding can.
contract MockFLAMMPool {
    /// Output per 1e18 units of input, per (tokenIn, tokenOut).
    mapping(address => mapping(address => uint256)) public priceWad;
    mapping(address => uint256) public custody;
    uint256 public fillCap;

    function setPrice(address tokenIn, address tokenOut, uint256 priceWad_)
        external
    {
        priceWad[tokenIn][tokenOut] = priceWad_;
    }

    function setFillCap(uint256 fillCap_) external {
        fillCap = fillCap_;
    }

    /// @dev Books the pool's balance as its custody: nothing is past it.
    function syncCustody(address token) external {
        custody[token] = IERC20(token).balanceOf(address(this));
    }

    function swap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut,
        address to,
        uint256 deadline
    ) external returns (uint256 amountInUsed, uint256 amountOut) {
        require(deadline >= block.timestamp, "MockFLAMMPool: expired");
        if (to == address(this) || amountIn == 0) revert InvalidAmount();
        amountInUsed = fillCap != 0 && amountIn > fillCap ? fillCap : amountIn;
        amountOut = amountInUsed * priceWad[tokenIn][tokenOut] / 1e18;
        require(
            amountOut != 0 && amountOut >= minAmountOut,
            "MockFLAMMPool: slippage"
        );
        _takeIn(tokenIn, amountIn, amountInUsed);
        custody[tokenIn] += amountInUsed;
        custody[tokenOut] -= amountOut;
        IERC20(tokenOut).transfer(to, amountOut);
    }

    function _takeIn(address tokenIn, uint256 asked, uint256 used)
        internal
        virtual
    {
        uint256 past =
            IERC20(tokenIn).balanceOf(address(this)) - custody[tokenIn];
        if (past >= asked) {
            if (used != asked) revert InputUnused();
        } else {
            IERC20(tokenIn).transferFrom(msg.sender, address(this), used);
        }
    }
}

/// @dev The same pool had it gone back to pulling every input.
contract MockPullingFLAMMPool is MockFLAMMPool {
    function _takeIn(address tokenIn, uint256, uint256 used) internal override {
        IERC20(tokenIn).transferFrom(msg.sender, address(this), used);
    }
}

/// @dev The addresses the Rust `flamm` tests encode: the mock pools (their
/// components' `pamm_address`), and ALICE's sale of `SALE_IN` sats for `SALE_OUT`
/// USDC at `CBBTC_USDC_PRICE_WAD`, which then buys `ROUTE_OUT` WETH at
/// `USDC_WETH_PRICE_WAD`.
abstract contract FLAMMTestBase {
    address internal constant MOCK_POOL =
        0xF1a3300000000000000000000000000000000001;
    address internal constant MOCK_POOL_B =
        0xF1a3300000000000000000000000000000000002;
    /// 75,000 USDC per cbBTC: 750 USDC base units per sat.
    uint256 internal constant CBBTC_USDC_PRICE_WAD = 750e18;
    /// 0.0005 WETH per USDC.
    uint256 internal constant USDC_WETH_PRICE_WAD = 5e26;
    uint256 internal constant SALE_IN = 15_000;
    uint256 internal constant SALE_OUT = 11_250_000;
    uint256 internal constant ROUTE_OUT = 5_625_000_000_000_000;

    function _flammData(address pool, address tokenIn, address tokenOut)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(pool, tokenIn, tokenOut);
    }
}

/// @dev The generic executor against a pool that settles as an Efficient pool
/// does. Nothing here needs a chain: mock ERC20s stand at the Base addresses.
contract PropAMMExecutorForFLAMMTest is TestUtils, Constants, FLAMMTestBase {
    PropAMMExecutor executor;

    function setUp() public {
        deployCodeTo("MockERC20.sol:MockERC20", BASE_cbBTC);
        MockERC20(BASE_cbBTC).initialize("Coinbase Wrapped BTC", "cbBTC", 8);
        deployCodeTo("MockERC20.sol:MockERC20", BASE_USDC);
        MockERC20(BASE_USDC).initialize("USD Coin", "USDC", 6);
        deployCodeTo("FLAMM.t.sol:MockFLAMMPool", MOCK_POOL);
        MockFLAMMPool(MOCK_POOL)
            .setPrice(BASE_cbBTC, BASE_USDC, CBBTC_USDC_PRICE_WAD);
        deal(BASE_USDC, MOCK_POOL, 1_000_000e6);
        MockFLAMMPool(MOCK_POOL).syncCustody(BASE_USDC);
        executor = new PropAMMExecutor();
    }

    /// @dev The router sends the input to the pool, and the previous hop pays
    /// the pool too: a pool that pulled would have no allowance to pull with.
    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(
            _flammData(MOCK_POOL, BASE_cbBTC, BASE_USDC)
        );
        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, MOCK_POOL);
        assertEq(tokenIn, BASE_cbBTC);
        assertEq(tokenOut, BASE_USDC);
        assertFalse(outputToRouter);
    }

    function testFundsExpectedAddress() public view {
        assertEq(
            executor.fundsExpectedAddress(
                _flammData(MOCK_POOL, BASE_cbBTC, BASE_USDC)
            ),
            MOCK_POOL
        );
    }

    /// @dev The pool returns two words where the executor's interface declares
    /// one, which it ignores: the output is what reached the receiver.
    function testSwapFindsThePush() public {
        deal(BASE_cbBTC, MOCK_POOL, SALE_IN);
        executor.swap(
            SALE_IN, _flammData(MOCK_POOL, BASE_cbBTC, BASE_USDC), BOB
        );
        assertEq(IERC20(BASE_USDC).balanceOf(BOB), SALE_OUT);
        assertEq(MockFLAMMPool(MOCK_POOL).custody(BASE_cbBTC), SALE_IN);
    }

    function testAClipOfAFoundInputReverts() public {
        MockFLAMMPool(MOCK_POOL).setFillCap(SALE_IN - 1);
        deal(BASE_cbBTC, MOCK_POOL, SALE_IN);
        vm.expectRevert(InputUnused.selector);
        executor.swap(
            SALE_IN, _flammData(MOCK_POOL, BASE_cbBTC, BASE_USDC), BOB
        );
    }

    function testAPoolRefusesToPayItself() public {
        deal(BASE_cbBTC, MOCK_POOL, SALE_IN);
        vm.expectRevert(InvalidAmount.selector);
        executor.swap(
            SALE_IN, _flammData(MOCK_POOL, BASE_cbBTC, BASE_USDC), MOCK_POOL
        );
    }

    /// @dev What the route needs of a pool: one that pulled reverts, as the
    /// executor gives it no allowance. The reason is the mock token's own for
    /// spending an allowance of zero.
    function testAPoolThatPullsReverts() public {
        deployCodeTo("FLAMM.t.sol:MockPullingFLAMMPool", MOCK_POOL_B);
        MockFLAMMPool(MOCK_POOL_B)
            .setPrice(BASE_cbBTC, BASE_USDC, CBBTC_USDC_PRICE_WAD);
        deal(BASE_USDC, MOCK_POOL_B, 1_000_000e6);
        MockFLAMMPool(MOCK_POOL_B).syncCustody(BASE_USDC);
        deal(BASE_cbBTC, MOCK_POOL_B, SALE_IN);
        vm.expectRevert("ERC20: subtraction underflow");
        executor.swap(
            SALE_IN, _flammData(MOCK_POOL_B, BASE_cbBTC, BASE_USDC), BOB
        );
    }
}

/// @dev The whole route through the router, on a pool that settles as an
/// Efficient pool does.
contract TychoRouterForFLAMMTest is TychoRouterTestSetup, FLAMMTestBase {
    /// @dev The generic executor's address in the shared setup on Base. Two
    /// other places carry it and must move with `deployExecutors`: the
    /// `base.flamm` entry of `config/test_executor_addresses.json`, and the
    /// `flamm` lines of `test/assets/calldata.txt` the Rust tests write from it.
    address internal constant FLAMM_TEST_EXECUTOR =
        0x92a6649Fdcc044DA968d94202465578a9371C7b1;

    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 53428793;
    }

    function setUp() public override {
        super.setUp();
        assertEq(address(propAMMExecutor), FLAMM_TEST_EXECUTOR);
        _placePool("FLAMM.t.sol:MockFLAMMPool", MOCK_POOL);
        _placePool("FLAMM.t.sol:MockFLAMMPool", MOCK_POOL_B);
    }

    function _placePool(string memory artifact, address pool) internal {
        deployCodeTo(artifact, pool);
        MockFLAMMPool(pool)
            .setPrice(BASE_cbBTC, BASE_USDC, CBBTC_USDC_PRICE_WAD);
        MockFLAMMPool(pool).setPrice(BASE_USDC, BASE_cbBTC, 1e15);
        MockFLAMMPool(pool).setPrice(BASE_USDC, BASE_WETH, USDC_WETH_PRICE_WAD);
        deal(BASE_USDC, pool, 1_000_000e6);
        deal(BASE_cbBTC, pool, 10e8);
        deal(BASE_WETH, pool, 100e18);
        MockFLAMMPool(pool).syncCustody(BASE_USDC);
        MockFLAMMPool(pool).syncCustody(BASE_cbBTC);
        MockFLAMMPool(pool).syncCustody(BASE_WETH);
    }

    function _assertRouterFlat(address pool) internal view {
        assertEq(IERC20(BASE_cbBTC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_USDC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_WETH).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_cbBTC).allowance(tychoRouterAddr, pool), 0);
        assertEq(IERC20(BASE_USDC).allowance(tychoRouterAddr, pool), 0);
    }

    /// @dev Router calldata from `test_single_encoding_strategy_flamm` (Rust):
    /// ALICE sells `SALE_IN` sats for at least `SALE_OUT` USDC.
    function testSingleFLAMMIntegration() public {
        deal(BASE_cbBTC, ALICE, SALE_IN);
        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(BASE_cbBTC).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_flamm");
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(BASE_USDC).balanceOf(ALICE) - balanceBefore, SALE_OUT);
        assertEq(
            MockFLAMMPool(MOCK_POOL).custody(BASE_cbBTC),
            10e8 + SALE_IN,
            "the pool found the input"
        );
        _assertRouterFlat(MOCK_POOL);
    }

    /// @dev Router calldata from
    /// `test_sequential_encoding_strategy_flamm_two_pools` (Rust): the first pool
    /// pays the second, which finds the input without the router touching it.
    function testSequentialTwoPoolsIntegration() public {
        deal(BASE_cbBTC, ALICE, SALE_IN);
        uint256 balanceBefore = IERC20(BASE_WETH).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(BASE_cbBTC).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData = loadCallDataFromFile(
            "test_sequential_encoding_strategy_flamm_two_pools"
        );
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(BASE_WETH).balanceOf(ALICE) - balanceBefore, ROUTE_OUT);
        assertEq(
            MockFLAMMPool(MOCK_POOL_B).custody(BASE_USDC),
            1_000_000e6 + SALE_OUT,
            "the second pool found the first pool's payout"
        );
        _assertRouterFlat(MOCK_POOL);
        _assertRouterFlat(MOCK_POOL_B);
    }

    /// @dev The first hop's receiver is the second hop's pool, here the same
    /// one, and it refuses to pay itself. The encoder refuses this route before
    /// it gets here.
    function testBackToBackHopsOnOnePoolRevert() public {
        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(
            FLAMM_TEST_EXECUTOR, _flammData(MOCK_POOL, BASE_USDC, BASE_cbBTC)
        );
        swaps[1] = encodeSequentialSwap(
            FLAMM_TEST_EXECUTOR, _flammData(MOCK_POOL, BASE_cbBTC, BASE_USDC)
        );

        deal(BASE_USDC, BOB, 10e6);
        vm.startPrank(BOB);
        IERC20(BASE_USDC).approve(tychoRouterAddr, 10e6);
        vm.expectRevert(InvalidAmount.selector);
        tychoRouter.sequentialSwap(
            10e6,
            BASE_USDC,
            BASE_USDC,
            1,
            1,
            BOB,
            noClientFee(),
            pleEncode(swaps)
        );
        vm.stopPrank();

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), 10e6);
    }

    /// @dev A clip surfaces as the pool's own error through the router, and the
    /// push comes back with the revert.
    function testAClipRevertsThroughRouter() public {
        MockFLAMMPool(MOCK_POOL).setFillCap(SALE_IN - 1);
        bytes memory swap = encodeSingleSwap(
            FLAMM_TEST_EXECUTOR, _flammData(MOCK_POOL, BASE_cbBTC, BASE_USDC)
        );
        uint256 atPool = IERC20(BASE_cbBTC).balanceOf(MOCK_POOL);
        deal(BASE_cbBTC, BOB, SALE_IN);

        vm.startPrank(BOB);
        IERC20(BASE_cbBTC).approve(tychoRouterAddr, SALE_IN);
        vm.expectRevert(InputUnused.selector);
        tychoRouter.singleSwap(
            SALE_IN, BASE_cbBTC, BASE_USDC, 1, 1, BOB, noClientFee(), swap
        );
        vm.stopPrank();

        assertEq(IERC20(BASE_cbBTC).balanceOf(BOB), SALE_IN);
        assertEq(IERC20(BASE_cbBTC).balanceOf(MOCK_POOL), atPool);
        _assertRouterFlat(MOCK_POOL);
    }

    /// @dev Anything already past the pool's custody stays there beside the
    /// push: the pool credits what the router names, no more.
    function testAStrayBesideThePushStays() public {
        uint256 stray = 7_000;
        deal(
            BASE_cbBTC,
            MOCK_POOL,
            IERC20(BASE_cbBTC).balanceOf(MOCK_POOL) + stray
        );
        deal(BASE_cbBTC, BOB, SALE_IN);

        vm.startPrank(BOB);
        IERC20(BASE_cbBTC).approve(tychoRouterAddr, SALE_IN);
        uint256 amountOut = tychoRouter.singleSwap(
            SALE_IN,
            BASE_cbBTC,
            BASE_USDC,
            SALE_OUT,
            SALE_OUT,
            BOB,
            noClientFee(),
            encodeSingleSwap(
                FLAMM_TEST_EXECUTOR,
                _flammData(MOCK_POOL, BASE_cbBTC, BASE_USDC)
            )
        );
        vm.stopPrank();

        assertEq(amountOut, SALE_OUT);
        assertEq(
            IERC20(BASE_cbBTC).balanceOf(MOCK_POOL)
                - MockFLAMMPool(MOCK_POOL).custody(BASE_cbBTC),
            stray
        );
        _assertRouterFlat(MOCK_POOL);
    }

    /// @dev The route grants nothing, so a pool that pulled reverts through the
    /// router, by cbBTC's refusal of the pull, and the input stays with the taker.
    function testAPoolThatPullsRevertsThroughRouter() public {
        _placePool("FLAMM.t.sol:MockPullingFLAMMPool", MOCK_POOL_B);
        deal(BASE_cbBTC, BOB, SALE_IN);

        vm.startPrank(BOB);
        IERC20(BASE_cbBTC).approve(tychoRouterAddr, SALE_IN);
        vm.expectRevert("ERC20: transfer amount exceeds allowance");
        tychoRouter.singleSwap(
            SALE_IN,
            BASE_cbBTC,
            BASE_USDC,
            1,
            1,
            BOB,
            noClientFee(),
            encodeSingleSwap(
                FLAMM_TEST_EXECUTOR,
                _flammData(MOCK_POOL_B, BASE_cbBTC, BASE_USDC)
            )
        );
        vm.stopPrank();

        assertEq(IERC20(BASE_cbBTC).balanceOf(BOB), SALE_IN);
        _assertRouterFlat(MOCK_POOL_B);
    }
}
