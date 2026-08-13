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
        0xDa7AfeeD01fe625CF15d187a19f94B45f00b8C5F;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant USDT = 0xdAC17F958D2ee523a2206206994597C13D831ec7;

    uint256 constant SELL_WETH_AMOUNT = 0.1 ether;
    uint256 constant BUY_USDC_AMOUNT = 100e6;

    function setUp() public {
        // The maker's last committed lane for USDC/WETH. Lane payloads persist
        // in registry storage after the commit, so the ladder is readable here;
        // only the timestamp goes stale, which _refreshLane restamps.
        vm.createSelectFork(vm.rpcUrl("mainnet"), 25673715);
        _refreshLane(USDC, WETH);

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

    /// The adapter's pool id must equal the router's own `laneFor`, which is
    /// also the component id the substreams package emits.
    function testPoolIdMatchesLaneFor() public pure {
        assertEq(
            _poolId(USDC, WETH),
            bytes32(ITempest(TEMPEST_ROUTER).laneFor(USDC, WETH))
        );
        // Direction-independent, because the router sorts the pair.
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

        assertEq(capabilities.length, 3);
        assertEq(uint256(capabilities[0]), uint256(Capability.SellOrder));
        assertEq(uint256(capabilities[1]), uint256(Capability.BuyOrder));
        assertEq(uint256(capabilities[2]), uint256(Capability.PriceFunction));

        // ConstantPrice must NOT be declared: the lane is a VWAP spread ladder,
        // so an amount crossing a breakpoint gets a worse rate. The impact
        // itself is not asserted here -- see the note on `getCapabilities` in
        // TempestAdapter for why it is unreachable at this block.
        for (uint256 i = 0; i < capabilities.length; i++) {
            assertTrue(capabilities[i] != Capability.ConstantPrice);
        }
    }

    function testPrice() public view {
        uint256[] memory amounts = new uint256[](2);
        amounts[0] = SELL_WETH_AMOUNT;
        amounts[1] = SELL_WETH_AMOUNT * 2;

        Fraction[] memory prices =
            adapter.price(_poolId(USDC, WETH), WETH, USDC, amounts);

        assertEq(prices.length, 2);
        for (uint256 i = 0; i < prices.length; i++) {
            assertGt(prices[i].numerator, 0);
            assertGt(prices[i].denominator, 0);
        }
    }

    function testSwapSell() public view {
        Trade memory trade = adapter.swap(
            _poolId(USDC, WETH), WETH, USDC, OrderSide.Sell, SELL_WETH_AMOUNT
        );

        // Selling 0.1 WETH must return a plausible USDC amount (6 decimals).
        assertGt(trade.calculatedAmount, 0);
        assertGt(trade.gasUsed, 0);
    }

    function testSwapBuy() public view {
        Trade memory trade = adapter.swap(
            _poolId(USDC, WETH), WETH, USDC, OrderSide.Buy, BUY_USDC_AMOUNT
        );

        // Exact-output returns the WETH input needed for 100 USDC.
        assertGt(trade.calculatedAmount, 0);
        assertGt(trade.gasUsed, 0);
    }

    function testSwapZeroAmountIsNoop() public view {
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

    /// A pair with no lane ever committed must report zero limits rather than
    /// bubbling up the router's `StaleUpdate` revert.
    function testGetLimitsUnquotedPairIsZero() public view {
        uint256[] memory limits =
            adapter.getLimits(_poolId(USDC, USDT), USDC, USDT);

        assertEq(limits[0], 0);
        assertEq(limits[1], 0);
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

    /// Restamps a committed lane to the current block so `getState` passes the
    /// router's freshness window. Mirrors what the builder does by ordering the
    /// maker's quote tx immediately ahead of the fill in the same block.
    function _refreshLane(address tokenA, address tokenB) internal {
        bytes32 laneSlot = keccak256(
            abi.encode(TEMPEST_ROUTER, uint256(_poolId(tokenA, tokenB)))
        );
        uint256 storedLane = uint256(vm.load(PRIO_UPDATE_REGISTRY, laneSlot));
        uint256 storedSlotCount = (storedLane >> 216) & 0xff;

        // Guards against the fork block silently having no committed ladder,
        // which would make every assertion below vacuous.
        assertGt(storedSlotCount, 0);

        vm.store(
            PRIO_UPDATE_REGISTRY,
            laneSlot,
            bytes32(
                (uint256(uint32(block.timestamp)) << 224)
                    | (storedLane & ((uint256(1) << 224) - 1))
            )
        );
    }

    /// Mirrors `Tempest.laneFor`: keccak of the ascending-sorted packed pair.
    function _poolId(address tokenA, address tokenB)
        internal
        pure
        returns (bytes32)
    {
        (address token0, address token1) =
            tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);
        return keccak256(abi.encodePacked(token0, token1));
    }
}
