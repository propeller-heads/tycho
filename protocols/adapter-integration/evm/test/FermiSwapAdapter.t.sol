// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import "./AdapterTest.sol";
import "openzeppelin-contracts/contracts/interfaces/IERC20.sol";
import "src/fermiswap/FermiSwapAdapter.sol";
import "src/interfaces/ISwapAdapterTypes.sol";

contract FermiSwapAdapterTest is AdapterTest {
    FermiSwapAdapter adapter;

    address constant FERMI_SWAPPER = 0xb1076fE3AB5e28005C7c323Bac5AC06a680d452e;
    address constant PRIO_UPDATE_REGISTRY =
        0xDa7AfeeD01fe625CF15d187a19f94B45f00b8C5F;
    address constant TRADER_VAULT = 0x585d44727129B9C69791B10238Ca605932938B4F;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant USDT = 0xdAC17F958D2ee523a2206206994597C13D831ec7;

    uint256 constant WETH_BALANCE = 10 ether;
    uint256 constant SELL_WETH_AMOUNT = 0.1 ether;
    uint256 constant BUY_USDC_AMOUNT = 1_00e6;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 25295257);
        address fermiEngine = address(IFermiSwapper(FERMI_SWAPPER).fermi());
        _refreshFermiPair(fermiEngine, WETH, USDC);

        adapter = new FermiSwapAdapter(FERMI_SWAPPER);

        vm.label(address(adapter), "FermiSwapAdapter");
        vm.label(FERMI_SWAPPER, "FermiSwapper");
        vm.label(fermiEngine, "FermiEngine");
        vm.label(PRIO_UPDATE_REGISTRY, "PrioUpdateRegistry");
        vm.label(TRADER_VAULT, "TraderVault");
        vm.label(WETH, "WETH");
        vm.label(USDC, "USDC");
        vm.label(USDT, "USDT");
    }

    function testConstructorConfig() public view {
        assertEq(address(adapter.fermiSwapper()), FERMI_SWAPPER);
    }

    function testGetPoolIds() public view {
        bytes32[] memory poolIds = adapter.getPoolIds(0, 10);

        assertEq(poolIds.length, 5);
        assertEq(poolIds[0], _poolId(WETH, USDC));
        assertEq(poolIds[1], _poolId(WETH, USDT));
        assertEq(poolIds[2], _poolId(USDC, USDT));

        bytes32[] memory offsetPoolIds = adapter.getPoolIds(1, 10);
        assertEq(offsetPoolIds.length, 4);
        assertEq(offsetPoolIds[0], _poolId(WETH, USDT));
        assertEq(offsetPoolIds[1], _poolId(USDC, USDT));

        bytes32[] memory emptyPoolIds = adapter.getPoolIds(5, 10);
        assertEq(emptyPoolIds.length, 0);
    }

    function testGetTokens() public view {
        address[] memory tokens = adapter.getTokens(_poolId(WETH, USDC));

        assertEq(tokens.length, 2);
        assertEq(tokens[0], WETH);
        assertEq(tokens[1], USDC);
    }

    function testGetCapabilities() public view {
        Capability[] memory capabilities =
            adapter.getCapabilities(_poolId(WETH, USDC), WETH, USDC);

        assertEq(capabilities.length, 4);
        assertEq(uint256(capabilities[0]), uint256(Capability.SellOrder));
        assertEq(uint256(capabilities[1]), uint256(Capability.BuyOrder));
        assertEq(uint256(capabilities[2]), uint256(Capability.PriceFunction));
        assertEq(uint256(capabilities[3]), uint256(Capability.ConstantPrice));
    }

    function testGetLimits() public view {
        uint256[] memory limits =
            adapter.getLimits(_poolId(WETH, USDC), WETH, USDC);

        assertEq(limits.length, 2);
        assertGt(limits[0], 0);
        assertGt(limits[1], 0);

        uint256[] memory reverseLimits =
            adapter.getLimits(_poolId(WETH, USDC), USDC, WETH);
        assertEq(reverseLimits.length, 2);
        assertGt(reverseLimits[0], 0);
        assertGt(reverseLimits[1], 0);
    }

    function testPrice() public view {
        uint256[] memory amounts = new uint256[](2);
        amounts[0] = SELL_WETH_AMOUNT;
        amounts[1] = SELL_WETH_AMOUNT * 2;

        Fraction[] memory prices =
            adapter.price(_poolId(WETH, USDC), WETH, USDC, amounts);

        assertEq(prices.length, 2);
        assertGt(prices[0].numerator, 0);
        assertGt(prices[0].denominator, 0);
        assertGt(prices[1].numerator, 0);
        assertGt(prices[1].denominator, 0);
    }

    function testSwapSell() public {
        deal(WETH, address(this), SELL_WETH_AMOUNT);
        IERC20(WETH).approve(address(adapter), SELL_WETH_AMOUNT);

        uint256 wethBalance = IERC20(WETH).balanceOf(address(this));
        uint256 usdcBalance = IERC20(USDC).balanceOf(address(this));

        Trade memory trade = adapter.swap(
            _poolId(WETH, USDC), WETH, USDC, OrderSide.Sell, SELL_WETH_AMOUNT
        );

        assertEq(
            SELL_WETH_AMOUNT,
            wethBalance - IERC20(WETH).balanceOf(address(this))
        );
        assertEq(
            trade.calculatedAmount,
            IERC20(USDC).balanceOf(address(this)) - usdcBalance
        );
        assertGt(trade.calculatedAmount, 0);
        assertGt(trade.gasUsed, 0);
        assertGt(trade.price.numerator, 0);
        assertGt(trade.price.denominator, 0);
    }

    function testSwapBuy() public {
        deal(WETH, address(this), WETH_BALANCE);
        IERC20(WETH).approve(address(adapter), WETH_BALANCE);

        uint256 wethBalance = IERC20(WETH).balanceOf(address(this));
        uint256 usdcBalance = IERC20(USDC).balanceOf(address(this));

        Trade memory trade = adapter.swap(
            _poolId(WETH, USDC), WETH, USDC, OrderSide.Buy, BUY_USDC_AMOUNT
        );

        assertEq(
            trade.calculatedAmount,
            wethBalance - IERC20(WETH).balanceOf(address(this))
        );
        assertEq(
            BUY_USDC_AMOUNT, IERC20(USDC).balanceOf(address(this)) - usdcBalance
        );
        assertGt(trade.calculatedAmount, 0);
        assertGt(trade.gasUsed, 0);
        assertGt(trade.price.numerator, 0);
        assertGt(trade.price.denominator, 0);
    }

    function testRevertsOnPoolTokenMismatch() public {
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = SELL_WETH_AMOUNT;

        vm.expectRevert();
        adapter.price(_poolId(WETH, USDT), WETH, USDC, amounts);
    }

    function _poolId(address baseAsset, address quoteAsset)
        internal
        pure
        returns (bytes32)
    {
        return keccak256(abi.encodePacked(baseAsset, quoteAsset));
    }

    function _refreshFermiPair(
        address fermiEngine,
        address baseAsset,
        address quoteAsset
    ) internal {
        bytes32 laneSlot = keccak256(
            abi.encode(
                fermiEngine, uint256(_fermiPairKey(baseAsset, quoteAsset))
            )
        );
        uint256 storedLane = uint256(vm.load(PRIO_UPDATE_REGISTRY, laneSlot));
        uint256 storedSlotCount = (storedLane >> 216) & 0xff;

        assertGt(storedSlotCount, 0);
        vm.store(
            PRIO_UPDATE_REGISTRY,
            laneSlot,
            bytes32(
                (uint256(uint32(block.timestamp)) << 224)
                    | _lanePayload(storedLane)
            )
        );
    }

    function _fermiPairKey(address baseAsset, address quoteAsset)
        internal
        pure
        returns (bytes32)
    {
        return keccak256(abi.encode(baseAsset, quoteAsset));
    }

    function _lanePayload(uint256 storedLane) internal pure returns (uint256) {
        return storedLane & (type(uint256).max >> 32);
    }
}
