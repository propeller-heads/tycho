// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import "./AdapterTest.sol";
import "forge-std/Test.sol";
import "src/interfaces/ISwapAdapterTypes.sol";
import "src/libraries/FractionMath.sol";
import "src/camelot-v3/CamelotV3SwapAdapter.sol";

/// @dev The factory functions the tests use beyond what the adapter needs.
interface IAlgebraFactoryTest {
    function vaultAddress() external view returns (address);
    function createPool(address tokenA, address tokenB)
        external
        returns (address pool);
}

/// @dev Fork tests against Camelot V3 on Arbitrum One. `ARBITRUM_RPC_URL`
/// must point at a node that serves state at the fork block.
contract CamelotV3SwapAdapterTest is AdapterTest {
    using FractionMath for Fraction;

    struct Direction {
        address pool;
        address tokenIn;
        address tokenOut;
    }

    CamelotV3SwapAdapter adapter;

    address constant FACTORY = 0x1a3c9B1d2F0529D97f2afC5136Cc23e58f1FD35B;
    address constant WETH = 0x82aF49447D8a07e3bd95BD0d56f35241523fBab1;
    address constant USDC = 0xaf88d065e77c8cC2239327C5EDb3A432268e5831;
    address constant ARB = 0x912CE59144191C1204E64559FE8253a0e49E6548;
    // Camelot V3 WETH/USDC pool: token0 = WETH, token1 = USDC.
    address constant WETH_USDC_POOL =
        0xB1026b8e7276e7AC75410F1fcbbe21796e8f7526;
    // Camelot V3 WETH/ARB pool: token0 = WETH, token1 = ARB.
    address constant WETH_ARB_POOL = 0xe51635ae8136aBAc44906A8f230C2D235E9c195F;
    address constant SUSHI = 0x1bc6059b12A452DD9Ec0140fDc79140Ebf4DBE12;
    // Camelot V3 SUSHI/WETH pool: token0 = SUSHI, token1 = WETH. At the fork
    // block it holds no liquidity and its price sits at the minimum.
    address constant SUSHI_WETH_POOL =
        0x1Ec0979B8A1F9E09066EdfB065B9A8Bc4d99a155;

    uint256 constant FORK_BLOCK = 504411371;
    uint256 constant FEE_DENOMINATOR = 1e6;
    uint256 constant TEST_ITERATIONS = 10;
    /// @dev Gas the simulation engine gives the transaction that calls the
    /// adapter (`SimulationEngine`'s default in tycho-simulation), and what
    /// that leaves for the call itself once the intrinsic transaction cost is
    /// paid: the 21k base plus the calldata of a `swap` or `getLimits` call.
    uint256 constant ENGINE_GAS_LIMIT = 8_000_000;
    uint256 constant ENGINE_CALL_GAS = ENGINE_GAS_LIMIT - 25_000;
    /// @dev Fuzz lower bounds on the WETH/USDC pool, kept well above dust so
    /// every case moves tokens: at the fork price a WETH sell below roughly
    /// 4e8 wei pays no USDC and is reported as `TooSmall`. Buys start at
    /// 0.01 USDC.
    uint256 constant MIN_WETH_SELL = 1e12;
    uint256 constant MIN_USDC_BUY = 1e4;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("arbitrum"), FORK_BLOCK);
        adapter = new CamelotV3SwapAdapter(FACTORY);

        vm.label(address(adapter), "CamelotV3SwapAdapter");
        vm.label(FACTORY, "AlgebraFactory");
        vm.label(WETH, "WETH");
        vm.label(USDC, "USDC");
        vm.label(ARB, "ARB");
        vm.label(WETH_USDC_POOL, "WETH_USDC_POOL");
        vm.label(WETH_ARB_POOL, "WETH_ARB_POOL");
    }

    function poolId(address pool) internal pure returns (bytes32) {
        return bytes32(bytes20(pool));
    }

    /// @dev Both pools in both directions.
    function directions() internal pure returns (Direction[4] memory list) {
        list[0] = Direction(WETH_USDC_POOL, WETH, USDC);
        list[1] = Direction(WETH_USDC_POOL, USDC, WETH);
        list[2] = Direction(WETH_ARB_POOL, WETH, ARB);
        list[3] = Direction(WETH_ARB_POOL, ARB, WETH);
    }

    /// @dev Receives the community share of every swap's fee.
    function vault() internal view returns (address) {
        return IAlgebraFactoryTest(FACTORY).vaultAddress();
    }

    /// @dev Everything a swap on the WETH/USDC pool would change.
    function poolStateHash() internal view returns (bytes32) {
        (
            uint160 price,
            int24 tick,
            uint16 feeZto,
            uint16 feeOtz,
            uint16 timepointIndex,
            uint8 communityFee0,
            uint8 communityFee1,
            bool unlocked
        ) = IAlgebraPool(WETH_USDC_POOL).globalState();
        return keccak256(
            abi.encode(
                price,
                tick,
                feeZto,
                feeOtz,
                timepointIndex,
                communityFee0,
                communityFee1,
                unlocked,
                IAlgebraPool(WETH_USDC_POOL).liquidity(),
                IERC20(WETH).balanceOf(WETH_USDC_POOL),
                IERC20(USDC).balanceOf(WETH_USDC_POOL)
            )
        );
    }

    function testGetTokens() public view {
        address[] memory tokens = adapter.getTokens(poolId(WETH_USDC_POOL));
        assertEq(tokens.length, 2);
        assertEq(tokens[0], WETH);
        assertEq(tokens[1], USDC);
    }

    /// @dev The repo's shared adapter behaviour test: marginal prices fall as
    /// the sold amount grows, every executed price sits between the marginal
    /// prices before and after the trade, and amounts 5% above the walked
    /// limit still price and swap, since the limit is only a lower bound. That
    /// last check needs pools whose liquidity extends past the walk's step
    /// cap; both pools here do.
    function testPoolBehaviour() public {
        bytes32[] memory pools = new bytes32[](2);
        pools[0] = poolId(WETH_USDC_POOL);
        pools[1] = poolId(WETH_ARB_POOL);
        runPoolBehaviourTest(adapter, pools);
    }

    function testGetCapabilities() public view {
        Capability[] memory capabilities =
            adapter.getCapabilities(poolId(WETH_USDC_POOL), WETH, USDC);
        assertEq(capabilities.length, 4);
        assertEq(uint256(capabilities[0]), uint256(Capability.SellOrder));
        assertEq(uint256(capabilities[1]), uint256(Capability.BuyOrder));
        assertEq(uint256(capabilities[2]), uint256(Capability.PriceFunction));
        assertEq(uint256(capabilities[3]), uint256(Capability.MarginalPrice));
    }

    function testGetPoolIdsNotImplemented() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                ISwapAdapterTypes.NotImplemented.selector,
                "CamelotV3SwapAdapter.getPoolIds"
            )
        );
        adapter.getPoolIds(0, 10);
    }

    /// @dev Every entry point resolves the pair the same way, and the same
    /// token on both sides is not a pair either.
    function testRejectsTokensOutsideThePair() public {
        bytes memory invalidOrder = abi.encodeWithSelector(
            ISwapAdapterTypes.InvalidOrder.selector,
            "tokens are not the pool's pair"
        );
        uint256[] memory amounts = new uint256[](1);

        vm.expectRevert(invalidOrder);
        adapter.getLimits(poolId(WETH_USDC_POOL), WETH, ARB);

        vm.expectRevert(invalidOrder);
        adapter.price(poolId(WETH_USDC_POOL), WETH, ARB, amounts);

        vm.expectRevert(invalidOrder);
        adapter.swap(poolId(WETH_USDC_POOL), WETH, ARB, OrderSide.Sell, 1 ether);

        vm.expectRevert(invalidOrder);
        adapter.getLimits(poolId(WETH_USDC_POOL), WETH, WETH);
    }

    function testGetLimits() public {
        bytes32 pool = poolId(WETH_USDC_POOL);
        uint256[] memory sellWeth = adapter.getLimits(pool, WETH, USDC);
        uint256[] memory sellUsdc = adapter.getLimits(pool, USDC, WETH);

        assertEq(sellWeth.length, 2);
        assertGt(sellWeth[0], 0, "WETH sell limit");
        assertGt(sellWeth[1], 0, "USDC buy limit");
        assertGt(sellUsdc[0], 0, "USDC sell limit");
        assertGt(sellUsdc[1], 0, "WETH buy limit");
        // A pool cannot pay out more than it holds.
        assertLe(sellWeth[1], IERC20(USDC).balanceOf(WETH_USDC_POOL));
        assertLe(sellUsdc[1], IERC20(WETH).balanceOf(WETH_USDC_POOL));
    }

    /// @dev Selling exactly the limit is fully executed and pays out the buy
    /// limit up to the rounding of the last step, since the pool rounds every
    /// step the way the walk does. Both calls get the gas the engine's
    /// transaction leaves them, as the engine funds the payer with exactly
    /// this limit.
    function testSellingTheLimitIsFullyExecuted() public {
        Direction[4] memory list = directions();
        for (uint256 i = 0; i < list.length; i++) {
            uint256 snapshot = vm.snapshot();
            bytes32 pool = poolId(list[i].pool);
            uint256[] memory limits = adapter.getLimits{gas: ENGINE_CALL_GAS}(
                pool, list[i].tokenIn, list[i].tokenOut
            );

            deal(list[i].tokenIn, address(this), limits[0]);
            IERC20(list[i].tokenIn).approve(address(adapter), limits[0]);
            uint256 outBefore =
                IERC20(list[i].tokenOut).balanceOf(address(this));

            Trade memory trade = adapter.swap{gas: ENGINE_CALL_GAS}(
                pool,
                list[i].tokenIn,
                list[i].tokenOut,
                OrderSide.Sell,
                limits[0]
            );

            assertEq(
                IERC20(list[i].tokenIn).balanceOf(address(this)),
                0,
                "input spent"
            );
            assertEq(
                IERC20(list[i].tokenOut).balanceOf(address(this)) - outBefore,
                trade.calculatedAmount,
                "output received"
            );
            assertLe(
                trade.calculatedAmount, limits[1], "output within the buy limit"
            );
            assertApproxEqRel(
                trade.calculatedAmount,
                limits[1],
                1e12, // 1e-6 relative
                "output reaches the buy limit"
            );
            console2.log(
                "sell limit swap gas",
                list[i].tokenIn,
                list[i].tokenOut,
                trade.gasUsed
            );
            vm.revertTo(snapshot);
        }
    }

    /// @dev Buying exactly the buy limit is fully executed and costs the sell
    /// limit, up to the rounding of the last step. Both calls run with the gas
    /// the engine's transaction leaves them.
    function testBuyingTheLimitIsFullyExecuted() public {
        Direction[4] memory list = directions();
        for (uint256 i = 0; i < list.length; i++) {
            uint256 snapshot = vm.snapshot();
            bytes32 pool = poolId(list[i].pool);
            uint256[] memory limits = adapter.getLimits{gas: ENGINE_CALL_GAS}(
                pool, list[i].tokenIn, list[i].tokenOut
            );

            deal(list[i].tokenIn, address(this), type(uint128).max);
            IERC20(list[i].tokenIn).approve(address(adapter), type(uint256).max);
            uint256 inBefore = IERC20(list[i].tokenIn).balanceOf(address(this));
            uint256 outBefore =
                IERC20(list[i].tokenOut).balanceOf(address(this));

            Trade memory trade = adapter.swap{gas: ENGINE_CALL_GAS}(
                pool,
                list[i].tokenIn,
                list[i].tokenOut,
                OrderSide.Buy,
                limits[1]
            );

            assertEq(
                IERC20(list[i].tokenOut).balanceOf(address(this)) - outBefore,
                limits[1],
                "exact output"
            );
            assertEq(
                inBefore - IERC20(list[i].tokenIn).balanceOf(address(this)),
                trade.calculatedAmount,
                "input paid"
            );
            assertApproxEqRel(
                trade.calculatedAmount,
                limits[0],
                1e12, // 1e-6 relative
                "input matches the sell limit"
            );
            vm.revertTo(snapshot);
        }
    }

    /// @dev The price at zero uses the pool's current sqrt price and the fee
    /// the pool charges the next swap, which the pool only computes when a
    /// swap runs. That fee is read here by running a tiny swap in a snapshot.
    function testPriceAtZeroMatchesPoolState() public {
        bytes32 pool = poolId(WETH_USDC_POOL);
        (uint160 sqrtPrice,,,,,,,) = IAlgebraPool(WETH_USDC_POOL).globalState();

        uint256 snapshot = vm.snapshot();
        uint256 tiny = 1e12; // 0.000001 WETH, enough for a non-zero output
        deal(WETH, address(this), tiny);
        IERC20(WETH).approve(address(adapter), tiny);
        adapter.swap(pool, WETH, USDC, OrderSide.Sell, tiny);
        (,, uint16 feeZto, uint16 feeOtz,,,,) =
            IAlgebraPool(WETH_USDC_POOL).globalState();
        vm.revertTo(snapshot);

        uint256[] memory amounts = new uint256[](1);
        Fraction memory wethInUsdc = adapter.price(pool, WETH, USDC, amounts)[0];
        Fraction memory usdcInWeth = adapter.price(pool, USDC, WETH, amounts)[0];

        // price of token0 in token1 = sqrtPrice^2 / 2^192, net of the fee.
        uint256 priceX128 = Math.mulDiv(sqrtPrice, sqrtPrice, 2 ** 64);
        assertApproxEqRel(
            wethInUsdc.toQ128x128(),
            Math.mulDiv(priceX128, FEE_DENOMINATOR - feeZto, FEE_DENOMINATOR),
            1e6, // 1e-12 relative
            "WETH -> USDC price at zero"
        );
        assertApproxEqRel(
            usdcInWeth.toQ128x128(),
            Math.mulDiv(
                (2 ** 128) * (FEE_DENOMINATOR - feeOtz),
                2 ** 128,
                priceX128 * FEE_DENOMINATOR
            ),
            1e6,
            "USDC -> WETH price at zero"
        );
    }

    /// @dev Any two amounts up to the limit price, and selling more never
    /// improves the marginal price.
    function testPriceFuzz(uint256 amount0, uint256 amount1) public {
        bytes32 pool = poolId(WETH_USDC_POOL);
        uint256 limit = adapter.getLimits(pool, WETH, USDC)[0];
        uint256[] memory amounts = new uint256[](2);
        amounts[0] = bound(amount0, 0, limit);
        amounts[1] = bound(amount1, 0, limit);
        if (amounts[0] > amounts[1]) {
            (amounts[0], amounts[1]) = (amounts[1], amounts[0]);
        }

        Fraction[] memory prices = adapter.price(pool, WETH, USDC, amounts);

        for (uint256 i = 0; i < prices.length; i++) {
            assertGt(prices[i].numerator, 0);
            assertGt(prices[i].denominator, 0);
        }
        assertGe(
            prices[0].compareFractions(prices[1]),
            0,
            "price must not rise with the sold amount"
        );
    }

    function testPricesDecreaseWithAmount() public {
        checkPricesDecrease(WETH_USDC_POOL, WETH, USDC);
        checkPricesDecrease(WETH_USDC_POOL, USDC, WETH);
        checkPricesDecrease(WETH_ARB_POOL, WETH, ARB);
        checkPricesDecrease(WETH_ARB_POOL, ARB, WETH);
    }

    function checkPricesDecrease(
        address pool,
        address tokenIn,
        address tokenOut
    ) internal {
        uint256 limit = adapter.getLimits(poolId(pool), tokenIn, tokenOut)[0];
        uint256[] memory amounts = new uint256[](6);
        amounts[0] = 0;
        amounts[1] = limit / 1000;
        amounts[2] = limit / 100;
        amounts[3] = limit / 10;
        amounts[4] = limit / 2;
        amounts[5] = limit;

        Fraction[] memory prices =
            adapter.price(poolId(pool), tokenIn, tokenOut, amounts);
        for (uint256 i = 0; i < prices.length; i++) {
            assertGt(prices[i].numerator, 0);
            assertGt(prices[i].denominator, 0);
            if (i > 0) {
                assertEq(
                    prices[i - 1].compareFractions(prices[i]),
                    1,
                    "price must decrease as the sold amount grows"
                );
            }
        }
    }

    function testSwapSell() public {
        checkSell(WETH_USDC_POOL, WETH, USDC, 1 ether);
        checkSell(WETH_USDC_POOL, USDC, WETH, 3_000e6);
        checkSell(WETH_ARB_POOL, WETH, ARB, 1 ether);
        checkSell(WETH_ARB_POOL, ARB, WETH, 5_000 ether);
    }

    /// @dev A sell moves exactly the specified input, pays out the calculated
    /// amount, lands the input in the pool and the community vault, and prices
    /// behave like a pool with price impact: executed price <= price before,
    /// and > marginal price after.
    function checkSell(
        address pool,
        address tokenIn,
        address tokenOut,
        uint256 amount
    ) internal {
        uint256 snapshot = vm.snapshot();
        uint256[] memory amounts = new uint256[](2);
        amounts[1] = amount;
        Fraction[] memory prices =
            adapter.price(poolId(pool), tokenIn, tokenOut, amounts);

        deal(tokenIn, address(this), amount);
        IERC20(tokenIn).approve(address(adapter), amount);
        uint256 outBefore = IERC20(tokenOut).balanceOf(address(this));
        uint256 poolBefore = IERC20(tokenIn).balanceOf(pool);
        uint256 vaultBefore = IERC20(tokenIn).balanceOf(vault());

        Trade memory trade = adapter.swap(
            poolId(pool), tokenIn, tokenOut, OrderSide.Sell, amount
        );

        assertEq(IERC20(tokenIn).balanceOf(address(this)), 0, "input spent");
        assertEq(
            IERC20(tokenOut).balanceOf(address(this)) - outBefore,
            trade.calculatedAmount,
            "output received"
        );
        // The pool keeps the input minus the community fee it forwards.
        assertEq(
            IERC20(tokenIn).balanceOf(pool) - poolBefore
                + IERC20(tokenIn).balanceOf(vault()) - vaultBefore,
            amount,
            "input lands in the pool and the vault"
        );
        assertGt(trade.calculatedAmount, 0);
        assertGt(trade.gasUsed, 0);
        console2.log("pool swap gas", tokenIn, tokenOut, trade.gasUsed);
        assertEq(
            trade.price.compareFractions(prices[1]),
            0,
            "swap reports the price quoted for the same amount"
        );

        Fraction memory executed = Fraction(trade.calculatedAmount, amount);
        assertEq(
            prices[0].compareFractions(executed), 1, "price at zero > executed"
        );
        assertEq(
            executed.compareFractions(trade.price), 1, "executed > price after"
        );
        vm.revertTo(snapshot);
    }

    function testSwapBuy() public {
        checkBuy(WETH_USDC_POOL, WETH, USDC, 1_000e6);
        checkBuy(WETH_USDC_POOL, USDC, WETH, 1 ether);
        checkBuy(WETH_ARB_POOL, WETH, ARB, 5_000 ether);
        checkBuy(WETH_ARB_POOL, ARB, WETH, 1 ether);
    }

    /// @dev A buy pays out exactly the wanted amount, pulls the calculated
    /// input into the pool and the vault, and reports the marginal price after
    /// the trade: below the executed price, and the price quoted for selling
    /// the same input up to the rounding of the last step.
    function checkBuy(
        address pool,
        address tokenIn,
        address tokenOut,
        uint256 wanted
    ) internal {
        uint256 snapshot = vm.snapshot();
        deal(tokenIn, address(this), type(uint128).max);
        IERC20(tokenIn).approve(address(adapter), type(uint256).max);
        uint256 inBefore = IERC20(tokenIn).balanceOf(address(this));
        uint256 outBefore = IERC20(tokenOut).balanceOf(address(this));
        uint256 poolBefore = IERC20(tokenIn).balanceOf(pool);
        uint256 vaultBefore = IERC20(tokenIn).balanceOf(vault());
        uint256[] memory amounts = new uint256[](1);
        Fraction memory priceAtZero =
            adapter.price(poolId(pool), tokenIn, tokenOut, amounts)[0];

        Trade memory trade = adapter.swap(
            poolId(pool), tokenIn, tokenOut, OrderSide.Buy, wanted
        );

        assertEq(
            IERC20(tokenOut).balanceOf(address(this)) - outBefore,
            wanted,
            "exact output"
        );
        assertEq(
            inBefore - IERC20(tokenIn).balanceOf(address(this)),
            trade.calculatedAmount,
            "input paid"
        );
        assertEq(
            IERC20(tokenIn).balanceOf(pool) - poolBefore
                + IERC20(tokenIn).balanceOf(vault()) - vaultBefore,
            trade.calculatedAmount,
            "input lands in the pool and the vault"
        );
        assertGt(trade.calculatedAmount, 0);
        assertGt(trade.gasUsed, 0);

        Fraction memory executed = Fraction(wanted, trade.calculatedAmount);
        assertEq(
            priceAtZero.compareFractions(executed),
            1,
            "price at zero > executed"
        );
        assertEq(
            executed.compareFractions(trade.price), 1, "executed > price after"
        );

        vm.revertTo(snapshot);
        amounts[0] = trade.calculatedAmount;
        Fraction memory quoted =
            adapter.price(poolId(pool), tokenIn, tokenOut, amounts)[0];
        assertApproxEqRel(
            trade.price.toQ128x128(),
            quoted.toQ128x128(),
            1e9, // 1e-9 relative
            "buy reports the price quoted for selling its input"
        );
    }

    /// @dev Like the other adapter suites: any amount up to the limit, on
    /// either side, moves exactly the specified amount and reports the rest.
    function testSwapFuzz(uint256 specifiedAmount, bool isBuy) public {
        bytes32 pool = poolId(WETH_USDC_POOL);
        uint256[] memory limits = adapter.getLimits(pool, WETH, USDC);
        uint256 usdcBefore = IERC20(USDC).balanceOf(address(this));
        Trade memory trade;

        if (isBuy) {
            specifiedAmount = bound(specifiedAmount, MIN_USDC_BUY, limits[1]);
            deal(WETH, address(this), type(uint128).max);
            IERC20(WETH).approve(address(adapter), type(uint256).max);
            uint256 wethBefore = IERC20(WETH).balanceOf(address(this));

            trade =
                adapter.swap(pool, WETH, USDC, OrderSide.Buy, specifiedAmount);

            assertEq(
                IERC20(USDC).balanceOf(address(this)) - usdcBefore,
                specifiedAmount,
                "exact output"
            );
            assertEq(
                wethBefore - IERC20(WETH).balanceOf(address(this)),
                trade.calculatedAmount,
                "input paid"
            );
        } else {
            specifiedAmount = bound(specifiedAmount, MIN_WETH_SELL, limits[0]);
            uint256[] memory amounts = new uint256[](1);
            amounts[0] = specifiedAmount;
            Fraction memory quoted = adapter.price(pool, WETH, USDC, amounts)[0];
            deal(WETH, address(this), specifiedAmount);
            IERC20(WETH).approve(address(adapter), specifiedAmount);

            trade =
                adapter.swap(pool, WETH, USDC, OrderSide.Sell, specifiedAmount);

            assertEq(IERC20(WETH).balanceOf(address(this)), 0, "input spent");
            assertEq(
                IERC20(USDC).balanceOf(address(this)) - usdcBefore,
                trade.calculatedAmount,
                "output received"
            );
            assertEq(
                trade.price.compareFractions(quoted),
                0,
                "swap reports the price quoted for the same amount"
            );
        }

        assertGt(trade.calculatedAmount, 0);
        assertGt(trade.price.numerator, 0);
        assertGt(trade.price.denominator, 0);
    }

    function testSwapSellIncreasing() public {
        executeIncreasingSwaps(OrderSide.Sell);
    }

    function testSwapBuyIncreasing() public {
        executeIncreasingSwaps(OrderSide.Buy);
    }

    /// @dev Larger trades move at least as many tokens, cost at least as much
    /// gas, and end at a strictly worse marginal price.
    function executeIncreasingSwaps(OrderSide side) internal {
        bytes32 pool = poolId(WETH_USDC_POOL);
        uint256[] memory limits = adapter.getLimits(pool, WETH, USDC);
        uint256 limit = side == OrderSide.Sell ? limits[0] : limits[1];
        if (side == OrderSide.Buy) {
            deal(WETH, address(this), type(uint128).max);
            IERC20(WETH).approve(address(adapter), type(uint256).max);
        }

        Trade[] memory trades = new Trade[](TEST_ITERATIONS);
        for (uint256 i = 0; i < TEST_ITERATIONS; i++) {
            uint256 amount = limit * (i + 1) / TEST_ITERATIONS;
            uint256 snapshot = vm.snapshot();
            if (side == OrderSide.Sell) {
                deal(WETH, address(this), amount);
                IERC20(WETH).approve(address(adapter), amount);
            }
            trades[i] = adapter.swap(pool, WETH, USDC, side, amount);
            vm.revertTo(snapshot);
        }

        for (uint256 i = 0; i < TEST_ITERATIONS - 1; i++) {
            assertLe(
                trades[i].calculatedAmount,
                trades[i + 1].calculatedAmount,
                "amount grows with the trade"
            );
            assertLe(
                trades[i].gasUsed,
                trades[i + 1].gasUsed,
                "gas grows with the trade"
            );
            assertEq(
                trades[i].price.compareFractions(trades[i + 1].price),
                1,
                "price worsens with the trade"
            );
        }
    }

    /// @dev A swap changes the state every later call sees: the price at zero
    /// afterwards is the price the swap reported, and repeating the same sell
    /// pays out less at a worse price.
    function testSwapUpdatesQuotesForTheNextTrade() public {
        bytes32 pool = poolId(WETH_USDC_POOL);
        deal(WETH, address(this), 2 ether);
        IERC20(WETH).approve(address(adapter), 2 ether);

        Trade memory first =
            adapter.swap(pool, WETH, USDC, OrderSide.Sell, 1 ether);
        uint256[] memory amounts = new uint256[](1);
        Fraction memory priceAfter = adapter.price(pool, WETH, USDC, amounts)[0];
        assertEq(
            priceAfter.compareFractions(first.price),
            0,
            "price at zero after the swap is the price the swap reported"
        );

        Trade memory second =
            adapter.swap(pool, WETH, USDC, OrderSide.Sell, 1 ether);
        assertLt(
            second.calculatedAmount,
            first.calculatedAmount,
            "the second sell pays out less"
        );
        assertEq(
            second.price.compareFractions(first.price),
            -1,
            "the second sell ends at a worse price"
        );
    }

    /// @dev Selling more than the pool can absorb drains it to its minimum
    /// price; the adapter must report how much was absorbed instead of
    /// pretending the whole amount was traded.
    function testSwapSellAboveLiquidityReverts() public {
        bytes32 pool = poolId(WETH_USDC_POOL);
        uint256 amount = 2 ** 120;
        deal(WETH, address(this), amount);
        IERC20(WETH).approve(address(adapter), amount);

        try adapter.swap(pool, WETH, USDC, OrderSide.Sell, amount) {
            revert("swap above liquidity must revert");
        } catch (bytes memory reason) {
            uint256 absorbed = decodeLimitExceeded(reason);
            assertGt(absorbed, 0);
            assertLt(absorbed, amount);
        }
    }

    function testPriceAboveLiquidityReverts() public {
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 2 ** 120;
        try adapter.price(poolId(WETH_USDC_POOL), WETH, USDC, amounts) {
            revert("price above liquidity must revert");
        } catch (bytes memory reason) {
            uint256 absorbed = decodeLimitExceeded(reason);
            assertGt(absorbed, 0);
            assertLt(absorbed, amounts[0]);
        }
    }

    function testSwapBuyAboveLiquidityReverts() public {
        bytes32 pool = poolId(WETH_USDC_POOL);
        uint256 wanted = IERC20(USDC).balanceOf(WETH_USDC_POOL) * 2;
        deal(WETH, address(this), 2 ** 120);
        IERC20(WETH).approve(address(adapter), 2 ** 120);

        try adapter.swap(pool, WETH, USDC, OrderSide.Buy, wanted) {
            revert("buy above liquidity must revert");
        } catch (bytes memory reason) {
            uint256 available = decodeLimitExceeded(reason);
            assertGt(available, 0);
            assertLt(available, wanted);
        }
    }

    /// @dev Asserts `reason` is `LimitExceeded(limit)` and returns `limit`.
    function decodeLimitExceeded(bytes memory reason)
        internal
        pure
        returns (uint256 limit)
    {
        assertEq(reason.length, 36, "LimitExceeded(uint256) revert data");
        assertEq(
            bytes4(reason),
            ISwapAdapterTypes.LimitExceeded.selector,
            "LimitExceeded selector"
        );
        assembly {
            limit := mload(add(reason, 36))
        }
    }

    /// @dev A sell too small to produce any output is reported as `TooSmall`
    /// instead of as a trade that returns nothing.
    function testDustSellReverts() public {
        deal(WETH, address(this), 1);
        IERC20(WETH).approve(address(adapter), 1);
        vm.expectRevert(
            abi.encodeWithSelector(ISwapAdapterTypes.TooSmall.selector, 0)
        );
        adapter.swap(poolId(WETH_USDC_POOL), WETH, USDC, OrderSide.Sell, 1);
    }

    function testSwapZeroAmountReverts() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                ISwapAdapterTypes.InvalidOrder.selector, "amount is zero"
            )
        );
        adapter.swap(poolId(WETH_USDC_POOL), WETH, USDC, OrderSide.Sell, 0);
    }

    /// @dev A pool without liquidity ahead of its price has nothing to sell or
    /// buy in either direction, and reports that without quoting a fee.
    function testEmptyPoolHasZeroLimits() public {
        bytes32 pool = poolId(SUSHI_WETH_POOL);
        uint256[] memory sellSushi = adapter.getLimits(pool, SUSHI, WETH);
        uint256[] memory sellWeth = adapter.getLimits(pool, WETH, SUSHI);
        assertEq(sellSushi[0], 0);
        assertEq(sellSushi[1], 0);
        assertEq(sellWeth[0], 0);
        assertEq(sellWeth[1], 0);
    }

    /// @dev A pool the factory created but nobody initialized has no price and
    /// every entry point reports it as unavailable, never as the pool's own
    /// `LOK`. Anyone may create one, and the factory never looks at the token
    /// contracts, so two fresh addresses do.
    function testUninitializedPoolIsUnavailable() public {
        address tokenA = makeAddr("tokenA");
        address tokenB = makeAddr("tokenB");
        address pool = IAlgebraFactoryTest(FACTORY).createPool(tokenA, tokenB);
        (address token0, address token1) =
            tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);

        address[] memory tokens = adapter.getTokens(poolId(pool));
        assertEq(tokens[0], token0);
        assertEq(tokens[1], token1);

        bytes memory unavailable = abi.encodeWithSelector(
            ISwapAdapterTypes.Unavailable.selector, "pool is not initialized"
        );
        vm.expectRevert(unavailable);
        adapter.getLimits(poolId(pool), token0, token1);

        uint256[] memory amounts = new uint256[](1);
        vm.expectRevert(unavailable);
        adapter.price(poolId(pool), token0, token1, amounts);

        amounts[0] = 1 ether;
        vm.expectRevert(unavailable);
        adapter.price(poolId(pool), token0, token1, amounts);

        vm.expectRevert(unavailable);
        adapter.swap(poolId(pool), token0, token1, OrderSide.Sell, 1 ether);
    }

    /// @dev A pool parked at the end of its price range cannot trade in that
    /// direction; the price at zero reports it as unavailable instead of
    /// surfacing the pool's own `SPL` revert. The price is forced by writing
    /// the pool's `globalState` slot, which packs the sqrt price in its low
    /// 160 bits.
    function testPriceAtBoundaryReverts() public {
        uint256[] memory amounts = new uint256[](1);
        bytes32 slot = bytes32(uint256(2));
        uint256 state = uint256(vm.load(WETH_USDC_POOL, slot))
            & ~uint256(type(uint160).max);

        vm.store(
            WETH_USDC_POOL,
            slot,
            bytes32(state | (CamelotV3TickMath.MIN_SQRT_RATIO + 1))
        );
        vm.expectRevert(
            abi.encodeWithSelector(
                ISwapAdapterTypes.Unavailable.selector,
                "pool price is at its boundary"
            )
        );
        adapter.price(poolId(WETH_USDC_POOL), WETH, USDC, amounts);

        vm.store(
            WETH_USDC_POOL,
            slot,
            bytes32(state | (CamelotV3TickMath.MAX_SQRT_RATIO - 1))
        );
        vm.expectRevert(
            abi.encodeWithSelector(
                ISwapAdapterTypes.Unavailable.selector,
                "pool price is at its boundary"
            )
        );
        adapter.price(poolId(WETH_USDC_POOL), USDC, WETH, amounts);
    }

    function testCallbackRejectsUnknownPool() public {
        bytes memory data = abi.encode(
            CamelotV3SwapAdapter.CallbackData(WETH, USDC, address(this), false)
        );
        vm.expectRevert(
            CamelotV3SwapAdapter.CamelotV3SwapAdapter__UnknownPool.selector
        );
        adapter.algebraSwapCallback(1, 0, data);
    }

    /// @dev A real Camelot pool cannot collect a payment for a pair it does
    /// not serve: the caller must be the factory's pool for the pair named in
    /// the callback data.
    function testCallbackRejectsPoolOfAnotherPair() public {
        bytes memory data = abi.encode(
            CamelotV3SwapAdapter.CallbackData(WETH, USDC, address(this), false)
        );
        vm.prank(WETH_ARB_POOL);
        vm.expectRevert(
            CamelotV3SwapAdapter.CamelotV3SwapAdapter__UnknownPool.selector
        );
        adapter.algebraSwapCallback(1, 0, data);
    }

    function testExecuteQuoteRejectsExternalCallers() public {
        vm.expectRevert(
            CamelotV3SwapAdapter.CamelotV3SwapAdapter__NotSelf.selector
        );
        adapter.executeQuote(IAlgebraPool(WETH_USDC_POOL), true, 1, 4295128740);
    }

    /// @dev Quoting must leave the pool untouched, even for the largest quote
    /// in each direction, which crosses every walked tick.
    function testQuotesDoNotChangePoolState() public {
        bytes32 pool = poolId(WETH_USDC_POOL);
        bytes32 before = poolStateHash();

        uint256[] memory sellWeth = adapter.getLimits(pool, WETH, USDC);
        uint256[] memory sellUsdc = adapter.getLimits(pool, USDC, WETH);
        uint256[] memory amounts = new uint256[](2);
        amounts[1] = sellWeth[0];
        adapter.price(pool, WETH, USDC, amounts);
        amounts[1] = sellUsdc[0];
        adapter.price(pool, USDC, WETH, amounts);

        assertEq(poolStateHash(), before, "quotes leave the pool untouched");
    }
}
