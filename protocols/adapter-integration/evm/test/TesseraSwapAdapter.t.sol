// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.27;
import "forge-std/Test.sol";
import {
    TesseraSwapAdapter,
    ITesseraSwap,
    ITesseraPair
} from "src/tessera/TesseraSwapAdapter.sol";
import {ISwapAdapterTypes} from "src/interfaces/ISwapAdapterTypes.sol";
import {IERC20} from "openzeppelin-contracts/contracts/token/ERC20/IERC20.sol";

/// Engine stand-in that registers `pair` but quotes only up to 5e15 input, at
/// one output unit per 1e9 input units, and zero beyond.
contract LimitedTesseraEngine {
    address immutable pair;

    constructor(address pair_) {
        pair = pair_;
    }

    function getTesseraPool(address, address)
        external
        view
        returns (bool, address)
    {
        return (true, pair);
    }

    function fastPullBlock() external pure returns (uint256) {
        return 0;
    }

    function swapAmountView(address, address, int256 amount, address)
        external
        pure
        returns (uint256 input, uint256 output)
    {
        if (amount < 0) {
            output = uint256(-amount);
            input = output * 1e9;
        } else {
            input = uint256(amount);
            output = input / 1e9;
        }
        if (input > 5e15) output = 0;
    }
}

// Synthetic venue: the view quote is pre-fee, but settlement deducts 10%.
// This isolates the adapter's accounting from the real venue's currently zero
// tag-0 fee.
/// It also stands in for the Engine, whose view quote the adapter prices with.
contract DiscountedTesseraSettlement {
    address immutable pair;

    constructor(address pair_) {
        pair = pair_;
    }

    function getTesseraPool(address, address)
        external
        view
        returns (bool, address)
    {
        return (true, pair);
    }

    function swapAmountView(address, address, int256 amount, address)
        external
        pure
        returns (uint256, uint256)
    {
        uint256 input =
            amount < 0 ? (uint256(-amount) + 1) / 2 : uint256(amount);
        return (input, amount < 0 ? uint256(-amount) : input * 2);
    }

    function tesseraSwapWithAllowances(
        address sell,
        address buy,
        int256 amount,
        uint256,
        address recipient,
        bytes calldata data
    ) external {
        require(data.length == 0, "expected tag zero");
        uint256 input =
            amount < 0 ? (uint256(-amount) + 1) / 2 : uint256(amount);
        uint256 output = amount < 0 ? uint256(-amount) : input * 2;
        require(IERC20(sell).transferFrom(msg.sender, address(this), input));
        require(IERC20(buy).transfer(recipient, output * 9 / 10));
    }
}

