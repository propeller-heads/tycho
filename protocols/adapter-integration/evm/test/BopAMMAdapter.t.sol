// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import "./AdapterTest.sol";
import "openzeppelin-contracts/contracts/interfaces/IERC20.sol";
import "src/bopamm/BopAMMAdapter.sol";
import "src/interfaces/ISwapAdapterTypes.sol";

interface IPausable {
    function pause() external;
}

contract BopAMMAdapterTest is AdapterTest {
    BopAMMAdapter adapter;

    address constant SETTLEMENT = 0xdB13ad0fcD134E9c48f2fDaEa8f6751a0F5349ca;
    address constant MODULE = 0xBC60639345dFa607d73b74e88C2d54D8B8AD7Cc3;
    address constant REGISTRY = 0xDa7AfeeD01fe625CF15d187a19f94B45f00b8C5F;
    address constant MAKER = 0x6F7a3714D7FC266e3E84067AC31E7b1a3bE18060;
    address constant OWNER = 0xC5531177169b4576553Df6d4B4e176d0d7C3C826;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant WBTC = 0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599;
    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;

    // Venue errors (the contracts are unverified; selectors verified
    // on-chain).
    error StaleUpdate();
    error InsufficientLiquidity();

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"));
        _refreshBook(0);
        _refreshBook(1);

        // `quote()` only enforces the committed lane size; settlement
        // additionally transfers from the maker's wallet. Fund the maker and
        // make its allowances deterministic so swap tests exercise the full
        // lane size regardless of the live inventory at the fork block.
        deal(WETH, MAKER, 1_000 ether);
        deal(WBTC, MAKER, 100e8);
        deal(USDC, MAKER, 10_000_000e6);
        vm.startPrank(MAKER);
        IERC20(WETH).approve(SETTLEMENT, type(uint256).max);
        IERC20(WBTC).approve(SETTLEMENT, type(uint256).max);
        IERC20(USDC).approve(SETTLEMENT, type(uint256).max);
        vm.stopPrank();

        adapter = new BopAMMAdapter(SETTLEMENT);

        vm.label(address(adapter), "BopAMMAdapter");
        vm.label(SETTLEMENT, "BopAmmV2");
        vm.label(MODULE, "BopAmmPricing");
        vm.label(REGISTRY, "PrioUpdateRegistry");
        vm.label(MAKER, "Maker");
        vm.label(WETH, "WETH");
        vm.label(WBTC, "WBTC");
        vm.label(USDC, "USDC");
    }

    function testConstructorConfig() public view {
        assertEq(address(adapter.settlement()), SETTLEMENT);
        assertEq(address(adapter.pricing()), MODULE);
        assertEq(adapter.usdc(), USDC);
    }

    function testGetPoolIds() public view {
        bytes32[] memory poolIds = adapter.getPoolIds(0, 10);

        assertEq(poolIds.length, 2);
        assertEq(poolIds[0], _poolId(0));
        assertEq(poolIds[1], _poolId(1));

        bytes32[] memory offsetPoolIds = adapter.getPoolIds(1, 10);
        assertEq(offsetPoolIds.length, 1);
        assertEq(offsetPoolIds[0], _poolId(1));

        bytes32[] memory emptyPoolIds = adapter.getPoolIds(2, 10);
        assertEq(emptyPoolIds.length, 0);
    }

    function testGetTokens() public view {
        address[] memory wethBook = adapter.getTokens(_poolId(0));
        assertEq(wethBook.length, 2);
        assertEq(wethBook[0], WETH);
        assertEq(wethBook[1], USDC);

        address[] memory wbtcBook = adapter.getTokens(_poolId(1));
        assertEq(wbtcBook.length, 2);
        assertEq(wbtcBook[0], WBTC);
        assertEq(wbtcBook[1], USDC);
    }

    function testGetCapabilities() public view {
        Capability[] memory capabilities =
            adapter.getCapabilities(_poolId(0), WETH, USDC);

        assertEq(capabilities.length, 4);
        assertEq(uint256(capabilities[0]), uint256(Capability.SellOrder));
        assertEq(uint256(capabilities[1]), uint256(Capability.PriceFunction));
        assertEq(uint256(capabilities[2]), uint256(Capability.ConstantPrice));
        assertEq(uint256(capabilities[3]), uint256(Capability.HardLimits));
    }

    function testGetLimits() public view {
        uint256[] memory limits = adapter.getLimits(_poolId(0), WETH, USDC);
        assertEq(limits.length, 2);
        assertGt(limits[0], 0);
        assertGt(limits[1], 0);

        uint256[] memory reverseLimits =
            adapter.getLimits(_poolId(0), USDC, WETH);
        assertEq(reverseLimits.length, 2);
        assertGt(reverseLimits[0], 0);
        assertGt(reverseLimits[1], 0);

        uint256[] memory wbtcLimits = adapter.getLimits(_poolId(1), WBTC, USDC);
        assertEq(wbtcLimits.length, 2);
        assertGt(wbtcLimits[0], 0);
        assertGt(wbtcLimits[1], 0);
    }

    function testGetLimitsZeroWhenPaused() public {
        vm.prank(OWNER);
        IPausable(SETTLEMENT).pause();

        uint256[] memory limits = adapter.getLimits(_poolId(0), WETH, USDC);
        assertEq(limits[0], 0);
        assertEq(limits[1], 0);
    }

    function testPriceIsConstantUpToLimit() public view {
        uint256 sellLimit = adapter.getLimits(_poolId(0), WETH, USDC)[0];

        uint256[] memory amounts = new uint256[](3);
        amounts[0] = sellLimit / 1000;
        amounts[1] = sellLimit / 2;
        amounts[2] = sellLimit;

        Fraction[] memory prices =
            adapter.price(_poolId(0), WETH, USDC, amounts);

        assertEq(prices.length, 3);
        uint256 first = fractionToInt(prices[0]);
        for (uint256 i = 0; i < prices.length; i++) {
            assertGt(prices[i].numerator, 0);
            assertGt(prices[i].denominator, 0);
            // The venue quotes a constant price up to its liquidity cap.
            assertApproxEqRel(fractionToInt(prices[i]), first, 1e15);
        }
    }

    function testPriceRevertsAboveLimit() public view {
        uint256 sellLimit = adapter.getLimits(_poolId(0), WETH, USDC)[0];
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = sellLimit * 2;

        try adapter.price(_poolId(0), WETH, USDC, amounts) {
            revert("price above the limit should revert");
        } catch (bytes memory reason) {
            assertEq(bytes4(reason), InsufficientLiquidity.selector);
        }
    }

    function testPriceRevertsWhenStale() public {
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 0.01 ether;

        // Any timestamp other than the committed one trips the registry's
        // exact-timestamp gate.
        vm.warp(block.timestamp + 1);
        vm.expectRevert(StaleUpdate.selector);
        adapter.price(_poolId(0), WETH, USDC, amounts);
    }

    function testSwapSellWethForUsdc() public {
        uint256 sellAmount = adapter.getLimits(_poolId(0), WETH, USDC)[0] / 2;
        assertGt(sellAmount, 0);

        deal(WETH, address(this), sellAmount);
        IERC20(WETH).approve(address(adapter), sellAmount);

        uint256 wethBefore = IERC20(WETH).balanceOf(address(this));
        uint256 usdcBefore = IERC20(USDC).balanceOf(address(this));

        Trade memory trade =
            adapter.swap(_poolId(0), WETH, USDC, OrderSide.Sell, sellAmount);

        assertEq(sellAmount, wethBefore - IERC20(WETH).balanceOf(address(this)));
        assertEq(
            trade.calculatedAmount,
            IERC20(USDC).balanceOf(address(this)) - usdcBefore
        );
        assertGt(trade.calculatedAmount, 0);
        assertGt(trade.gasUsed, 0);
        assertGt(trade.price.numerator, 0);
        assertGt(trade.price.denominator, 0);
    }

    function testSwapSellUsdcForWeth() public {
        uint256 sellAmount = adapter.getLimits(_poolId(0), USDC, WETH)[0] / 2;
        assertGt(sellAmount, 0);

        deal(USDC, address(this), sellAmount);
        IERC20(USDC).approve(address(adapter), sellAmount);

        uint256 usdcBefore = IERC20(USDC).balanceOf(address(this));
        uint256 wethBefore = IERC20(WETH).balanceOf(address(this));

        Trade memory trade =
            adapter.swap(_poolId(0), USDC, WETH, OrderSide.Sell, sellAmount);

        assertEq(sellAmount, usdcBefore - IERC20(USDC).balanceOf(address(this)));
        assertEq(
            trade.calculatedAmount,
            IERC20(WETH).balanceOf(address(this)) - wethBefore
        );
        assertGt(trade.calculatedAmount, 0);
        assertGt(trade.gasUsed, 0);
    }

    function testSwapBuyNotImplemented() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                ISwapAdapterTypes.NotImplemented.selector,
                "BopAMM quotes are exact-input only"
            )
        );
        adapter.swap(_poolId(0), WETH, USDC, OrderSide.Buy, 1 ether);
    }

    function testRevertsOnPoolTokenMismatch() public {
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 0.01 ether;

        vm.expectRevert(
            abi.encodeWithSelector(
                ISwapAdapterTypes.InvalidOrder.selector, "Pool/token mismatch"
            )
        );
        adapter.price(_poolId(0), WBTC, USDC, amounts);
    }

    function testRevertsOnForeignSettlementPoolId() public {
        bytes32 foreignPoolId = bytes32(bytes20(WETH));
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 0.01 ether;

        vm.expectRevert(
            abi.encodeWithSelector(
                ISwapAdapterTypes.InvalidOrder.selector,
                "Pool id settlement mismatch"
            )
        );
        adapter.price(foreignPoolId, WETH, USDC, amounts);
    }

    /// @dev Pool id convention: `settlement (20 bytes) | assetId (12 bytes)`,
    /// matching the substreams component id.
    function _poolId(uint256 assetId) internal pure returns (bytes32) {
        return bytes32(bytes20(SETTLEMENT)) | bytes32(assetId);
    }

    /// @dev The registry's exact-timestamp gate (`MAX_UPDATE_AGE == 0`)
    /// requires `block.timestamp` to equal the committed update timestamp.
    /// Re-stamp the book's committed lane to the fork's timestamp, keeping
    /// the committed price data intact.
    function _refreshBook(uint256 bookId) internal {
        bytes32 laneSlot = keccak256(abi.encode(MODULE, bookId));
        uint256 storedLane = uint256(vm.load(REGISTRY, laneSlot));
        assertGt(storedLane, 0, "book lane should exist");

        uint256 payload = storedLane & (type(uint256).max >> 32);
        vm.store(
            REGISTRY,
            laneSlot,
            bytes32((uint256(uint32(block.timestamp)) << 224) | payload)
        );
    }
}
