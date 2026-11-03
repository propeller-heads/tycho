// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {TestUtils} from "../TestUtils.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {FallbackRouterAssertions, FallbackSwaps} from "./Fallback.t.sol";
import {LiquoriceOrders} from "./Liquorice.t.sol";
import {
    FallbackExecutor__InvalidDataLength
} from "../../src/executors/FallbackExecutor.sol";
import {
    LiquoriceFallbackExecutor
} from "../../src/executors/LiquoriceFallbackExecutor.sol";
import {TychoFallbackRouter} from "../../src/fallback/TychoFallbackRouter.sol";
import {
    LiquoriceFallbackRouter,
    LiquoriceFallbackRouter__AddressZero,
    LiquoriceFallbackRouter__InvalidDataLength,
    LiquoriceFallbackRouter__InvalidSelector
} from "../../src/fallback/LiquoriceFallbackRouter.sol";

/// @dev Uses `LiquoriceExecutorTest.testSettleSingle`'s signed quote, whose trader and receiver
/// is `TRADER`.
contract LiquoriceFallbackRouterTest is
    LiquoriceOrders,
    FallbackRouterAssertions,
    TestUtils
{
    address constant TRADER = 0x5615dEB798BB3E4dFa0139dFa1b3D433Cc23b72f;
    uint256 constant SIGNED_USDC_IN = 3000e6;
    uint256 constant SIGNED_WETH_OUT = 1 ether;
    LiquoriceFallbackRouter router;
    LiquoriceFallbackExecutor executor;

    function setUp() public {
        _setUpLiquorice(SIGNED_WETH_OUT);

        router = new LiquoriceFallbackRouter(
            IPoolManager(POOL_MANAGER),
            FLUIDV1_LIQUIDITY,
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER),
            LIQUORICE_SETTLEMENT,
            LIQUORICE_BALANCE_MANAGER
        );
        // The test contract's first deployment lands on the quote's trader.
        assertEq(address(router), TRADER);
        executor = new LiquoriceFallbackExecutor(address(router));

        _allowSolver(address(router));
    }

    /// @dev The quote names the router as trader and receiver, as the encoder requests it.
    function _signedQuote() internal view returns (bytes memory) {
        return
            _encodeSettleSingle(
                address(router), SIGNED_USDC_IN, SIGNED_WETH_OUT
            );
    }

    /// The signed price needs no comparison, so the fallback pool is never quoted.
    function testInputAtSignedAmount() public {
        deal(USDC_ADDR, address(router), SIGNED_USDC_IN);

        vm.expectCall(
            UNISWAP_V3_STATIC_QUOTER,
            abi.encodeWithSelector(IUniswapV3StaticQuoter.quote.selector),
            uint64(0)
        );
        vm.recordLogs();
        _swap(SIGNED_USDC_IN, _signedQuote());

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), SIGNED_WETH_OUT);
        _assertNoFallbackSwap(address(router), vm.getRecordedLogs());
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
        assertEq(
            IERC20(USDC_ADDR)
                .allowance(address(router), LIQUORICE_BALANCE_MANAGER),
            0
        );
    }

    /// The quote fills its signed amount and the caller gets the rest of the input back.
    function testInputAboveSignedAmount() public {
        uint256 remainder = 100e6;
        deal(USDC_ADDR, address(router), SIGNED_USDC_IN + remainder);
        uint256 callerUsdcBefore = IERC20(USDC_ADDR).balanceOf(address(this));

        _swap(SIGNED_USDC_IN + remainder, _signedQuote());

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), SIGNED_WETH_OUT);
        assertEq(
            IERC20(USDC_ADDR).balanceOf(address(this)) - callerUsdcBefore,
            remainder
        );
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
    }

    /// The settlement cannot pull the signed amount from a smaller input, so the fallback fills
    /// the whole leg.
    function testInputBelowSignedAmount() public {
        uint256 amountIn = SIGNED_USDC_IN / 2;
        deal(USDC_ADDR, address(router), amountIn);
        uint256 fallbackOut = _quoteFallback(amountIn);

        _expectFallbackSwap(amountIn);
        _swap(amountIn, _signedQuote());

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), fallbackOut);
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
    }

    /// An expired quote reverts in the settlement, so the fallback fills the whole leg.
    function testExpiredQuote() public {
        deal(USDC_ADDR, address(router), SIGNED_USDC_IN);
        // The quote is signed against the current timestamp, so it is built before the warp.
        bytes memory quote = _signedQuote();
        vm.warp(block.timestamp + QUOTE_VALIDITY + 1);
        uint256 fallbackOut = _quoteFallback(SIGNED_USDC_IN);

        _expectFallbackSwap(SIGNED_USDC_IN);
        _swap(SIGNED_USDC_IN, quote);

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), fallbackOut);
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
    }

    /// The `settle` entrypoint fills like `settleSingle`.
    function testSettleQuote() public {
        deal(USDC_ADDR, address(router), SIGNED_USDC_IN);

        vm.recordLogs();
        _swap(
            SIGNED_USDC_IN,
            _encodeSettle(address(router), SIGNED_USDC_IN, SIGNED_WETH_OUT)
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), SIGNED_WETH_OUT);
        _assertNoFallbackSwap(address(router), vm.getRecordedLogs());
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
    }

    function testUnknownSelector() public {
        bytes4 transfer = IERC20.transfer.selector;
        vm.expectRevert(
            abi.encodeWithSelector(
                LiquoriceFallbackRouter__InvalidSelector.selector, transfer
            )
        );
        _swap(SIGNED_USDC_IN, abi.encodePacked(transfer));
    }

    function testCalldataWithoutSelector() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                LiquoriceFallbackRouter__InvalidDataLength.selector, 3
            )
        );
        _swap(SIGNED_USDC_IN, hex"9935c8");
    }

    function testConstructorZeroAddress() public {
        vm.expectRevert(LiquoriceFallbackRouter__AddressZero.selector);
        new LiquoriceFallbackRouter(
            IPoolManager(POOL_MANAGER),
            FLUIDV1_LIQUIDITY,
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER),
            LIQUORICE_SETTLEMENT,
            address(0)
        );
    }

    /// The swap data comes from the Rust encoder's
    /// `test_encode_liquorice_fallback_for_solidity`.
    function testExecutorSwapsRustEncodedData() public {
        deal(USDC_ADDR, address(router), SIGNED_USDC_IN);

        executor.swap(
            SIGNED_USDC_IN,
            loadCallDataFromFile("test_encode_liquorice_fallback_for_solidity"),
            BOB
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), SIGNED_WETH_OUT);
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
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
        assertEq(tokenIn, USDC_ADDR);
        assertEq(tokenOut, WETH_ADDR);
        assertFalse(outputToRouter);
    }

    /// The length prefix says the settlement calldata runs to the end, leaving no fallback.
    function testExecutorDataWithoutFallback() public {
        bytes memory settlementCalldata = _signedQuote();
        bytes memory data = abi.encodePacked(
            USDC_ADDR,
            WETH_ADDR,
            uint32(settlementCalldata.length),
            settlementCalldata
        );
        vm.expectRevert(
            abi.encodeWithSelector(
                FallbackExecutor__InvalidDataLength.selector, data.length
            )
        );
        executor.getTransferData(data);
    }

    function testExecutorMissingLengthPrefix() public {
        bytes memory data = abi.encodePacked(USDC_ADDR, WETH_ADDR);
        vm.expectRevert(
            abi.encodeWithSelector(
                FallbackExecutor__InvalidDataLength.selector, 40
            )
        );
        executor.getTransferData(data);
    }

    function _swap(uint256 amountIn, bytes memory settlementCalldata) internal {
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, amountIn, BOB),
            settlementCalldata,
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );
    }

    function _quoteFallback(uint256 amountIn) internal returns (uint256) {
        vm.prank(address(router));
        return router.quoteFallback(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, amountIn, BOB),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );
    }

    function _expectFallbackSwap(uint256 amountIn) internal {
        _expectFallbackSwap(
            address(router),
            LIQUORICE_SETTLEMENT,
            USDC_ADDR,
            WETH_ADDR,
            amountIn,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.PrimaryFailed
        );
    }

    function _executorData() internal view returns (bytes memory) {
        bytes memory settlementCalldata = _signedQuote();
        return abi.encodePacked(
            USDC_ADDR,
            WETH_ADDR,
            uint32(settlementCalldata.length),
            settlementCalldata,
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );
    }
}
