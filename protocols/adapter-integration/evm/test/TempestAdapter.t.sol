// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import "./AdapterTest.sol";
import "openzeppelin-contracts/contracts/interfaces/IERC20.sol";
import "src/tempest/TempestAdapter.sol";
import "src/interfaces/ISwapAdapterTypes.sol";

contract TempestAdapterTest is AdapterTest {
    TempestAdapter adapter;

    address constant TEMPEST_ROUTER =
        0x00000003f1ec2379e79F58E12EC6C4F51Ee92149;
    address constant TEMPEST_VAULT = 0xC9d748e601d9984A43Da0b80E5b91dc28d31d9fB;
    address constant PRIO_UPDATE_REGISTRY =
        0xDa7AfEeD021EAFC1c1Af9C362dE477DaD0396B81;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant USDT = 0xdAC17F958D2ee523a2206206994597C13D831ec7;

    uint256 constant SELL_WETH_AMOUNT = 0.1 ether;
    uint256 constant BUY_USDC_AMOUNT = 100e6;

    function setUp() public {
        // A block in which the maker committed lanes, so they are inside the
        // router's freshness window without the test restamping anything.
        // Past the upgrade at 25744018 that turned on `openTakerAccess`, so
        // `swap` settles from the adapter's own address without an
        // `allowedTaker` entry -- the gate is intact, just bypassed -- and past
        // the registry switch at 25989123, so this runs against the registry
        // the router reads now.
        vm.createSelectFork(vm.rpcUrl("mainnet"), 26044074);

        adapter = new TempestAdapter(TEMPEST_ROUTER);

        vm.label(address(adapter), "TempestAdapter");
        vm.label(TEMPEST_ROUTER, "TempestRouter");
        vm.label(TEMPEST_VAULT, "TempestVault");
        vm.label(PRIO_UPDATE_REGISTRY, "PrioUpdateRegistry");
        vm.label(WETH, "WETH");
        vm.label(USDC, "USDC");
        vm.label(USDT, "USDT");
    }

    function testConstructorConfig() public view {
        assertEq(address(adapter.tempest()), TEMPEST_ROUTER);
    }

    /// The pool id must be router-scoped, matching the component id the
    /// substreams package emits. It must therefore NOT equal the router's own
    /// `laneFor`, which is only keccak(token0, token1) and so would collide
    /// with any other pAMM quoting the same pair.
    function testPoolIdIsRouterScoped() public pure {
        assertTrue(
            _poolId(USDC, WETH)
                != bytes32(ITempest(TEMPEST_ROUTER).laneFor(USDC, WETH))
        );
        assertEq(
            _poolId(USDC, WETH),
            keccak256(abi.encodePacked(TEMPEST_ROUTER, USDC, WETH))
        );
        // Direction-independent, because the pair is sorted.
        assertEq(_poolId(USDC, WETH), _poolId(WETH, USDC));
    }

    function testGetPoolIds() public view {
        bytes32[] memory poolIds = adapter.getPoolIds(0, 10);

        // Registration order: WETH/USDT (25637575), USDC/WETH (25637589),
        // USDC/USDT (25637601).
        assertEq(poolIds.length, 3);
        assertEq(poolIds[0], _poolId(WETH, USDT));
        assertEq(poolIds[1], _poolId(USDC, WETH));
        assertEq(poolIds[2], _poolId(USDC, USDT));

        bytes32[] memory offsetPoolIds = adapter.getPoolIds(1, 10);
        assertEq(offsetPoolIds.length, 2);
        assertEq(offsetPoolIds[0], _poolId(USDC, WETH));

        assertEq(adapter.getPoolIds(3, 10).length, 0);
    }

    function testGetTokens() public view {
        address[] memory tokens = adapter.getTokens(_poolId(USDC, WETH));

        assertEq(tokens.length, 2);
        assertEq(tokens[0], USDC);
        assertEq(tokens[1], WETH);
    }

    function testGetTokensRevertsOnUnknownPool() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                ISwapAdapterTypes.InvalidOrder.selector, "Unknown pool"
            )
        );
        adapter.getTokens(keccak256("not a pool"));
    }

    function testGetCapabilities() public view {
        Capability[] memory capabilities =
            adapter.getCapabilities(_poolId(USDC, WETH), WETH, USDC);

        assertEq(capabilities.length, 2);
        assertEq(uint256(capabilities[0]), uint256(Capability.SellOrder));
        assertEq(uint256(capabilities[1]), uint256(Capability.BuyOrder));

        // Neither ConstantPrice nor PriceFunction may be declared: the lane is
        // a VWAP spread ladder, so an amount crossing a breakpoint gets a worse
        // rate, and the adapter has no way to report a marginal price. Leaving
        // PriceFunction off makes simulation derive it numerically from `swap`.
        for (uint256 i = 0; i < capabilities.length; i++) {
            assertTrue(capabilities[i] != Capability.ConstantPrice);
            assertTrue(capabilities[i] != Capability.PriceFunction);
        }
    }

    function testPriceReverts() public {
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = SELL_WETH_AMOUNT;

        vm.expectRevert();
        adapter.price(_poolId(USDC, WETH), WETH, USDC, amounts);
    }

    /// `swap` must report no marginal price, but with a non-zero denominator:
    /// simulation divides the fraction and treats a zero denominator as a fatal
    /// error, which would fail the swap.
    function testSwapReportsUnsetPrice() public {
        _fund(WETH, SELL_WETH_AMOUNT);
        Trade memory trade = adapter.swap(
            _poolId(USDC, WETH), WETH, USDC, OrderSide.Sell, SELL_WETH_AMOUNT
        );

        assertEq(trade.price.numerator, 0);
        assertEq(trade.price.denominator, 1);
    }

    function testSwapSell() public {
        _fund(WETH, SELL_WETH_AMOUNT);
        uint256 usdcBefore = IERC20(USDC).balanceOf(address(this));
        Trade memory trade = adapter.swap(
            _poolId(USDC, WETH), WETH, USDC, OrderSide.Sell, SELL_WETH_AMOUNT
        );

        // Selling 0.1 WETH must return a plausible USDC amount (6 decimals).
        assertGt(trade.calculatedAmount, 0);
        assertGt(trade.gasUsed, 0);
        // The swap really settled: the venue paid this contract.
        assertEq(
            IERC20(USDC).balanceOf(address(this)) - usdcBefore,
            trade.calculatedAmount
        );
    }

    function testSwapBuy() public {
        _fund(WETH, SELL_WETH_AMOUNT);
        Trade memory trade = adapter.swap(
            _poolId(USDC, WETH), WETH, USDC, OrderSide.Buy, BUY_USDC_AMOUNT
        );

        // Exact-output returns the WETH input needed for 100 USDC.
        assertGt(trade.calculatedAmount, 0);
        assertGt(trade.gasUsed, 0);
    }

    function testSwapZeroAmountIsNoop() public {
        Trade memory trade =
            adapter.swap(_poolId(USDC, WETH), WETH, USDC, OrderSide.Sell, 0);

        assertEq(trade.calculatedAmount, 0);
        assertEq(trade.gasUsed, 0);
    }

    function testGetLimits() public view {
        uint256[] memory limits =
            adapter.getLimits(_poolId(USDC, WETH), WETH, USDC);

        assertEq(limits.length, 2);
        assertGt(limits[0], 0);
        assertGt(limits[1], 0);
        // The buy-side limit can never exceed the vault's payable inventory.
        assertLe(limits[1], IERC20(USDC).balanceOf(TEMPEST_VAULT));
    }

    /// A second pair carries a fresh lane at this block too, so it must quote
    /// as well as USDC/WETH.
    function testGetLimitsSecondPair() public view {
        uint256[] memory limits =
            adapter.getLimits(_poolId(USDC, USDT), USDC, USDT);

        assertGt(limits[0], 0);
        assertGt(limits[1], 0);
    }

    /// A stale lane makes the pair inactive, so limits must be zero and the
    /// adapter must not revert.
    function testGetLimitsStaleLaneIsZero() public {
        // Past `laneWindow`'s max age, so `getState` reverts `StaleUpdate`.
        vm.warp(block.timestamp + 1 hours);

        uint256[] memory limits =
            adapter.getLimits(_poolId(USDC, WETH), WETH, USDC);

        assertEq(limits[0], 0);
        assertEq(limits[1], 0);
    }

    function testRevertsOnPoolTokenMismatch() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                ISwapAdapterTypes.InvalidOrder.selector, "Pool/token mismatch"
            )
        );
        adapter.getLimits(_poolId(USDC, WETH), WETH, USDT);
    }

    /// Gives this contract `amount` of `token` and lets the adapter pull it.
    function _fund(address token, uint256 amount) internal {
        deal(token, address(this), amount);
        IERC20(token).approve(address(adapter), type(uint256).max);
    }

    /// Mirrors the component id the substreams package emits: keccak of the
    /// router followed by the ascending-sorted packed pair.
    function _poolId(address tokenA, address tokenB)
        internal
        pure
        returns (bytes32)
    {
        (address token0, address token1) =
            tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);
        return keccak256(abi.encodePacked(TEMPEST_ROUTER, token0, token1));
    }
}
