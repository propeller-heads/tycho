// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {TestUtils} from "../TestUtils.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {FallbackRouterAssertions, FallbackSwaps} from "./Fallback.t.sol";
import {
    ILiquoriceSettlement,
    IAllowListAuthentication
} from "./Liquorice.t.sol";
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
contract LiquoriceFallbackRouterTest is FallbackRouterAssertions, TestUtils {
    uint256 constant FORK_BLOCK = 24_392_845;
    address constant TRADER = 0x5615dEB798BB3E4dFa0139dFa1b3D433Cc23b72f;
    address constant AUTH_MANAGER = 0x000438801500c89E225E8D6CB69D9c14dD05e000;
    address constant MAKER = 0x06465bcEEaef280Bb7340A58D75dfc5E1F687058;
    uint256 constant QUOTE_EXPIRY = 0x69850367;
    uint256 constant SIGNED_USDC_IN = 3000e6;
    uint256 constant SIGNED_WETH_OUT = 1 ether;
    bytes constant SETTLE_SINGLE_CALLDATA =
        hex"9935c86800000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f68705800000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000024000000000000000000000000000000000000000000000000000000000b2d05e000000000000000000000000000000000000000000000000000000000000000320000000000000000000000000000000000000000000000000000000000000016000000000000000000000000000000000000000000000000000000000000000010000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc200000000000000000000000000000000000000000000000000000000b2d05e000000000000000000000000000000000000000000000000000de0b6b3a76400000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000006985036700000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f687058000000000000000000000000000000000000000000000000000000000000000131000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000041883a6506193307eebda0f3adf2cb81f84a073e030749055ebb18cbf98704eef100a03c307266527d706f9a5c3e08ed0988f5b130bc5327e0ad62dde6f3709d251b0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000";

    LiquoriceFallbackRouter router;
    LiquoriceFallbackExecutor executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);
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

        IAllowListAuthentication authenticator = IAllowListAuthentication(
            ILiquoriceSettlement(LIQUORICE_SETTLEMENT).AUTHENTICATOR()
        );
        vm.startPrank(AUTH_MANAGER);
        authenticator.addSolver(address(router));
        authenticator.addMaker(MAKER);
        vm.stopPrank();

        deal(WETH_ADDR, MAKER, SIGNED_WETH_OUT);
        vm.prank(MAKER);
        IERC20(WETH_ADDR).approve(LIQUORICE_BALANCE_MANAGER, type(uint256).max);
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
        _swap(SIGNED_USDC_IN, SETTLE_SINGLE_CALLDATA);

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

        _swap(SIGNED_USDC_IN + remainder, SETTLE_SINGLE_CALLDATA);

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
        _swap(amountIn, SETTLE_SINGLE_CALLDATA);

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), fallbackOut);
        _assertRouterDrained(address(router), USDC_ADDR, WETH_ADDR);
    }

    /// An expired quote reverts in the settlement, so the fallback fills the whole leg.
    function testExpiredQuote() public {
        deal(USDC_ADDR, address(router), SIGNED_USDC_IN);
        vm.warp(QUOTE_EXPIRY + 1);
        uint256 fallbackOut = _quoteFallback(SIGNED_USDC_IN);

        _expectFallbackSwap(SIGNED_USDC_IN);
        _swap(SIGNED_USDC_IN, SETTLE_SINGLE_CALLDATA);

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), fallbackOut);
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
        bytes memory settlementCalldata = SETTLE_SINGLE_CALLDATA;
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
        bytes memory settlementCalldata = SETTLE_SINGLE_CALLDATA;
        return abi.encodePacked(
            USDC_ADDR,
            WETH_ADDR,
            uint32(settlementCalldata.length),
            settlementCalldata,
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );
    }
}
