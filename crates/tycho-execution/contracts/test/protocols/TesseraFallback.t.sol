// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {TestUtils} from "../TestUtils.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {FallbackRouterAssertions, FallbackSwaps} from "./Fallback.t.sol";
import {TESSERA_FORK_BLOCK} from "./Tessera.t.sol";
import {ITesseraSwap} from "../../src/executors/TesseraExecutor.sol";
import {
    FallbackExecutor__InvalidDataLength
} from "../../src/executors/FallbackExecutor.sol";
import {
    TesseraFallbackExecutor
} from "../../src/executors/TesseraFallbackExecutor.sol";
import {TychoFallbackRouter} from "../../src/fallback/TychoFallbackRouter.sol";
import {
    TesseraFallbackRouter,
    TesseraFallbackRouter__AddressZero
} from "../../src/fallback/TesseraFallbackRouter.sol";

/// @notice A `TesseraSwap` that quotes `amountOut`, then pulls only half the input and pays
/// `amountOut`. The live one pulls the whole input.
contract MockPartialTesseraSwap {
    uint256 public immutable amountOut;

    constructor(uint256 amountOut_) {
        amountOut = amountOut_;
    }

    function tesseraSwapViewAmounts(address, address, int256 amountSpecified)
        external
        view
        returns (uint256, uint256)
    {
        return (uint256(amountSpecified), amountOut);
    }

    function tesseraSwapWithAllowances(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified,
        uint256, /* amountCheck */
        address recipient,
        bytes calldata /* swapData */
    ) external {
        IERC20(tokenIn)
            .transferFrom(
                msg.sender, address(this), uint256(amountSpecified) / 2
            );
        IERC20(tokenOut).transfer(recipient, amountOut);
    }
}

contract TesseraFallbackRouterExposed is TesseraFallbackRouter {
    constructor(
        IPoolManager poolManager_,
        IUniswapV3StaticQuoter uniswapV3StaticQuoter_,
        address tesseraSwap_
    )
        TesseraFallbackRouter(
            poolManager_, address(0), uniswapV3StaticQuoter_, tesseraSwap_
        )
    {}

    function quotePrimary(Swap calldata swap_) external returns (uint256) {
        return _quotePrimary(swap_, address(tesseraSwap), msg.data[0:0]);
    }
}

