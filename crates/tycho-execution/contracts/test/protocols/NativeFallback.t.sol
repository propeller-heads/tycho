// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {TestUtils} from "../TestUtils.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {FallbackRouterAssertions, FallbackSwaps} from "./Fallback.t.sol";
import {BebopToken, MockBebopTarget} from "./BebopFallback.t.sol";
import {
    FallbackExecutor__InvalidDataLength
} from "../../src/executors/FallbackExecutor.sol";
import {
    NativeFallbackExecutor
} from "../../src/executors/NativeFallbackExecutor.sol";
import {TychoFallbackRouter} from "../../src/fallback/TychoFallbackRouter.sol";
import {
    NativeFallbackRouter,
    NativeFallbackRouter__AddressZero,
    NativeFallbackRouter__InvalidAmountIn,
    NativeFallbackRouter__InvalidDataLength,
    NativeFallbackRouter__InvalidSelector,
    NativeFallbackRouter__UnexpectedOverride
} from "../../src/fallback/NativeFallbackRouter.sol";

/// @dev Uses `NativeExecutorForkTest`'s recorded quote, whose recipient is `ALICE`.
contract NativeFallbackRouterTest is FallbackRouterAssertions, TestUtils {
    uint256 constant FORK_BLOCK = 25_930_213;
    uint256 constant QUOTE_TIMESTAMP = 1_788_841_206;
    uint256 constant QUOTE_DEADLINE = 0x6a9f8d3c;
    uint256 constant SIGNED_USDC_IN = 1_000_000;
    uint256 constant SIGNED_MINIMUM_WETH_OUT = 402_408_818_842_972;
    uint256 constant ACTUAL_SELLER_AMOUNT_OFFSET = 4 + 32;
    bytes constant TRADE_RFQT_CALLDATA =
        hex"7083527c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ef435b99c2108d0309211d0cc05b70d202c0b5df000000000000000000000000129b3d9a0a6e4beab88f5cb1e57995d72a6e24f1000000000000000000000000cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc200000000000000000000000000000000000000000000000000000000000f424000000000000000000000000000000000000000000000000000016dfd1ab39d5c00000000000000000000000000000000000000000000000000016dfd1ab39d5c000000000000000000000000000000000000000000000000000000006a9f8d3c00000000000000000000000000000000000000000000000035a49220053ede33000000000000000000000000000000000000000000000000000000006a9f8cf5000000000000000000000000000000000000000000000000000000006a9f8d1d00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ec6e372d2326451e8711e06e0d8b78f9000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000003600000000000000000000000006044eef7179034319e2c8636ea885b37cbfa9aba000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003e00000000000000000000000000000000000000000000000000000000000000041dc6662535476055fdc70cbf26235a36548260584b63670d587fff026324eacff3a91e0b6cb791bd8059d8701ad7065a8944385d96b4c6dd6910d74f9d06364a91c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000041fbb4628ced3cdc79524c223e42adae7c302475dbac6ac3eb119293a41c3f3cf5482d18a6fa8c2bcdc07ae6551068dfa1dba9cff189cc56f8bc6c26f8783c8d261c00000000000000000000000000000000000000000000000000000000000000";

    NativeFallbackRouter router;
    NativeFallbackExecutor executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);
        vm.warp(QUOTE_TIMESTAMP);
        deployCodeTo(
            "NativeFallbackRouter.sol:NativeFallbackRouter",
            abi.encode(
                IPoolManager(POOL_MANAGER),
                FLUIDV1_LIQUIDITY,
                IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER),
                NATIVE_ROUTER_V6_ETHEREUM
            ),
            ALICE
        );
        router = NativeFallbackRouter(ALICE);
        executor = new NativeFallbackExecutor(address(router));
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
        _swap(SIGNED_USDC_IN, _nativeData());

        assertGe(IERC20(WETH_ADDR).balanceOf(BOB), SIGNED_MINIMUM_WETH_OUT);
        _assertNoFallbackSwap(address(router), vm.getRecordedLogs());
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
        assertEq(
            IERC20(USDC_ADDR)
                .allowance(address(router), NATIVE_ROUTER_V6_ETHEREUM),
            0
        );
    }

    /// Native scales the signed minimum output to an input below the signed amount.
    function testInputBelowSignedAmount() public {
        uint256 amountIn = SIGNED_USDC_IN * 9 / 10;
        deal(USDC_ADDR, address(router), amountIn);

        vm.recordLogs();
        _swap(amountIn, _nativeData());

        assertGe(
            IERC20(WETH_ADDR).balanceOf(BOB),
            SIGNED_MINIMUM_WETH_OUT * amountIn / SIGNED_USDC_IN
        );
        _assertNoFallbackSwap(address(router), vm.getRecordedLogs());
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
    }

    /// Native rejects an input 10% above the signed amount, so the fallback fills the whole leg.
    function testInputAboveDeviationLimit() public {
        uint256 amountIn = SIGNED_USDC_IN * 11 / 10;
        deal(USDC_ADDR, address(router), amountIn);
        uint256 fallbackOut = _quoteFallback(amountIn);

        _expectFallbackSwap(amountIn);
        _swap(amountIn, _nativeData());

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), fallbackOut);
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
    }

    /// An expired quote reverts in the Native router, so the fallback fills the whole leg.
    function testExpiredQuote() public {
        deal(USDC_ADDR, address(router), SIGNED_USDC_IN);
        vm.warp(QUOTE_DEADLINE + 1);
        uint256 fallbackOut = _quoteFallback(SIGNED_USDC_IN);

        _expectFallbackSwap(SIGNED_USDC_IN);
        _swap(SIGNED_USDC_IN, _nativeData());

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), fallbackOut);
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
    }

    /// Uses Native's V4 tradeRFQT selector in place of the V6 one.
    function testV4TradeRfqtSelector() public {
        bytes memory nativeData = _nativeData();
        bytes4 v4TradeRfqt = 0x0947c2d9;
        _writeBytes4(nativeData, 32, v4TradeRfqt);
        vm.expectRevert(
            abi.encodeWithSelector(
                NativeFallbackRouter__InvalidSelector.selector, v4TradeRfqt
            )
        );
        _swap(SIGNED_USDC_IN, nativeData);
    }

    function testPresetSellerAmount() public {
        bytes memory nativeData = _nativeData();
        _writeWord(nativeData, 32 + ACTUAL_SELLER_AMOUNT_OFFSET, 1);
        vm.expectRevert(NativeFallbackRouter__UnexpectedOverride.selector);
        _swap(SIGNED_USDC_IN, nativeData);
    }

    function testZeroSignedAmount() public {
        vm.expectRevert(NativeFallbackRouter__InvalidAmountIn.selector);
        _swap(SIGNED_USDC_IN, abi.encodePacked(uint256(0), TRADE_RFQT_CALLDATA));
    }

    function testZeroAmountIn() public {
        vm.expectRevert(NativeFallbackRouter__InvalidAmountIn.selector);
        _swap(0, _nativeData());
    }

    function testShortNativeData() public {
        bytes memory noOverrides = abi.encodePacked(
            SIGNED_USDC_IN, bytes4(0x7083527c), bytes32(uint256(0x60))
        );
        vm.expectRevert(
            abi.encodeWithSelector(
                NativeFallbackRouter__InvalidDataLength.selector,
                noOverrides.length
            )
        );
        _swap(SIGNED_USDC_IN, noOverrides);
    }

    function testConstructorZeroAddress() public {
        vm.expectRevert(NativeFallbackRouter__AddressZero.selector);
        new NativeFallbackRouter(
            IPoolManager(POOL_MANAGER),
            FLUIDV1_LIQUIDITY,
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER),
            address(0)
        );
    }

    /// The swap data comes from the Rust encoder's
    /// `test_encode_native_fallback_for_solidity`.
    function testExecutorSwapsRustEncodedData() public {
        deal(USDC_ADDR, address(router), SIGNED_USDC_IN);

        executor.swap(
            SIGNED_USDC_IN,
            loadCallDataFromFile("test_encode_native_fallback_for_solidity"),
            BOB
        );

        assertGe(IERC20(WETH_ADDR).balanceOf(BOB), SIGNED_MINIMUM_WETH_OUT);
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

    /// The length prefix says the Native data runs to the end, leaving no fallback.
    function testExecutorDataWithoutFallback() public {
        bytes memory nativeData = _nativeData();
        bytes memory data = abi.encodePacked(
            USDC_ADDR, WETH_ADDR, uint32(nativeData.length), nativeData
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

    function _swap(uint256 amountIn, bytes memory nativeData) internal {
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, amountIn, BOB),
            nativeData,
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
            NATIVE_ROUTER_V6_ETHEREUM,
            USDC_ADDR,
            WETH_ADDR,
            amountIn,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.PrimaryFailed
        );
    }

    function _nativeData() internal pure returns (bytes memory) {
        return abi.encodePacked(SIGNED_USDC_IN, TRADE_RFQT_CALLDATA);
    }

    function _executorData() internal view returns (bytes memory) {
        bytes memory nativeData = _nativeData();
        return abi.encodePacked(
            USDC_ADDR,
            WETH_ADDR,
            uint32(nativeData.length),
            nativeData,
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );
    }

    function _writeWord(bytes memory data, uint256 offset, uint256 word)
        internal
        pure
    {
        assembly ("memory-safe") {
            mstore(add(add(data, 0x20), offset), word)
        }
    }

    function _writeBytes4(bytes memory data, uint256 offset, bytes4 value)
        internal
        pure
    {
        for (uint256 i = 0; i < 4; i++) {
            data[offset + i] = value[i];
        }
    }
}

