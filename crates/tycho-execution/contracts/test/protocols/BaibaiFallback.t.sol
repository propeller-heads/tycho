// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {TestUtils} from "../TestUtils.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {FallbackRouterAssertions, FallbackSwaps} from "./Fallback.t.sol";
import {IBaibaiFees} from "./Baibai.t.sol";
import {
    FallbackExecutor__InvalidDataLength
} from "../../src/executors/FallbackExecutor.sol";
import {
    BaibaiFallbackExecutor
} from "../../src/executors/BaibaiFallbackExecutor.sol";
import {TychoFallbackRouter} from "../../src/fallback/TychoFallbackRouter.sol";
import {
    BaibaiFallbackRouter,
    BaibaiFallbackRouter__AddressZero,
    BaibaiFallbackRouter__ZeroQuoteToken
} from "../../src/fallback/BaibaiFallbackRouter.sol";

/// @notice Runs the router as `deploy-fallback-router.js` deploys it on Base, against BaiBai's
/// entrypoint with the chain's WETH/USDC Uniswap V3 pool as fallback.
contract BaibaiFallbackRouterTest is FallbackRouterAssertions, TestUtils {
    /// After the Entrypoint upgrade at 53492231, the block `BaibaiTest` forks.
    uint256 constant FORK_BLOCK = 53_545_000;
    address constant BAIBAI_ENTRYPOINT =
        0x98c1D9E102Eb2806D902b13186BDc7892aC4fFBa;
    address constant CUSTODIAN = 0xAaC48FEB93c5C97E0fb3c7C57E1633922A4ACDa3;
    address constant BASE_USDC_WETH_USV3 =
        0xd0b53D9277642d899DF5C87A3966A349A798F224;
    address constant BASE_POOL_MANAGER =
        0x498581fF718922c3f8e6A244956aF099B2652b2b;
    address constant BASE_STATIC_QUOTER =
        0x28aF629a9F3ECE3c8D9F0b7cCf6349708CeC8cFb;
    uint256 constant WETH_IN = 0.01 ether;
    uint256 constant USDC_IN = 10e6;

    BaibaiFallbackRouter router;
    BaibaiFallbackExecutor executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), FORK_BLOCK);
        router = _deployRouter(BAIBAI_ENTRYPOINT);
        executor = new BaibaiFallbackExecutor(address(router));
    }

    function testRejectsZeroEntrypoint() public {
        vm.expectRevert(BaibaiFallbackRouter__AddressZero.selector);
        _deployRouter(address(0));
    }

    function testRejectsEntrypointWithoutQuoteToken() public {
        vm.mockCall(
            BAIBAI_ENTRYPOINT,
            abi.encodeWithSignature("quoteToken()"),
            abi.encode(address(0))
        );
        vm.expectRevert(BaibaiFallbackRouter__ZeroQuoteToken.selector);
        _deployRouter(BAIBAI_ENTRYPOINT);
    }

    function testReadsTheQuoteToken() public view {
        assertEq(router.quoteToken(), BASE_USDC);
        assertEq(address(router.entrypoint()), BAIBAI_ENTRYPOINT);
    }

    /// A fallback that cannot quote leaves BaiBai to fill, at its quote for this router, in both
    /// directions.
    function testBaibaiFillsTheQuotedAmount() public {
        for (uint8 sell; sell < 2; ++sell) {
            bool sellBase = sell == 1;
            uint256 snapshot = vm.snapshotState();
            (address tokenIn, address tokenOut, uint256 amountIn) =
                _direction(sellBase);
            uint256 quoted = _baibaiQuote(tokenIn, amountIn);
            assertGt(quoted, 0);
            uint256 custodyBefore = IERC20(tokenIn).balanceOf(CUSTODIAN);

            vm.recordLogs();
            _swap(sellBase, FallbackSwaps.uniswapV3(address(0xdead)));

            assertEq(IERC20(tokenOut).balanceOf(BOB), quoted);
            assertEq(
                IERC20(tokenIn).balanceOf(CUSTODIAN) - custodyBefore, amountIn
            );
            _assertNoFallbackSwap(address(router), vm.getRecordedLogs());
            _assertRouterDrained(address(router), tokenIn, tokenOut);
            assertEq(
                IERC20(tokenIn).allowance(address(router), BAIBAI_ENTRYPOINT), 0
            );
            vm.revertToState(snapshot);
        }
    }

    /// The router runs the higher of the two quotes.
    function testPaysTheHigherQuote() public {
        bytes memory v3 = FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3);
        uint256 baibaiOut = _baibaiQuote(BASE_WETH, WETH_IN);
        uint256 v3Out = _quoteFallback(true, v3);

        _swap(true, v3);

        uint256 received = IERC20(BASE_USDC).balanceOf(BOB);
        assertEq(received, baibaiOut >= v3Out ? baibaiOut : v3Out);
        _assertRouterDrained(address(router), BASE_WETH, BASE_USDC);
    }

    /// A 10% taker fee for the router puts BaiBai's quote below the fallback's.
    function testFallsBackWhenBaibaiQuotesLower() public {
        vm.prank(IBaibaiFees(BAIBAI_ENTRYPOINT).owner());
        IBaibaiFees(BAIBAI_ENTRYPOINT)
            .setTakerFee(address(router), BASE_WETH, 1000);
        bytes memory v3 = FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3);
        uint256 v3Out = _quoteFallback(true, v3);
        assertLt(_baibaiQuote(BASE_WETH, WETH_IN), v3Out);

        _expectFallbackSwap(
            address(router),
            BAIBAI_ENTRYPOINT,
            BASE_WETH,
            BASE_USDC,
            WETH_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        _swap(true, v3);

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), v3Out);
        _assertRouterDrained(address(router), BASE_WETH, BASE_USDC);
    }

    /// A day later the curve is stale: `quoteFor` reverts, so BaiBai quotes zero.
    function testStaleCurveFallsBack() public {
        vm.warp(block.timestamp + 1 days);
        bytes memory v3 = FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3);
        uint256 v3Out = _quoteFallback(true, v3);

        _expectFallbackSwap(
            address(router),
            BAIBAI_ENTRYPOINT,
            BASE_WETH,
            BASE_USDC,
            WETH_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        _swap(true, v3);

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), v3Out);
        _assertRouterDrained(address(router), BASE_WETH, BASE_USDC);
    }

    /// BaiBai wins the quote and then reverts: the fallback fills the leg.
    function testFallsBackWhenBaibaiSwapReverts() public {
        bytes memory v3 = FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3);
        uint256 v3Out = _quoteFallback(true, v3);
        vm.mockCall(
            BAIBAI_ENTRYPOINT,
            abi.encodeWithSignature(
                "quoteFor(address,address,uint256,address)",
                BASE_WETH,
                BASE_WETH,
                WETH_IN,
                address(router)
            ),
            abi.encode(v3Out + 1, uint256(0))
        );
        vm.mockCallRevert(
            BAIBAI_ENTRYPOINT,
            abi.encodeWithSignature(
                "swapExactAmountIn(address,address,uint256,uint256,address)"
            ),
            ""
        );

        _expectFallbackSwap(
            address(router),
            BAIBAI_ENTRYPOINT,
            BASE_WETH,
            BASE_USDC,
            WETH_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.PrimaryFailed
        );
        _swap(true, v3);

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), v3Out);
        _assertRouterDrained(address(router), BASE_WETH, BASE_USDC);
        assertEq(
            IERC20(BASE_WETH).allowance(address(router), BAIBAI_ENTRYPOINT), 0
        );
    }

    /// An entrypoint whose `quoteFor` returns fewer than two words quotes zero instead of
    /// reverting the swap.
    function testUndecodableQuoteFallsBack() public {
        vm.mockCall(
            BAIBAI_ENTRYPOINT,
            abi.encodeWithSignature(
                "quoteFor(address,address,uint256,address)"
            ),
            abi.encode(uint256(1))
        );
        bytes memory v3 = FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3);
        uint256 v3Out = _quoteFallback(true, v3);

        _swap(true, v3);

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), v3Out);
    }

    /// Two tokens that are not a base and the quote token name no BaiBai pair, so BaiBai is not
    /// asked to quote.
    function testTokensOutsideAPairAreNotQuoted() public {
        deal(BASE_USDC, address(router), USDC_IN);
        TychoFallbackRouter.Swap memory usdcToUsdc =
            FallbackSwaps.swap(BASE_USDC, BASE_USDC, USDC_IN, BOB);

        vm.expectCall(
            BAIBAI_ENTRYPOINT,
            abi.encodeWithSelector(IBaibaiFees.quoteFor.selector),
            0
        );
        // Only BaiBai's part is under test: whatever the fallback does with these tokens.
        try router.swap(
            usdcToUsdc, FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3)
        ) {}
            catch {}
    }

    /// The swap data comes from the Rust encoder's `test_encode_baibai_with_uniswap_v3_fallback`.
    function testExecutorSwapsRustEncodedData() public {
        for (uint8 sell; sell < 2; ++sell) {
            bool sellBase = sell == 1;
            uint256 snapshot = vm.snapshotState();
            (address tokenIn, address tokenOut, uint256 amountIn) =
                _direction(sellBase);
            bytes memory data = loadCallDataFromFile(
                sellBase
                    ? "test_encode_baibai_fallback_sell_base_for_solidity"
                    : "test_encode_baibai_fallback_buy_base_for_solidity"
            );
            uint256 v3Out = _quoteFallback(
                sellBase, FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3)
            );
            uint256 baibaiOut = _baibaiQuote(tokenIn, amountIn);
            deal(tokenIn, address(router), amountIn);

            executor.swap(amountIn, data, BOB);

            assertEq(
                IERC20(tokenOut).balanceOf(BOB),
                baibaiOut >= v3Out ? baibaiOut : v3Out
            );
            _assertRouterDrained(address(router), tokenIn, tokenOut);
            vm.revertToState(snapshot);
        }
    }

    function testExecutorGetTransferData() public view {
        bytes memory data = abi.encodePacked(
            BASE_WETH, BASE_USDC, FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3)
        );
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(data);

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, address(router));
        assertEq(tokenIn, BASE_WETH);
        assertEq(tokenOut, BASE_USDC);
        assertFalse(outputToRouter);
        assertEq(executor.fundsExpectedAddress(data), address(router));
    }

    function testExecutorRejectsDataWithoutFallback() public {
        bytes memory data = abi.encodePacked(BASE_WETH, BASE_USDC);
        vm.expectRevert(
            abi.encodeWithSelector(
                FallbackExecutor__InvalidDataLength.selector, data.length
            )
        );
        executor.getTransferData(data);
    }

    function _deployRouter(address entrypoint)
        internal
        returns (BaibaiFallbackRouter)
    {
        return new BaibaiFallbackRouter(
            IPoolManager(BASE_POOL_MANAGER),
            address(0),
            IUniswapV3StaticQuoter(BASE_STATIC_QUOTER),
            entrypoint
        );
    }

    function _direction(bool sellBase)
        internal
        pure
        returns (address tokenIn, address tokenOut, uint256 amountIn)
    {
        return sellBase
            ? (BASE_WETH, BASE_USDC, WETH_IN)
            : (BASE_USDC, BASE_WETH, USDC_IN);
    }

    /// Funds the router and swaps through it, as the executor does after the TychoRouter's
    /// transfer.
    function _swap(bool sellBase, bytes memory fallbackSwap) internal {
        (address tokenIn,, uint256 amountIn) = _direction(sellBase);
        deal(tokenIn, address(router), amountIn);
        router.swap(_swapStruct(sellBase), fallbackSwap);
    }

    function _swapStruct(bool sellBase)
        internal
        view
        returns (TychoFallbackRouter.Swap memory)
    {
        (address tokenIn, address tokenOut, uint256 amountIn) =
            _direction(sellBase);
        return FallbackSwaps.swap(tokenIn, tokenOut, amountIn, BOB);
    }

    function _baibaiQuote(address tokenIn, uint256 amountIn)
        internal
        view
        returns (uint256 amountOut)
    {
        (amountOut,) = IBaibaiFees(BAIBAI_ENTRYPOINT)
            .quoteFor(BASE_WETH, tokenIn, amountIn, address(router));
    }

    function _quoteFallback(bool sellBase, bytes memory fallbackSwap)
        internal
        returns (uint256)
    {
        vm.prank(address(router));
        return router.quoteFallback(_swapStruct(sellBase), fallbackSwap);
    }
}
