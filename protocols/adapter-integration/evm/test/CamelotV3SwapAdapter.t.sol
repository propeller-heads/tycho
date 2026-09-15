// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import "./AdapterTest.sol";
import "forge-std/Test.sol";
import "src/interfaces/ISwapAdapterTypes.sol";
import "src/libraries/FractionMath.sol";
import "src/camelot-v3/CamelotV3SwapAdapter.sol";

/// @dev Fork tests against Camelot V3 on Arbitrum One. `ARBITRUM_RPC_URL`
/// must point at a node that serves state at the fork block.
contract CamelotV3SwapAdapterTest is AdapterTest {
    using FractionMath for Fraction;

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

    uint256 constant FORK_BLOCK = 504411371;
    uint256 constant FEE_DENOMINATOR = 1e6;

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

    function testGetTokens() public view {
        address[] memory tokens = adapter.getTokens(poolId(WETH_USDC_POOL));
        assertEq(tokens.length, 2);
        assertEq(tokens[0], WETH);
        assertEq(tokens[1], USDC);
    }

    /// @dev The repo's shared adapter behaviour test: marginal prices fall as
    /// the sold amount grows, every executed price sits between the marginal
    /// prices before and after the trade, and amounts 5% above the walked
    /// limit still price and swap, since the limit is only a lower bound.
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

    function testRejectsTokensOutsideThePair() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                ISwapAdapterTypes.InvalidOrder.selector,
                "tokens are not the pool's pair"
            )
        );
        adapter.getLimits(poolId(WETH_USDC_POOL), WETH, ARB);
    }

    function testGetLimits() public view {
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

    /// @dev Selling exactly the limit is fully executed: the limit is what the
    /// walked liquidity absorbs net of fees, so the pool never runs short.
    function testSellingTheLimitIsFullyExecuted() public {
        bytes32 pool = poolId(WETH_USDC_POOL);
        uint256 limit = adapter.getLimits(pool, WETH, USDC)[0];

        deal(WETH, address(this), limit);
        IERC20(WETH).approve(address(adapter), limit);
        Trade memory trade =
            adapter.swap(pool, WETH, USDC, OrderSide.Sell, limit);

        assertEq(IERC20(WETH).balanceOf(address(this)), 0);
        assertEq(IERC20(USDC).balanceOf(address(this)), trade.calculatedAmount);
    }

    /// @dev The price at zero uses the pool's current sqrt price and the fee
    /// the pool charges the next swap, which the pool only computes when a
    /// swap runs. That fee is read here by running a 1 wei swap in a snapshot.
    function testPriceAtZeroMatchesPoolState() public {
        bytes32 pool = poolId(WETH_USDC_POOL);
        (uint160 sqrtPrice,,,,,,,) = IAlgebraPool(WETH_USDC_POOL).globalState();

        uint256 snapshot = vm.snapshot();
        deal(WETH, address(this), 1);
        IERC20(WETH).approve(address(adapter), 1);
        adapter.swap(pool, WETH, USDC, OrderSide.Sell, 1);
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
    /// amount, and prices behave like a pool with price impact: executed
    /// price <= price before, and > marginal price after.
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

        Trade memory trade = adapter.swap(
            poolId(pool), tokenIn, tokenOut, OrderSide.Sell, amount
        );

        assertEq(IERC20(tokenIn).balanceOf(address(this)), 0, "input spent");
        assertEq(
            IERC20(tokenOut).balanceOf(address(this)) - outBefore,
            trade.calculatedAmount,
            "output received"
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
        bytes32 pool = poolId(WETH_USDC_POOL);
        uint256 wanted = 1_000e6;
        deal(WETH, address(this), 10 ether);
        IERC20(WETH).approve(address(adapter), 10 ether);

        Trade memory trade =
            adapter.swap(pool, WETH, USDC, OrderSide.Buy, wanted);

        assertEq(IERC20(USDC).balanceOf(address(this)), wanted, "exact output");
        assertEq(
            10 ether - IERC20(WETH).balanceOf(address(this)),
            trade.calculatedAmount,
            "input paid"
        );
        assertGt(trade.gasUsed, 0);
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

    function testSwapZeroAmountIsNoop() public {
        Trade memory trade =
            adapter.swap(poolId(WETH_USDC_POOL), WETH, USDC, OrderSide.Sell, 0);
        assertEq(trade.calculatedAmount, 0);
        assertEq(trade.gasUsed, 0);
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

    function testExecuteQuoteRejectsExternalCallers() public {
        vm.expectRevert(
            CamelotV3SwapAdapter.CamelotV3SwapAdapter__NotSelf.selector
        );
        adapter.executeQuote(IAlgebraPool(WETH_USDC_POOL), true, 1, 4295128740);
    }

    /// @dev Quoting must leave the pool untouched.
    function testQuotesDoNotChangePoolState() public {
        bytes32 pool = poolId(WETH_USDC_POOL);
        (uint160 priceBefore,,,, uint16 indexBefore,,,) =
            IAlgebraPool(WETH_USDC_POOL).globalState();
        uint256[] memory amounts = new uint256[](2);
        amounts[1] = 1 ether;

        adapter.getLimits(pool, WETH, USDC);
        adapter.price(pool, WETH, USDC, amounts);

        (uint160 priceAfter,,,, uint16 indexAfter,,,) =
            IAlgebraPool(WETH_USDC_POOL).globalState();
        assertEq(priceAfter, priceBefore);
        assertEq(indexAfter, indexBefore);
    }
}
