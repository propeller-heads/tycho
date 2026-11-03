// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {TestUtils} from "../TestUtils.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {FallbackRouterAssertions, FallbackSwaps} from "./Fallback.t.sol";
import {
    FallbackExecutor__InvalidDataLength
} from "../../src/executors/FallbackExecutor.sol";
import {
    HashflowFallbackExecutor
} from "../../src/executors/HashflowFallbackExecutor.sol";
import {TychoFallbackRouter} from "../../src/fallback/TychoFallbackRouter.sol";
import {
    HashflowFallbackRouter,
    HashflowFallbackRouter__AddressZero,
    HashflowFallbackRouter__InvalidDataLength
} from "../../src/fallback/HashflowFallbackRouter.sol";

/// @dev Uses `HashflowExecutorECR20Test`'s signed quote, whose trader is `ALICE`.
contract HashflowFallbackRouterTest is FallbackRouterAssertions, TestUtils {
    uint256 constant FORK_BLOCK = 23_188_416;
    address constant HASHFLOW_POOL = 0x5d8853028fbF6a2da43c7A828cc5f691E9456B44;
    uint256 constant SIGNED_WETH_IN = 1 ether;
    uint256 constant SIGNED_USDC_OUT = 4_286_117_034;
    uint256 constant QUOTE_EXPIRY = 1_755_766_775;

    HashflowFallbackRouter router;
    HashflowFallbackExecutor executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);
        deployCodeTo(
            "HashflowFallbackRouter.sol:HashflowFallbackRouter",
            abi.encode(
                IPoolManager(POOL_MANAGER),
                FLUIDV1_LIQUIDITY,
                IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER),
                HASHFLOW_ROUTER
            ),
            ALICE
        );
        router = HashflowFallbackRouter(ALICE);
        executor = new HashflowFallbackExecutor(address(router));
    }

    /// The signed price needs no comparison, so the fallback pool is never quoted.
    function testHashflowFills() public {
        deal(WETH_ADDR, address(router), SIGNED_WETH_IN);

        vm.expectCall(
            UNISWAP_V3_STATIC_QUOTER,
            abi.encodeWithSelector(IUniswapV3StaticQuoter.quote.selector),
            uint64(0)
        );
        vm.recordLogs();
        _swap(SIGNED_WETH_IN, _quote());

        assertEq(IERC20(USDC_ADDR).balanceOf(BOB), SIGNED_USDC_OUT);
        _assertNoFallbackSwap(address(router), vm.getRecordedLogs());
        _assertRouterDrained(address(router), WETH_ADDR, USDC_ADDR);
        assertEq(
            IERC20(WETH_ADDR).allowance(address(router), HASHFLOW_ROUTER), 0
        );
    }

    /// Half the signed input fills half the signed output.
    function testHashflowPartialFill() public {
        deal(WETH_ADDR, address(router), SIGNED_WETH_IN / 2);

        _swap(SIGNED_WETH_IN / 2, _quote());

        assertEq(IERC20(USDC_ADDR).balanceOf(BOB), SIGNED_USDC_OUT / 2);
        _assertRouterDrained(address(router), WETH_ADDR, USDC_ADDR);
    }

    /// The quote fills its signed amount and the caller gets the rest of the input back.
    function testInputAboveSignedAmount() public {
        uint256 remainder = 0.1 ether;
        deal(WETH_ADDR, address(router), SIGNED_WETH_IN + remainder);
        uint256 callerWethBefore = IERC20(WETH_ADDR).balanceOf(address(this));

        _swap(SIGNED_WETH_IN + remainder, _quote());

        assertEq(IERC20(USDC_ADDR).balanceOf(BOB), SIGNED_USDC_OUT);
        assertEq(
            IERC20(WETH_ADDR).balanceOf(address(this)) - callerWethBefore,
            remainder
        );
        _assertRouterDrained(address(router), WETH_ADDR, USDC_ADDR);
    }

    /// An expired quote reverts in the Hashflow router, so the fallback fills the whole leg.
    function testExpiredQuoteFallsBack() public {
        deal(WETH_ADDR, address(router), SIGNED_WETH_IN);
        vm.warp(QUOTE_EXPIRY + 1);
        vm.prank(address(router));
        uint256 fallbackOut = router.quoteFallback(
            FallbackSwaps.swap(WETH_ADDR, USDC_ADDR, SIGNED_WETH_IN, BOB),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );

        _expectFallbackSwap(
            address(router),
            HASHFLOW_POOL,
            WETH_ADDR,
            USDC_ADDR,
            SIGNED_WETH_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.PrimaryFailed
        );
        _swap(SIGNED_WETH_IN, _quote());

        assertEq(IERC20(USDC_ADDR).balanceOf(BOB), fallbackOut);
        _assertRouterDrained(address(router), WETH_ADDR, USDC_ADDR);
    }

    function testRejectsMalformedQuote() public {
        bytes memory quote = _quote();
        bytes memory short = new bytes(quote.length - 1);
        vm.expectRevert(
            abi.encodeWithSelector(
                HashflowFallbackRouter__InvalidDataLength.selector, short.length
            )
        );
        _swap(SIGNED_WETH_IN, short);
    }

    function testConstructorRejectsZeroAddress() public {
        vm.expectRevert(HashflowFallbackRouter__AddressZero.selector);
        new HashflowFallbackRouter(
            IPoolManager(POOL_MANAGER),
            FLUIDV1_LIQUIDITY,
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER),
            address(0)
        );
    }

    /// The swap data comes from the Rust encoder's
    /// `test_encode_hashflow_fallback_for_solidity`.
    function testExecutorSwapsRustEncodedData() public {
        deal(WETH_ADDR, address(router), SIGNED_WETH_IN);

        executor.swap(
            SIGNED_WETH_IN,
            loadCallDataFromFile("test_encode_hashflow_fallback_for_solidity"),
            BOB
        );

        assertEq(IERC20(USDC_ADDR).balanceOf(BOB), SIGNED_USDC_OUT);
        _assertRouterDrained(address(router), WETH_ADDR, USDC_ADDR);
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
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
        assertFalse(outputToRouter);
    }

    /// 385 bytes carry the tokens and the quote but no fallback.
    function testExecutorRejectsDataWithoutFallback() public {
        bytes memory data = abi.encodePacked(WETH_ADDR, USDC_ADDR, _quote());
        vm.expectRevert(
            abi.encodeWithSelector(
                FallbackExecutor__InvalidDataLength.selector, 385
            )
        );
        executor.getTransferData(data);
    }

    function _swap(uint256 amountIn, bytes memory quote) internal {
        router.swap(
            FallbackSwaps.swap(WETH_ADDR, USDC_ADDR, amountIn, BOB),
            quote,
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );
    }

    function _executorData() internal view returns (bytes memory) {
        return abi.encodePacked(
            WETH_ADDR,
            USDC_ADDR,
            _quote(),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );
    }

    function _quote() internal view returns (bytes memory) {
        return abi.encodePacked(
            HASHFLOW_POOL,
            address(0x9bA0CF1588E1DFA905eC948F7FE5104dD40EDa31),
            ALICE,
            ALICE,
            WETH_ADDR,
            USDC_ADDR,
            SIGNED_WETH_IN,
            SIGNED_USDC_OUT,
            QUOTE_EXPIRY,
            uint256(1_755_766_744_988),
            bytes32(
                0x12500006400064000186078c183380ffffffffffffff00296d737ff6ae950000
            ),
            hex"649d31cd74f1b11b4a3b32bd38c2525d78ce8f23bc2eaf7700899c3a396d3a137c861737dc780fa154699eafb3108a34cbb2d4e31a6f0623c169cc19e0fa296a1c"
        );
    }
}