/// @notice `NativeFallbackRouter` against a mock Native router. The fallback is never quoted or
/// run, so no fork is needed.
contract NativeFallbackRouterMockTest is FallbackRouterAssertions {
    uint256 constant AMOUNT_IN = 1000;

    BebopToken tokenIn;
    BebopToken tokenOut;
    MockBebopTarget nativeRouterV6;
    NativeFallbackRouter router;

    function setUp() public {
        tokenIn = new BebopToken();
        tokenOut = new BebopToken();
        nativeRouterV6 = new MockBebopTarget(tokenIn, tokenOut);
        router = new NativeFallbackRouter(
            IPoolManager(address(0)),
            address(0),
            IUniswapV3StaticQuoter(address(0)),
            address(nativeRouterV6)
        );
        tokenIn.mint(address(router), AMOUNT_IN);
    }

    /// Native keeps no allowance, and the caller gets the unpulled input back.
    function testNativePullsLessThanAmountIn() public {
        nativeRouterV6.setPullAmount(AMOUNT_IN / 2);

        router.swap(
            FallbackSwaps.swap(
                address(tokenIn), address(tokenOut), AMOUNT_IN, BOB
            ),
            abi.encodePacked(
                AMOUNT_IN,
                bytes4(0x7083527c),
                bytes32(uint256(0x60)),
                bytes32(0),
                bytes32(0)
            ),
            FallbackSwaps.uniswapV2(address(0xdead), 30)
        );

        assertEq(tokenOut.balanceOf(BOB), nativeRouterV6.AMOUNT_OUT());
        assertEq(tokenIn.allowance(address(router), address(nativeRouterV6)), 0);
        assertEq(tokenIn.balanceOf(address(this)), AMOUNT_IN / 2);
        _assertRouterDrained(
            address(router), address(tokenIn), address(tokenOut)
        );
    }
}