/// @notice Runs the router against Tessera's live WETH/USDC pair on a Base fork, with the chain's
/// Uniswap V3 pool as fallback. The paths Tessera cannot take at the fork block, a low quote, no
/// quote or a failing swap, are forced by mocking `TesseraSwap`.
contract TesseraFallbackRouterTest is FallbackRouterAssertions, TestUtils {
    uint256 constant WETH_IN = 0.01 ether;
    address constant BASE_USDC_WETH_USV3 =
        0xd0b53D9277642d899DF5C87A3966A349A798F224;
    address constant BASE_POOL_MANAGER =
        0x498581fF718922c3f8e6A244956aF099B2652b2b;
    address constant BASE_STATIC_QUOTER =
        0x28aF629a9F3ECE3c8D9F0b7cCf6349708CeC8cFb;

    TesseraFallbackRouter router;
    TesseraFallbackExecutor executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), TESSERA_FORK_BLOCK);
        router = _newRouter(TESSERA_SWAP);
        executor = new TesseraFallbackExecutor(address(router));
        deal(BASE_WETH, address(router), WETH_IN);
    }

    function testConstructorRejectsZeroTesseraSwap() public {
        vm.expectRevert(TesseraFallbackRouter__AddressZero.selector);
        _newRouter(address(0));
    }

    /// A fallback pool with no code quotes zero, so Tessera fills, at exactly its view quote, and
    /// the router keeps no allowance to `TesseraSwap`.
    function testTesseraFillsTheQuotedAmount() public {
        uint256 tesseraOut = _tesseraQuote();
        assertGt(tesseraOut, 0);

        vm.recordLogs();
        router.swap(_swapStruct(), FallbackSwaps.uniswapV3(address(0xdead)));

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), tesseraOut);
        _assertNoFallbackSwap(address(router), vm.getRecordedLogs());
        _assertRouterDrained(address(router), BASE_WETH, BASE_USDC);
        assertEq(IERC20(BASE_WETH).allowance(address(router), TESSERA_SWAP), 0);
    }

    /// The router leaves `TesseraSwap` no allowance, even when it pulls less than approved.
    function testRevokesTheApprovalAfterTheSwap() public {
        MockPartialTesseraSwap tessera = new MockPartialTesseraSwap(1e6);
        deal(BASE_USDC, address(tessera), 1e6);
        TesseraFallbackRouter partialRouter = _newRouter(address(tessera));
        deal(BASE_WETH, address(partialRouter), WETH_IN);

        partialRouter.swap(
            _swapStruct(), FallbackSwaps.uniswapV3(address(0xdead))
        );

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), 1e6);
        assertEq(
            IERC20(BASE_WETH)
                .allowance(address(partialRouter), address(tessera)),
            0
        );
    }

    /// Against the live Uniswap V3 pool the router pays whichever quotes more.
    function testPaysTheHigherQuote() public {
        bytes memory v3 = FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3);
        uint256 tesseraOut = _tesseraQuote();
        uint256 v3Out = _quoteFallback(v3);

        router.swap(_swapStruct(), v3);

        assertEq(
            IERC20(BASE_USDC).balanceOf(BOB),
            tesseraOut >= v3Out ? tesseraOut : v3Out
        );
        _assertRouterDrained(address(router), BASE_WETH, BASE_USDC);
    }

    function testFallsBackWhenTesseraQuotesLower() public {
        bytes memory v3 = FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3);
        uint256 v3Out = _quoteFallback(v3);
        _mockTesseraQuote(1);

        vm.expectCall(
            TESSERA_SWAP,
            abi.encodeWithSelector(
                ITesseraSwap.tesseraSwapWithAllowances.selector
            ),
            0
        );
        _expectFallbackSwap(
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        router.swap(_swapStruct(), v3);

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), v3Out);
        _assertRouterDrained(address(router), BASE_WETH, BASE_USDC);
    }

    /// A pair that cannot quote, such as one past its price cutoff, reverts the view, which
    /// quotes zero.
    function testFallsBackWhenTesseraCannotQuote() public {
        bytes memory v3 = FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3);
        uint256 v3Out = _quoteFallback(v3);
        vm.mockCallRevert(
            TESSERA_SWAP,
            abi.encodeWithSelector(
                ITesseraSwap.tesseraSwapViewAmounts.selector
            ),
            "stale price"
        );

        _expectFallbackSwap(
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        router.swap(_swapStruct(), v3);

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), v3Out);
        _assertRouterDrained(address(router), BASE_WETH, BASE_USDC);
    }

    /// Tessera wins the quote but its swap reverts, as one past the treasury's balance does: the
    /// swap falls through to the fallback, and the approval rolls back with it.
    function testFallsBackWhenTesseraSwapFails() public {
        bytes memory v3 = FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3);
        uint256 v3Out = _quoteFallback(v3);
        _mockTesseraQuote(type(uint128).max);
        vm.mockCallRevert(
            TESSERA_SWAP,
            abi.encodeWithSelector(
                ITesseraSwap.tesseraSwapWithAllowances.selector
            ),
            "treasury short"
        );

        _expectFallbackSwap(TychoFallbackRouter.FallbackReason.PrimaryFailed);
        router.swap(_swapStruct(), v3);

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), v3Out);
        _assertRouterDrained(address(router), BASE_WETH, BASE_USDC);
        assertEq(IERC20(BASE_WETH).allowance(address(router), TESSERA_SWAP), 0);
    }

    /// Tessera wins on its view quote but its swap pays less, as one charged by the pair's fee
    /// module does: the swap requires the quote, reverts, and the fallback fills.
    function testFallsBackWhenTesseraPaysLessThanItQuoted() public {
        bytes memory v3 = FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3);
        uint256 v3Out = _quoteFallback(v3);
        _mockTesseraQuote(_tesseraQuote() + 1);

        _expectFallbackSwap(TychoFallbackRouter.FallbackReason.PrimaryFailed);
        router.swap(_swapStruct(), v3);

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), v3Out);
        _assertRouterDrained(address(router), BASE_WETH, BASE_USDC);
        assertEq(IERC20(BASE_WETH).allowance(address(router), TESSERA_SWAP), 0);
    }

    function testPaysTheHigherQuoteFromUsdcToWeth() public {
        uint256 usdcIn = 25e6;
        TychoFallbackRouter.Swap memory swap_ =
            FallbackSwaps.swap(BASE_USDC, BASE_WETH, usdcIn, BOB);
        bytes memory v3 = FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3);
        vm.startPrank(address(router));
        (, uint256 tesseraOut) = ITesseraSwap(TESSERA_SWAP)
            .tesseraSwapViewAmounts(BASE_USDC, BASE_WETH, int256(usdcIn));
        uint256 v3Out = router.quoteFallback(swap_, v3);
        vm.stopPrank();
        assertGt(tesseraOut, 0);
        deal(BASE_WETH, address(router), 0);
        deal(BASE_USDC, address(router), usdcIn);

        router.swap(swap_, v3);

        assertEq(
            IERC20(BASE_WETH).balanceOf(BOB),
            tesseraOut >= v3Out ? tesseraOut : v3Out
        );
        _assertRouterDrained(address(router), BASE_USDC, BASE_WETH);
    }

    /// A negative amount would quote exact-out, so an amount above int256's maximum quotes zero
    /// without asking Tessera.
    function testQuotesZeroAboveInt256Max() public {
        TesseraFallbackRouterExposed exposed = _newExposedRouter();
        vm.expectCall(
            TESSERA_SWAP,
            abi.encodeWithSelector(
                ITesseraSwap.tesseraSwapViewAmounts.selector
            ),
            0
        );

        uint256 quote = exposed.quotePrimary(
            FallbackSwaps.swap(
                BASE_WETH, BASE_USDC, uint256(type(int256).max) + 1, BOB
            )
        );

        assertEq(quote, 0);
    }

    function testQuotesZeroOnShortReturnData() public {
        TesseraFallbackRouterExposed exposed = _newExposedRouter();
        vm.mockCall(
            TESSERA_SWAP,
            abi.encodeWithSelector(
                ITesseraSwap.tesseraSwapViewAmounts.selector
            ),
            abi.encode(WETH_IN)
        );

        assertEq(exposed.quotePrimary(_swapStruct()), 0);
    }

    function testExecutorGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(_executorData());

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, address(router));
        assertEq(tokenIn, BASE_WETH);
        assertEq(tokenOut, BASE_USDC);
        assertFalse(outputToRouter);
        assertEq(
            executor.fundsExpectedAddress(_executorData()), address(router)
        );
    }

    /// 40 bytes carry the tokens but no fallback.
    function testExecutorRejectsDataWithoutFallback() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                FallbackExecutor__InvalidDataLength.selector, 40
            )
        );
        executor.getTransferData(abi.encodePacked(BASE_WETH, BASE_USDC));
    }

    /// The swap data comes from the Rust encoder's `test_encode_tessera_fallback_for_solidity`,
    /// which names the same Uniswap V3 pool.
    function testExecutorSwapsRustEncodedData() public {
        uint256 tesseraOut = _tesseraQuote();
        uint256 v3Out =
            _quoteFallback(FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3));

        executor.swap(
            WETH_IN,
            loadCallDataFromFile("test_encode_tessera_fallback_for_solidity"),
            BOB
        );

        assertEq(
            IERC20(BASE_USDC).balanceOf(BOB),
            tesseraOut >= v3Out ? tesseraOut : v3Out
        );
        _assertRouterDrained(address(router), BASE_WETH, BASE_USDC);
    }

    /// The same swap data with Tessera quoting low, so the fallback the data names fills.
    function testExecutorSwapsRustEncodedDataThroughTheFallback() public {
        uint256 v3Out =
            _quoteFallback(FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3));
        _mockTesseraQuote(1);

        _expectFallbackSwap(
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        executor.swap(
            WETH_IN,
            loadCallDataFromFile("test_encode_tessera_fallback_for_solidity"),
            BOB
        );

        assertEq(IERC20(BASE_USDC).balanceOf(BOB), v3Out);
        _assertRouterDrained(address(router), BASE_WETH, BASE_USDC);
    }

    function _newRouter(address tesseraSwap)
        internal
        returns (TesseraFallbackRouter)
    {
        return new TesseraFallbackRouter(
            IPoolManager(BASE_POOL_MANAGER),
            address(0),
            IUniswapV3StaticQuoter(BASE_STATIC_QUOTER),
            tesseraSwap
        );
    }

    function _newExposedRouter()
        internal
        returns (TesseraFallbackRouterExposed)
    {
        return new TesseraFallbackRouterExposed(
            IPoolManager(BASE_POOL_MANAGER),
            IUniswapV3StaticQuoter(BASE_STATIC_QUOTER),
            TESSERA_SWAP
        );
    }

    /// Tessera's view quote, priced for the router as its caller, as the swap is.
    function _tesseraQuote() internal returns (uint256 amountOut) {
        vm.prank(address(router));
        (, amountOut) = ITesseraSwap(TESSERA_SWAP)
            .tesseraSwapViewAmounts(BASE_WETH, BASE_USDC, int256(WETH_IN));
    }

    function _quoteFallback(bytes memory fallbackSwap)
        internal
        returns (uint256)
    {
        vm.prank(address(router));
        return router.quoteFallback(_swapStruct(), fallbackSwap);
    }

    function _mockTesseraQuote(uint256 amountOut) internal {
        vm.mockCall(
            TESSERA_SWAP,
            abi.encodeWithSelector(
                ITesseraSwap.tesseraSwapViewAmounts.selector
            ),
            abi.encode(WETH_IN, amountOut)
        );
    }

    function _swapStruct()
        internal
        view
        returns (TychoFallbackRouter.Swap memory)
    {
        return FallbackSwaps.swap(BASE_WETH, BASE_USDC, WETH_IN, BOB);
    }

    function _executorData() internal view returns (bytes memory) {
        return abi.encodePacked(
            BASE_WETH, BASE_USDC, FallbackSwaps.uniswapV3(BASE_USDC_WETH_USV3)
        );
    }

    function _expectFallbackSwap(TychoFallbackRouter.FallbackReason reason)
        internal
    {
        _expectFallbackSwap(
            address(router),
            TESSERA_SWAP,
            BASE_WETH,
            BASE_USDC,
            WETH_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            reason
        );
    }
}