contract TesseraSwapAdapterTest is Test {
    address constant VENUE = 0x55555522005BcAE1c2424D474BfD5ed477749E3e;
    address constant PAIR = 0xf524C1Bc1C64A2C99bc7eccf19EDe9a1d89d5a7C;
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant ENGINE = 0x31e99E05fee3DCE580af777C3fD63eE1B3B40c17;
    address constant EURC = 0x60a3E35Cc302bFA44Cb288Bc5a4F316Fdb1adb42;
    address constant CBBTC = 0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf;
    address constant BTC_PAIR = 0xED57BacDc2a990B631F8817853935791C122c356;
    address constant TREASURY = 0x3dBE077e7986657E95e1CC50089f17a5a4AF0AaE;
    // keccak256(abi.encode(keccak256(abi.encode(WETH, USDC)), 8)): the Engine
    // registry entry for the sorted WETH/USDC pair.
    bytes32 constant WETH_USDC_REGISTRY_SLOT =
        0x7de34ae1e8ba739875fb9abb94e6baebe5cefd45e23d860202def0971a284fe7;
    TesseraSwapAdapter adapter;
    bytes32 poolId = bytes32(bytes20(PAIR));

    function setUp() public {
        vm.createSelectFork(vm.envString("BASE_RPC_URL"), 50_548_423);
        adapter = new TesseraSwapAdapter(VENUE, ENGINE, TREASURY);
    }

    function testTokensCapabilitiesAndLimits() public {
        address[] memory tokens = adapter.getTokens(poolId);
        assertEq(tokens[0], WETH);
        assertEq(tokens[1], USDC);
        assertEq(adapter.getCapabilities(poolId, WETH, USDC).length, 4);
        for (uint256 i; i < 2; ++i) {
            (address sell, address buy) = i == 0 ? (WETH, USDC) : (USDC, WETH);
            uint256 gasBefore = gasleft();
            uint256[] memory limits = adapter.getLimits(poolId, sell, buy);
            uint256 used = gasBefore - gasleft();
            emit log_named_uint("getLimits gas", used);
            assertLe(used, 3_000_000, "getLimits budget");
            assertGt(limits[0], 0);
            assertGt(limits[1], 0);
            (, uint256 quoted) = ITesseraSwap(VENUE)
                .tesseraSwapViewAmounts(sell, buy, int256(limits[0]));
            assertEq(limits[1], quoted);
        }
    }

    function _swap(
        address sell,
        address buy,
        ISwapAdapterTypes.OrderSide side,
        uint256 amount
    ) internal {
        int256 specified = side == ISwapAdapterTypes.OrderSide.Sell
            ? int256(amount)
            : -int256(amount);
        (uint256 input, uint256 output) =
            ITesseraSwap(VENUE).tesseraSwapViewAmounts(sell, buy, specified);
        assertGt(output, 0);
        deal(sell, address(this), input);
        IERC20(sell).approve(address(adapter), input);
        uint256 beforeOut = IERC20(buy).balanceOf(address(this));
        ISwapAdapterTypes.Trade memory trade =
            adapter.swap(poolId, sell, buy, side, amount);
        assertEq(IERC20(buy).balanceOf(address(this)) - beforeOut, output);
        assertEq(
            trade.calculatedAmount,
            side == ISwapAdapterTypes.OrderSide.Sell ? output : input
        );
    }

    function testExactInputBothDirections() public {
        _swap(WETH, USDC, ISwapAdapterTypes.OrderSide.Sell, 0.01 ether);
        _swap(USDC, WETH, ISwapAdapterTypes.OrderSide.Sell, 100e6);
    }

    function testExactOutputBothDirections() public {
        _swap(WETH, USDC, ISwapAdapterTypes.OrderSide.Buy, 100e6);
        _swap(USDC, WETH, ISwapAdapterTypes.OrderSide.Buy, 0.01 ether);
    }

    function testTwoFillsReuseMutatedPairState() public {
        uint256 beforeAccumulator = uint256(vm.load(PAIR, bytes32(uint256(3))));
        _swap(USDC, WETH, ISwapAdapterTypes.OrderSide.Sell, 100e6);
        assertGt(uint256(vm.load(PAIR, bytes32(uint256(3)))), beforeAccumulator);
        _swap(USDC, WETH, ISwapAdapterTypes.OrderSide.Sell, 100e6);
    }

    function testPriceAndStalenessUseBlockNumber() public {
        uint256[] memory amounts = new uint256[](1);
        assertGt(adapter.price(poolId, WETH, USDC, amounts)[0].numerator, 0);
        vm.roll(block.number + 100);
        // A stale Pair quotes zero; a zero price would read as a free token.
        vm.expectRevert(
            abi.encodeWithSignature("Unavailable(string)", "No price")
        );
        adapter.price(poolId, WETH, USDC, amounts);
    }

    function testLimitsFallBackToSearchWhenLadderBoundDoesNotQuote() public {
        // The real Pair's ladder bound is far above what this Engine quotes,
        // so the computed bound fails and a conservative search runs below it.
        TesseraSwapAdapter limited = new TesseraSwapAdapter(
            VENUE, address(new LimitedTesseraEngine(PAIR)), TREASURY
        );
        uint256[] memory limits = limited.getLimits(poolId, WETH, USDC);
        assertGt(limits[0], 0);
        assertLe(limits[0], 5e15);
        assertEq(limits[1], limits[0] / 1e9);
    }

    /// The limit settles in full and one more raw unit does not: either the
    /// ladder is exhausted or the treasury cannot pay the output.
    function _assertTightLimits(address pair, address sell, address buy)
        internal
        view
        returns (uint256[] memory limits)
    {
        limits = adapter.getLimits(bytes32(bytes20(pair)), sell, buy);
        assertGt(limits[0], 0);
        (, uint256 output) = ITesseraSwap(VENUE)
            .tesseraSwapViewAmounts(sell, buy, int256(limits[0]));
        assertEq(limits[1], output, "limits[1] is the quote at limits[0]");
        uint256 inventory = IERC20(buy).balanceOf(TREASURY);
        assertLe(output, inventory, "the treasury pays the limit");
        (, uint256 beyond) = ITesseraSwap(VENUE)
            .tesseraSwapViewAmounts(sell, buy, int256(limits[0] + 1));
        assertTrue(beyond == 0 || beyond >= inventory, "limit is tight");
    }

    function testLimitsMatchReviewWorkedExample() public {
        // True limits from the PR 1364 review at block 52,515,061, each checked
        // there by quoting one raw unit more.
        vm.createSelectFork(vm.envString("BASE_RPC_URL"), 52_515_061);
        adapter = new TesseraSwapAdapter(VENUE, ENGINE, TREASURY);
        // Treasury USDC binds below the ladder net of fills.
        uint256[] memory sellBtc = _assertTightLimits(BTC_PAIR, CBBTC, USDC);
        assertEq(sellBtc[0], 507_614_366);
        assertEq(sellBtc[1], 419_634_350_069);
        // More flowed back than out since the last post: the whole live ladder.
        uint256[] memory buyBtc = _assertTightLimits(BTC_PAIR, USDC, CBBTC);
        assertEq(buyBtc[0], 162_243_010_211);
        assertEq(buyBtc[1], 196_210_709);
    }

    function testLimitsSkipFastPulledLevels() public {
        vm.createSelectFork(vm.envString("BASE_RPC_URL"), 52_890_608);
        adapter = new TesseraSwapAdapter(VENUE, ENGINE, TREASURY);
        // While block.number <= fastPullBlock the venue skips each ladder's
        // first fastPullLevel orders: two orders of 12,038,253 sats here.
        vm.mockCall(
            ENGINE,
            abi.encodeWithSignature("fastPullBlock()"),
            abi.encode(block.number)
        );
        uint256[] memory limits =
            adapter.getLimits(bytes32(bytes20(BTC_PAIR)), CBBTC, USDC);
        assertEq(limits[0], 501_275_927 - 2 * 12_038_253);
    }

    function testLimitsNetOppositeFillsOnlyWithReduceAccumulator() public {
        vm.createSelectFork(vm.envString("BASE_RPC_URL"), 52_515_061);
        adapter = new TesseraSwapAdapter(VENUE, ENGINE, TREASURY);
        ITesseraPair.State memory state = ITesseraPair(BTC_PAIR).poolState();
        state.reduceAccumulator = false;
        vm.mockCall(
            BTC_PAIR,
            abi.encodeWithSelector(ITesseraPair.poolState.selector),
            abi.encode(state)
        );
        // The whole own-side accumulator, 11,537.106141 USDC, comes off the
        // live ladder; opposite fills give nothing back.
        uint256[] memory limits =
            adapter.getLimits(bytes32(bytes20(BTC_PAIR)), USDC, CBBTC);
        assertEq(limits[0], 162_243_010_211 - 11_537_106_141);
    }

    function testLimitsNetOwnFillsAndCapByTreasury() public {
        vm.createSelectFork(vm.envString("BASE_RPC_URL"), 52_890_608);
        adapter = new TesseraSwapAdapter(VENUE, ENGINE, TREASURY);
        // Ladder up to its first dead order; no fills on this side.
        assertEq(_assertTightLimits(BTC_PAIR, CBBTC, USDC)[0], 501_275_927);
        // Ladder net of 14,040.302492 USDC filled, then capped by the
        // treasury's 1.71543772 cbBTC.
        assertEq(_assertTightLimits(BTC_PAIR, USDC, CBBTC)[0], 142_476_461_306);
        // The WETH Pair's ladders start with a dead order.
        for (uint256 i; i < 2; ++i) {
            (address sell, address buy) = i == 0 ? (WETH, USDC) : (USDC, WETH);
            uint256[] memory limits = adapter.getLimits(poolId, sell, buy);
            assertEq(limits[0], 0);
            assertEq(limits[1], 0);
        }
    }

    function testCbbtcLimitsAndPriceBothDirections() public {
        address btc = 0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf;
        bytes32 btcPool = bytes32(
            bytes20(address(0xED57BacDc2a990B631F8817853935791C122c356))
        );
        for (uint256 i; i < 2; ++i) {
            (address sell, address buy) = i == 0 ? (USDC, btc) : (btc, USDC);
            uint256 gasBefore = gasleft();
            uint256[] memory limits = adapter.getLimits(btcPool, sell, buy);
            emit log_named_uint("cbBTC getLimits gas", gasBefore - gasleft());
            assertLe(gasBefore - gasleft(), 3_000_000);
            assertGt(limits[0], 0);
            assertGt(limits[1], 0);
            (, uint256 output) = ITesseraSwap(VENUE)
                .tesseraSwapViewAmounts(sell, buy, int256(limits[0]));
            assertGe(output, limits[1]);
            uint256[] memory amounts = new uint256[](1);
            assertGt(adapter.price(btcPool, sell, buy, amounts)[0].numerator, 0);
        }
    }

    function _assertPairUnavailable() internal {
        bytes memory notRegistered = abi.encodeWithSignature(
            "Unavailable(string)", "Pair not registered"
        );
        uint256[] memory amounts = new uint256[](1);
        vm.expectRevert(notRegistered);
        adapter.price(poolId, WETH, USDC, amounts);
        vm.expectRevert(notRegistered);
        adapter.getLimits(poolId, WETH, USDC);
        deal(WETH, address(this), 0.01 ether);
        IERC20(WETH).approve(address(adapter), 0.01 ether);
        vm.expectRevert(notRegistered);
        adapter.swap(
            poolId, WETH, USDC, ISwapAdapterTypes.OrderSide.Sell, 0.01 ether
        );
    }

    function testReplacedPairStopsQuoting() public {
        // TesseraSwap routes by token pair, so the old Pair would otherwise
        // quote whatever Pair the Engine registry now points at.
        vm.store(
            ENGINE,
            WETH_USDC_REGISTRY_SLOT,
            bytes32(uint256(uint160(address(0xBEEF))))
        );
        _assertPairUnavailable();
    }

    function testDeregisteredPairStopsQuoting() public {
        vm.store(ENGINE, WETH_USDC_REGISTRY_SLOT, bytes32(0));
        _assertPairUnavailable();
    }

    /// Spot price against a quote of a tenth of the top ladder level: large
    /// enough that its own rounding is negligible, small enough to stay at the
    /// top level's price.
    function _assertSpotPriceMatchesTopLevel(
        address pair,
        address sell,
        address buy
    ) internal view {
        ITesseraPair.State memory state = ITesseraPair(pair).poolState();
        ITesseraPair.Order[20] memory orders = sell
            == ITesseraPair(pair).baseToken()
            ? state.curveState.baseToQuoteOrders
            : state.curveState.quoteToBaseOrders;
        uint256 referenceIn = orders[0].amount / 10;
        (, uint256 referenceOut) = ITesseraSwap(VENUE)
            .tesseraSwapViewAmounts(sell, buy, int256(referenceIn));
        uint256[] memory amounts = new uint256[](1);
        ISwapAdapterTypes.Fraction memory spot =
            adapter.price(bytes32(bytes20(pair)), sell, buy, amounts)[0];
        uint256 measured = spot.numerator * referenceIn;
        uint256 expected = referenceOut * spot.denominator;
        uint256 diff =
            measured > expected ? measured - expected : expected - measured;
        assertLe(diff * 1e5, expected, "spot price within 0.001% of top level");
    }

    function testSpotPriceStepDoesNotRoundLow() public {
        // At this block a 1,000-unit step priced EURC up to 0.08% low and
        // cbBTC 0.02% low: the view rounds a 1,000-unit output down.
        vm.createSelectFork(vm.envString("BASE_RPC_URL"), 52_590_300);
        adapter = new TesseraSwapAdapter(VENUE, ENGINE, TREASURY);
        address eurcPair = 0x4B963fB4A26f082D94f964FA3c2764821Cc06Bd4;
        address btcPair = 0xED57BacDc2a990B631F8817853935791C122c356;
        _assertSpotPriceMatchesTopLevel(eurcPair, EURC, USDC);
        _assertSpotPriceMatchesTopLevel(eurcPair, USDC, EURC);
        _assertSpotPriceMatchesTopLevel(btcPair, CBBTC, USDC);
        _assertSpotPriceMatchesTopLevel(btcPair, USDC, CBBTC);
    }

    function testRejectsWrongTokens() public {
        vm.expectRevert();
        adapter.getLimits(poolId, WETH, address(1));
    }

    function testSellReportsSettledOutputInsteadOfViewQuote() public {
        DiscountedTesseraSettlement venue =
            new DiscountedTesseraSettlement(PAIR);
        TesseraSwapAdapter localAdapter =
            new TesseraSwapAdapter(address(venue), address(venue), TREASURY);
        deal(WETH, address(this), 100);
        deal(USDC, address(venue), 200);
        IERC20(WETH).approve(address(localAdapter), 100);
        uint256 beforeOut = IERC20(USDC).balanceOf(address(this));
        ISwapAdapterTypes.Trade memory trade = localAdapter.swap(
            poolId, WETH, USDC, ISwapAdapterTypes.OrderSide.Sell, 100
        );
        assertEq(IERC20(USDC).balanceOf(address(this)) - beforeOut, 180);
        assertEq(trade.calculatedAmount, 180);
    }

    function testBuyRejectsUnderfilledExactOutput() public {
        DiscountedTesseraSettlement venue =
            new DiscountedTesseraSettlement(PAIR);
        TesseraSwapAdapter localAdapter =
            new TesseraSwapAdapter(address(venue), address(venue), TREASURY);
        deal(WETH, address(this), 100);
        deal(USDC, address(venue), 200);
        IERC20(WETH).approve(address(localAdapter), 100);
        vm.expectRevert(
            abi.encodeWithSignature(
                "Unavailable(string)", "Exact output not settled"
            )
        );
        localAdapter.swap(
            poolId, WETH, USDC, ISwapAdapterTypes.OrderSide.Buy, 200
        );
    }
}
