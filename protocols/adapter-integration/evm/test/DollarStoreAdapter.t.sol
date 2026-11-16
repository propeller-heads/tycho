// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import "./AdapterTest.sol";
import "openzeppelin-contracts/contracts/interfaces/IERC20.sol";
import "src/dollarstore/DollarStoreAdapter.sol";

interface IDollarStoreAdmin {
    function pause() external;

    function governor() external view returns (address);

    function removePool(uint16 poolId) external;

    function maxStaleness() external view returns (uint256);

    function swapOrQueue(
        address offerAsset,
        address wantAsset,
        uint256 amount,
        uint256 minAmountOut,
        uint256 deadline
    )
        external
        returns (
            uint256 amountIn,
            uint256 amountOut,
            uint256 amountInQueued,
            uint256 positionId
        );
}

contract DollarStoreAdapterTest is AdapterTest {
    DollarStoreAdapter adapter;

    address constant STORE = 0x825e1e13D804816b8ccdA405F1a7B7915461dDc4;
    address constant PAUSER = 0x50A8Ce671Ca3d343C3EE06D69101a78Cc89bd919;
    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant USDS = 0xdC035D45d973E3EC169d2276DDab16f1e407384F;
    // Spoke pool 1 holds USDG, spoke pool 2 holds PYUSD (empty at this
    // block).
    address constant USDG = 0xe343167631d89B6Ffc58B88d6b7fB0228795491D;
    address constant PYUSD = 0x6c3ea9036406852006290770BEdFcAbA0e23A0e8;

    // Hub: ~1.46M USDC, ~0.62M USDS. USDG spoke: ~1.93M USDG. Every feed
    // fresh and on peg.
    uint256 constant FORK_BLOCK = 26424000;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);
        adapter = new DollarStoreAdapter(STORE);
        vm.label(address(adapter), "DollarStoreAdapter");
        vm.label(STORE, "DollarStore");
    }

    function _pairs() internal pure returns (address[2][6] memory) {
        return [
            [USDC, USDS],
            [USDS, USDC],
            [USDG, USDC],
            [USDC, USDG],
            [USDG, USDS],
            [USDS, USDG]
        ];
    }

    // keccak256(abi.encodePacked(store, token0, token1)), from `cast keccak`;
    // the substreams package pins the same values.
    bytes32 constant USDC_USDS_ID =
        0x48883064a1a16978e8857f235d726998fd83be81c77ef5303f7b5d151b7e0b1a;
    bytes32 constant USDC_USDG_ID =
        0x097cc630397efe83d759e75de6f9f996bb350abdd015a099f837a2a69c42d5e4;
    bytes32 constant USDS_USDG_ID =
        0xc75d44634bc602c1fc11c34a06c53d64b96a06828594d761017663d63cc3f434;

    function testPoolIdOfIsOrderIndependent() public view {
        assertEq(adapter.poolIdOf(USDC, USDS), USDC_USDS_ID);
        assertEq(adapter.poolIdOf(USDG, USDC), USDC_USDG_ID);
        assertEq(adapter.poolIdOf(USDC, USDG), USDC_USDG_ID);
        assertEq(adapter.poolIdOf(USDG, USDS), USDS_USDG_ID);
    }

    function testPoolIdsListEveryOneLegPair() public view {
        bytes32[] memory ids = adapter.getPoolIds(0, 100);
        // USDC/USDS, then USDG and PYUSD against both hub assets.
        assertEq(ids.length, 5);
        assertEq(ids[0], USDC_USDS_ID);
        assertEq(ids[1], USDC_USDG_ID);
        assertEq(ids[2], USDS_USDG_ID);
        assertEq(ids[3], adapter.poolIdOf(PYUSD, USDC));
        assertEq(ids[4], adapter.poolIdOf(PYUSD, USDS));
        assertEq(adapter.getPoolIds(4, 100).length, 1);
        assertEq(adapter.getPoolIds(5, 100).length, 0);
    }

    function testGetTokensReturnsSortedPair() public view {
        address[] memory tokens = adapter.getTokens(USDC_USDG_ID);
        assertEq(tokens[0], USDC);
        assertEq(tokens[1], USDG);
    }

    function testGetTokensRejectsUnknownPool() public {
        vm.expectRevert(
            abi.encodeWithSelector(InvalidOrder.selector, "Unknown pool")
        );
        adapter.getTokens(bytes32(uint256(1)));
    }

    function testRemovedSpokeIsNotListed() public {
        // The PYUSD spoke (pool 2) is empty, so the governor can remove it.
        vm.prank(IDollarStoreAdmin(STORE).governor());
        IDollarStoreAdmin(STORE).removePool(2);
        assertEq(adapter.getPoolIds(0, 100).length, 3);
    }

    function testLimitsAreTheQuotedMaximum() public view {
        address[2][6] memory pairs = _pairs();
        for (uint256 i = 0; i < pairs.length; i++) {
            (address sell, address buy) = (pairs[i][0], pairs[i][1]);
            uint256[] memory limits =
                adapter.getLimits(adapter.poolIdOf(sell, buy), sell, buy);
            assertGt(limits[0], 0, "sell limit");
            assertEq(
                limits[1],
                IDollarStore(STORE).getSwapQuote(sell, buy, type(uint256).max)
            );
            // The whole sell limit is fillable.
            assertEq(
                IDollarStore(STORE).getSwapQuote(sell, buy, limits[0]),
                limits[1]
            );
        }
    }

    function testSwapAtLimitFills() public {
        address[2][6] memory pairs = _pairs();
        for (uint256 i = 0; i < pairs.length; i++) {
            (address sell, address buy) = (pairs[i][0], pairs[i][1]);
            bytes32 poolId = adapter.poolIdOf(sell, buy);
            uint256 snap = vm.snapshot();
            uint256[] memory limits = adapter.getLimits(poolId, sell, buy);
            deal(sell, address(this), limits[0]);
            IERC20(sell).approve(address(adapter), type(uint256).max);

            uint256 before = IERC20(buy).balanceOf(address(this));
            Trade memory trade =
                adapter.swap(poolId, sell, buy, OrderSide.Sell, limits[0]);
            assertEq(trade.calculatedAmount, limits[1]);
            assertEq(IERC20(buy).balanceOf(address(this)) - before, limits[1]);

            // The pair is now drained in this direction.
            assertEq(adapter.getLimits(poolId, sell, buy)[0], 0);
            vm.revertTo(snap);
        }
    }

    function testSwapOneUnitPastLimitReverts() public {
        address[2][6] memory pairs = _pairs();
        for (uint256 i = 0; i < pairs.length; i++) {
            (address sell, address buy) = (pairs[i][0], pairs[i][1]);
            bytes32 poolId = adapter.poolIdOf(sell, buy);
            uint256 limit = adapter.getLimits(poolId, sell, buy)[0];
            uint256 unit = IDollarStore(STORE).assetScalingFactor(sell);
            deal(sell, address(this), limit + unit);
            IERC20(sell).approve(address(adapter), type(uint256).max);
            vm.expectRevert(
                abi.encodeWithSelector(LimitExceeded.selector, limit)
            );
            adapter.swap(poolId, sell, buy, OrderSide.Sell, limit + unit);
        }
    }

    function testSwapGasIsMeasuredCold() public {
        // swapExactInput on a hub pair, nothing in the store read before it.
        deal(USDC, address(this), 1000e6);
        IERC20(USDC).approve(address(adapter), type(uint256).max);
        Trade memory trade =
            adapter.swap(USDC_USDS_ID, USDC, USDS, OrderSide.Sell, 1000e6);
        console2.log("DollarStore swap gas", trade.gasUsed);
        assertGt(trade.gasUsed, 100_000);
    }

    function testSwapIsOneToOneInUnits() public {
        bytes32 poolId = adapter.poolIdOf(USDC, USDS);
        deal(USDC, address(this), 1000e6);
        IERC20(USDC).approve(address(adapter), type(uint256).max);
        Trade memory trade =
            adapter.swap(poolId, USDC, USDS, OrderSide.Sell, 1000e6);
        assertEq(trade.calculatedAmount, 1000e18);
        assertEq(trade.price.numerator, 1e12);
        assertEq(trade.price.denominator, 1);

        // 18-decimal input: the sub-unit remainder is not taken.
        deal(USDS, address(this), 5e18 + 123);
        IERC20(USDS).approve(address(adapter), type(uint256).max);
        trade = adapter.swap(poolId, USDS, USDC, OrderSide.Sell, 5e18 + 123);
        assertEq(trade.calculatedAmount, 5e6);
        assertEq(IERC20(USDS).balanceOf(address(this)), 123);
        assertEq(IERC20(USDS).balanceOf(address(adapter)), 0);
    }

    function testBelowOneUnitIsTooSmall() public {
        bytes32 poolId = adapter.poolIdOf(USDS, USDC);
        deal(USDS, address(this), 1e12);
        IERC20(USDS).approve(address(adapter), type(uint256).max);
        vm.expectRevert(abi.encodeWithSelector(TooSmall.selector, 1e12));
        adapter.swap(poolId, USDS, USDC, OrderSide.Sell, 1e12 - 1);
    }

    function testZeroAmountIsAnEmptyTrade() public {
        vm.prank(PAUSER);
        IDollarStoreAdmin(STORE).pause();
        bytes32 poolId = adapter.poolIdOf(USDS, USDC);
        Trade memory trade = adapter.swap(poolId, USDS, USDC, OrderSide.Sell, 0);
        assertEq(trade.calculatedAmount, 0);
        assertEq(trade.gasUsed, 0);
        assertEq(trade.price.numerator, 1);
        assertEq(trade.price.denominator, 1e12);
    }

    function testBuyOrderNotImplemented() public {
        bytes32 poolId = adapter.poolIdOf(USDC, USDS);
        vm.expectRevert(
            abi.encodeWithSelector(
                NotImplemented.selector,
                "DollarStore swaps are exact-input only"
            )
        );
        adapter.swap(poolId, USDC, USDS, OrderSide.Buy, 1e18);
    }

    function testPriceIsConstantUpToLimit() public view {
        bytes32 poolId = adapter.poolIdOf(USDS, USDG);
        uint256 limit = adapter.getLimits(poolId, USDS, USDG)[0];
        uint256[] memory amounts = new uint256[](3);
        amounts[0] = 0;
        amounts[1] = 1e18;
        amounts[2] = limit;
        Fraction[] memory prices = adapter.price(poolId, USDS, USDG, amounts);
        for (uint256 i = 0; i < prices.length; i++) {
            // 1 USDG (6 dp) per USDS (18 dp).
            assertEq(prices[i].numerator, 1);
            assertEq(prices[i].denominator, 1e12);
        }
    }

    function testPriceAboveLimitReverts() public {
        bytes32 poolId = adapter.poolIdOf(USDC, USDG);
        uint256 limit = adapter.getLimits(poolId, USDC, USDG)[0];
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = limit + 1;
        vm.expectRevert(abi.encodeWithSelector(LimitExceeded.selector, limit));
        adapter.price(poolId, USDC, USDG, amounts);
    }

    function testEmptySpokeQuotesPriceAtZeroLimit() public view {
        bytes32 poolId = adapter.poolIdOf(PYUSD, USDC);
        assertEq(adapter.getLimits(poolId, USDC, PYUSD)[0], 0);
        assertEq(adapter.getLimits(poolId, PYUSD, USDC)[0], 0);
        uint256[] memory amounts = new uint256[](1);
        Fraction[] memory prices = adapter.price(poolId, PYUSD, USDC, amounts);
        assertEq(prices[0].numerator, 1);
        assertEq(prices[0].denominator, 1);
    }

    function testPausedStoreHasZeroLimits() public {
        vm.prank(PAUSER);
        IDollarStoreAdmin(STORE).pause();
        address[2][6] memory pairs = _pairs();
        for (uint256 i = 0; i < pairs.length; i++) {
            (address sell, address buy) = (pairs[i][0], pairs[i][1]);
            uint256[] memory limits =
                adapter.getLimits(adapter.poolIdOf(sell, buy), sell, buy);
            assertEq(limits[0], 0);
            assertEq(limits[1], 0);
        }
    }

    function testStaleFeedStopsSellingThatAsset() public {
        // Past maxStaleness every feed is stale, so no asset can be sold in.
        vm.warp(block.timestamp + IDollarStoreAdmin(STORE).maxStaleness() + 1);
        bytes32 poolId = adapter.poolIdOf(USDC, USDS);
        assertEq(adapter.getLimits(poolId, USDC, USDS)[0], 0);
        assertEq(adapter.getLimits(poolId, USDS, USDC)[0], 0);
    }

    function testOppositeQueueRaisesTheLimit() public {
        bytes32 poolId = adapter.poolIdOf(USDG, USDC);
        uint256 usdcToUsdg = adapter.getLimits(poolId, USDC, USDG)[0];
        uint256 usdgToUsdc = adapter.getLimits(poolId, USDG, USDC)[0];

        // Sell more USDG than the store can pay in USDC: the rest queues as
        // a USDG -> USDC position.
        uint256 queued = 50_000e6;
        deal(USDG, address(this), usdgToUsdc + queued);
        IERC20(USDG).approve(STORE, type(uint256).max);
        (,, uint256 amountInQueued,) = IDollarStoreAdmin(STORE)
            .swapOrQueue(USDG, USDC, usdgToUsdc + queued, 0, block.timestamp);
        assertEq(amountInQueued, queued);

        // The filled USDG joined the spoke reserve, and a USDC -> USDG swap
        // also fills the queued position, so both raise that limit. The
        // store has no USDC left to sell for USDG.
        assertEq(
            adapter.getLimits(poolId, USDC, USDG)[0],
            usdcToUsdg + usdgToUsdc + queued
        );
        assertEq(adapter.getLimits(poolId, USDG, USDC)[0], 0);

        uint256 amount = usdcToUsdg + usdgToUsdc + queued;
        deal(USDC, address(this), amount);
        IERC20(USDC).approve(address(adapter), type(uint256).max);
        Trade memory trade =
            adapter.swap(poolId, USDC, USDG, OrderSide.Sell, amount);
        assertEq(trade.calculatedAmount, amount);
    }

    function testWrongPoolIdReverts() public {
        bytes32 poolId = adapter.poolIdOf(USDC, USDS);
        vm.expectRevert(
            abi.encodeWithSelector(InvalidOrder.selector, "Pool/token mismatch")
        );
        adapter.getLimits(poolId, USDC, USDG);
        vm.expectRevert(
            abi.encodeWithSelector(InvalidOrder.selector, "Pool/token mismatch")
        );
        adapter.getLimits(poolId, USDC, USDC);
    }
}
