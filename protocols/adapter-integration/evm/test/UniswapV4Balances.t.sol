// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import "forge-std/Test.sol";
import {IBalanceLens} from "src/balance-lenses/IBalanceLens.sol";
import {UniswapV4Balances} from "src/balance-lenses/UniswapV4Balances.sol";
import {TickMath} from "v4-core/src/libraries/TickMath.sol";
import {StateLibrary} from "v4-core/src/libraries/StateLibrary.sol";

/// @notice A `PoolManager` that serves `extsload` from slots a test sets.
contract FakePoolManager {
    mapping(bytes32 => bytes32) internal slots;

    function set(bytes32 slot, bytes32 value) external {
        slots[slot] = value;
    }

    function extsload(bytes32 slot) external view returns (bytes32) {
        return slots[slot];
    }
}

/// @notice The pool's storage is laid out as v4-core's `StateLibrary`
/// reads it, at the slot and offsets that library exports. Expected amounts
/// are worked by hand from v4-core's documented sqrt prices at the tick
/// extremes (MIN_SQRT_PRICE 4295128739 at tick -887272, MAX_SQRT_PRICE
/// 1461446703485210103287273052203988822378723970342 at 887272) and the
/// exact 2^96 at tick 0.
contract UniswapV4BalancesTest is Test {
    bytes32 constant POOL_ID =
        0x35bd0e6a8b82e0e18b0381757d97ba01cde11a02740f006132dd40395ffc8ce2;
    int24 constant MIN_TICK = TickMath.MIN_TICK;
    int24 constant MAX_TICK = TickMath.MAX_TICK;
    uint160 constant PRICE_AT_TICK_0 = 2 ** 96;
    address constant TOKEN_A = address(0x1111);
    address constant TOKEN_B = address(0x2222);

    FakePoolManager manager;
    UniswapV4Balances lens;

    function setUp() public {
        manager = new FakePoolManager();
        lens = new UniswapV4Balances();
    }

    function stateSlot() internal pure returns (bytes32) {
        return keccak256(abi.encodePacked(POOL_ID, StateLibrary.POOLS_SLOT));
    }

    function setSlot0(uint160 sqrtPriceX96, int24 tick) internal {
        manager.set(
            stateSlot(),
            bytes32(uint256(sqrtPriceX96) | (uint256(uint24(tick)) << 160))
        );
    }

    function setLiquidity(uint128 liquidity) internal {
        manager.set(
            bytes32(uint256(stateSlot()) + StateLibrary.LIQUIDITY_OFFSET),
            bytes32(uint256(liquidity))
        );
    }

    /// @notice Marks `tick` initialized (tick spacing 1) with `net`.
    function setTick(int24 tick, int128 net) internal {
        setTick(tick, int256(tick) >> 8, uint256(uint24(tick)) & 0xff, net);
    }

    /// @notice Marks `tick` initialized with `net`, at bit `bitPos` of
    /// bitmap word `wordPos`.
    function setTick(int24 tick, int256 wordPos, uint256 bitPos, int128 net)
        internal
    {
        bytes32 ticks =
            bytes32(uint256(stateSlot()) + StateLibrary.TICKS_OFFSET);
        bytes32 bitmap =
            bytes32(uint256(stateSlot()) + StateLibrary.TICK_BITMAP_OFFSET);
        bytes32 info = bytes32(
            (uint256(uint128(net)) << 128)
                | uint256(uint128(net < 0 ? -net : net))
        );
        manager.set(keccak256(abi.encodePacked(int256(tick), ticks)), info);
        bytes32 wordSlot = keccak256(abi.encodePacked(wordPos, bitmap));
        manager.set(
            wordSlot,
            bytes32(uint256(manager.extsload(wordSlot)) | (1 << bitPos))
        );
    }

    function attributes(uint256 tickSpacing)
        internal
        view
        returns (IBalanceLens.Attribute[] memory list)
    {
        list = new IBalanceLens.Attribute[](2);
        list[0] = IBalanceLens.Attribute(
            "tick_spacing", abi.encodePacked(uint16(tickSpacing))
        );
        list[1] = IBalanceLens.Attribute(
            "balance_owner", abi.encodePacked(address(manager))
        );
    }

    function tokens(address first, address second)
        internal
        pure
        returns (address[] memory list)
    {
        list = new address[](2);
        list[0] = first;
        list[1] = second;
    }

    /// @notice The lens's amounts for the pool in `TOKEN_A`, `TOKEN_B` order
    /// at `tickSpacing`.
    function balancesAt(uint256 tickSpacing)
        internal
        view
        returns (uint256[] memory)
    {
        return lens.balances(
            abi.encodePacked(POOL_ID),
            tokens(TOKEN_A, TOKEN_B),
            attributes(tickSpacing)
        );
    }

    /// @notice At tick 0, [MIN, 0) holds 3e18 below the price (currency1
    /// only) and [0, MAX) holds 5e17 from the price up (currency0 only).
    function setRangesBelowAndAboveThePrice() internal {
        setSlot0(PRICE_AT_TICK_0, 0);
        setLiquidity(5e17);
        setTick(MIN_TICK, 3e18);
        setTick(0, -25e17);
        setTick(MAX_TICK, -5e17);
    }

    function testFullRangeLiquidityHoldsBothCurrenciesAtItsPrice() public {
        setSlot0(PRICE_AT_TICK_0, 0);
        setLiquidity(1e18);
        setTick(MIN_TICK, 1e18);
        setTick(MAX_TICK, -1e18);

        uint256[] memory amounts = balancesAt(1);

        // L * (2^96 - MIN_SQRT_PRICE) / 2^96 and
        // L * 2^96 * (MAX_SQRT_PRICE - 2^96) / MAX_SQRT_PRICE / 2^96, floored.
        assertEq(amounts.length, 2);
        assertEq(amounts[0], 999999999999999999);
        assertEq(amounts[1], 999999999999999999);
    }

    function testRangesBelowAndAboveThePriceHoldOneCurrencyEach() public {
        setRangesBelowAndAboveThePrice();

        uint256[] memory amounts = balancesAt(1);

        assertEq(amounts[0], 499999999999999999);
        assertEq(amounts[1], 2999999999999999999);
    }

    function testAmountsFollowTheOrderOfTheTokens() public {
        setRangesBelowAndAboveThePrice();

        uint256[] memory amounts = lens.balances(
            abi.encodePacked(POOL_ID), tokens(TOKEN_B, TOKEN_A), attributes(1)
        );

        assertEq(amounts[0], 2999999999999999999);
        assertEq(amounts[1], 499999999999999999);
    }

    function testTicksOfASpacing60PoolOnBothSidesOfZero() public {
        // Tick -120 is compressed -2: bitmap word -1, bit 254. Tick 60 is
        // compressed 1: word 0, bit 1. Amounts of 1e18 over [-120, 60) at
        // tick 0, from sqrt(1.0001^t) * 2^96 worked to 80 digits
        // (78754240422856966435523493929.66 at -120,
        // 79466191966197645195421774832.22 at 60; either rounding gives
        // the same amounts):
        // currency0 = floor(floor(L * 2^96 * (sU - 2^96) / sU) / 2^96),
        // currency1 = floor(L * (2^96 - sL) / 2^96).
        setSlot0(PRICE_AT_TICK_0, 0);
        setLiquidity(1e18);
        setTick(-120, -1, 254, 1e18);
        setTick(60, 0, 1, -1e18);

        uint256[] memory amounts = balancesAt(60);

        assertEq(amounts[0], 2995354955910780);
        assertEq(amounts[1], 5981737760509662);
    }

    /// @notice Puts the pool at tick 0 with two ranges and an empty gap
    /// [-120, 60) around the price: [MIN_TICK, -120) holds 1e18 below it and
    /// [60, MAX_TICK) holds 2e18 above it. Writes `poolLiquidity` into the
    /// pool's liquidity slot; 0 is the liquidity in force inside the gap.
    function setRangesAroundAnEmptyGap(uint128 poolLiquidity) internal {
        setSlot0(PRICE_AT_TICK_0, 0);
        setLiquidity(poolLiquidity);
        setTick(MIN_TICK, 1e18);
        setTick(-120, -1e18);
        setTick(60, 2e18);
        setTick(MAX_TICK, -2e18);
    }

    function testRangesAroundAnEmptyGapHoldOneCurrencyEach() public {
        setRangesAroundAnEmptyGap(0);

        uint256[] memory amounts = balancesAt(1);

        // The sqrt prices at -120 and 60 of
        // testTicksOfASpacing60PoolOnBothSidesOfZero (either rounding gives
        // the same amounts):
        // currency0 = floor(floor(2e18 * 2^96 * (MAX_SQRT_PRICE - s(60))
        //     / MAX_SQRT_PRICE) / s(60)),
        // currency1 = floor(1e18 * (s(-120) - MIN_SQRT_PRICE) / 2^96),
        // where s(t) is the sqrt price at tick t.
        assertEq(amounts[0], 1994009290088178438);
        assertEq(amounts[1], 994018262239490337);
    }

    function testLiquidityInForceInsideAnEmptyGapReverts() public {
        setRangesAroundAnEmptyGap(5);

        vm.expectRevert(
            abi.encodeWithSelector(
                UniswapV4Balances.UniswapV4Balances__LiquidityMismatch.selector,
                0,
                5
            )
        );
        balancesAt(1);
    }

    function testAnUninitializedPoolReverts() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                UniswapV4Balances.UniswapV4Balances__PoolNotInitialized
                .selector,
                POOL_ID
            )
        );
        balancesAt(1);
    }

    function testGasOfAnEmptyPoolAtTickSpacing1() public {
        // 6,932 bitmap words, each one extsload call reading a cold slot:
        // measured 28,232,670 gas, about 4,070 per word.
        setSlot0(PRICE_AT_TICK_0, 0);

        uint256 before = gasleft();
        balancesAt(1);
        uint256 used = before - gasleft();

        assertGt(used, 26_000_000);
        assertLt(used, 30_000_000);
    }

    function testAPoolWithoutLiquidityHoldsNothing() public {
        setSlot0(PRICE_AT_TICK_0, 0);

        uint256[] memory amounts = balancesAt(200);

        assertEq(amounts[0], 0);
        assertEq(amounts[1], 0);
    }

    function testAWalkThatMissesThePoolLiquidityReverts() public {
        setSlot0(PRICE_AT_TICK_0, 0);
        setLiquidity(7);
        setTick(MIN_TICK, 1e18);
        setTick(MAX_TICK, -1e18);

        vm.expectRevert(
            abi.encodeWithSelector(
                UniswapV4Balances.UniswapV4Balances__LiquidityMismatch.selector,
                1e18,
                7
            )
        );
        balancesAt(1);
    }

    function testANetThatTakesLiquidityBelowZeroReverts() public {
        setSlot0(PRICE_AT_TICK_0, 0);
        setTick(MIN_TICK, -1);

        vm.expectRevert(
            abi.encodeWithSelector(
                UniswapV4Balances.UniswapV4Balances__NegativeLiquidity.selector,
                MIN_TICK
            )
        );
        balancesAt(1);
    }

    function testAnIdThatIsNotAPoolIdReverts() public {
        bytes memory id = abi.encodePacked(address(0xBEEF));

        vm.expectRevert(
            abi.encodeWithSelector(
                UniswapV4Balances.UniswapV4Balances__NotAPoolId.selector, id
            )
        );
        lens.balances(id, tokens(TOKEN_A, TOKEN_B), attributes(1));
    }

    function testATickSpacingOfZeroReverts() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                UniswapV4Balances.UniswapV4Balances__InvalidTickSpacing
                .selector,
                0
            )
        );
        balancesAt(0);
    }

    function testATickSpacingAboveV4BoundsReverts() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                UniswapV4Balances.UniswapV4Balances__InvalidTickSpacing
                .selector,
                32768
            )
        );
        balancesAt(32768);
    }

    function testAComponentWithoutTwoTokensReverts() public {
        address[] memory one = new address[](1);
        one[0] = TOKEN_A;

        vm.expectRevert(
            abi.encodeWithSelector(
                UniswapV4Balances.UniswapV4Balances__NotTwoTokens.selector, 1
            )
        );
        lens.balances(abi.encodePacked(POOL_ID), one, attributes(1));
    }
}
