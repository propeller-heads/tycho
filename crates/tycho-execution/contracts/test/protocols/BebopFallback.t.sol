// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {Vm} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {Constants} from "../Constants.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {FallbackSwaps} from "./Fallback.t.sol";
import {
    FallbackExecutor__InvalidDataLength
} from "../../src/executors/FallbackExecutor.sol";
import {
    BebopFallbackExecutor
} from "../../src/executors/BebopFallbackExecutor.sol";
import {TychoFallbackRouter} from "../../src/fallback/TychoFallbackRouter.sol";
import {
    BebopFallbackRouter,
    BebopFallbackRouter__AddressZero,
    BebopFallbackRouter__InvalidDataLength,
    BebopFallbackRouter__InvalidSelector,
    BebopFallbackRouter__InvalidTarget
} from "../../src/fallback/BebopFallbackRouter.sol";

/// @dev Uses `BebopExecutorTest.testSingleOrder`'s signed order, whose taker is `TAKER`.
contract BebopFallbackRouterTest is Constants {
    uint256 constant FORK_BLOCK = 23_124_275;
    address constant TAKER = 0x5615dEB798BB3E4dFa0139dFa1b3D433Cc23b72f;
    /// The order's `expiry`.
    uint256 constant EXPIRY = 0x689b137a;
    uint8 constant PARTIAL_FILL_OFFSET = 12;
    uint256 constant SIGNED_WETH_IN = 1 ether;
    uint256 constant SIGNED_WBTC_OUT = 3_617_660;
    bytes constant SWAP_SINGLE_CALLDATA =
        hex"4dcebcba00000000000000000000000000000000000000000000000000000000689b137a0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f000000000000000000000000bee3211ab312a8d065c4fef0247448e17a8da000000000000000000000000000000000000000000000000000279ead5d9683d8a5000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc20000000000000000000000002260fac5e5542a773aa44fbcfedf7c193bc2c5990000000000000000000000000000000000000000000000000de0b6b3a7640000000000000000000000000000000000000000000000000000000000000037337c0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f0000000000000000000000000000000000000000000000000000000000000000f71248bc6c123bbf12adc837470f75640000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000418e9b0fb72ed9b86f7a7345026269c02b9056efcdfb67a377c7ff6c4a62a4807a7671ae759edf29aea1b2cb8efc8659e3aedac72943cd3607985a1849256358641c00000000000000000000000000000000000000000000000000000000000000";

    BebopFallbackRouter router;
    BebopFallbackExecutor executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);
        router = new BebopFallbackRouter(
            IPoolManager(POOL_MANAGER),
            FLUIDV1_LIQUIDITY,
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER),
            BEBOP_SETTLEMENT,
            BEBOP_ROUTER
        );
        // The test contract's first deployment lands on the order's taker.
        assertEq(address(router), TAKER);
        executor = new BebopFallbackExecutor(address(router));
    }

    function testBebopFills() public {
        deal(WETH_ADDR, address(router), SIGNED_WETH_IN);

        vm.recordLogs();
        _swap(SIGNED_WETH_IN, _bebopData());

        assertEq(IERC20(WBTC_ADDR).balanceOf(BOB), SIGNED_WBTC_OUT);
        _assertNoFallbackSwap(vm.getRecordedLogs());
        _assertRouterDrained();
        assertEq(
            IERC20(WETH_ADDR).allowance(address(router), BEBOP_SETTLEMENT), 0
        );
    }

    /// Half the signed input fills half the signed output.
    function testBebopPartialFill() public {
        deal(WETH_ADDR, address(router), SIGNED_WETH_IN / 2);

        _swap(SIGNED_WETH_IN / 2, _bebopData());

        assertEq(IERC20(WBTC_ADDR).balanceOf(BOB), SIGNED_WBTC_OUT / 2);
        _assertRouterDrained();
    }

    /// An expired order reverts in the settlement, so the fallback fills the whole leg.
    function testExpiredOrderFallsBack() public {
        deal(WETH_ADDR, address(router), SIGNED_WETH_IN);
        vm.warp(EXPIRY + 1);
        uint256 fallbackOut = _quoteFallback(SIGNED_WETH_IN);

        _expectFallbackSwap(
            SIGNED_WETH_IN, TychoFallbackRouter.FallbackReason.PrimaryFailed
        );
        _swap(SIGNED_WETH_IN, _bebopData());

        assertEq(IERC20(WBTC_ADDR).balanceOf(BOB), fallbackOut);
        _assertRouterDrained();
        assertEq(
            IERC20(WETH_ADDR).allowance(address(router), BEBOP_SETTLEMENT), 0
        );
    }

    /// The whole leg goes to the fallback and the order is not attempted.
    function testInputAboveSignedAmountFallsBack() public {
        uint256 amountIn = SIGNED_WETH_IN + 1;
        deal(WETH_ADDR, address(router), amountIn);
        uint256 fallbackOut = _quoteFallback(amountIn);

        _expectFallbackSwap(
            amountIn, TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        _swap(amountIn, _bebopData());

        assertEq(IERC20(WBTC_ADDR).balanceOf(BOB), fallbackOut);
        _assertRouterDrained();
    }

    function testRejectsUnknownTarget() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                BebopFallbackRouter__InvalidTarget.selector, address(0xdead)
            )
        );
        router.swap(
            FallbackSwaps.swap(WETH_ADDR, WBTC_ADDR, SIGNED_WETH_IN, BOB),
            address(0xdead),
            _bebopData(),
            FallbackSwaps.uniswapV2(WETH_WBTC_POOL, 30)
        );
    }

    /// The settlement accepts only its own swap selectors, not the router's.
    function testRejectsRouterSelectorOnSettlement() public {
        bytes4 routerSwap = 0x9586d0e8;
        vm.expectRevert(
            abi.encodeWithSelector(
                BebopFallbackRouter__InvalidSelector.selector, routerSwap
            )
        );
        _swap(
            SIGNED_WETH_IN,
            abi.encodePacked(PARTIAL_FILL_OFFSET, SIGNED_WETH_IN, routerSwap)
        );
    }

    function testRejectsShortBebopData() public {
        bytes memory noSelector =
            abi.encodePacked(PARTIAL_FILL_OFFSET, SIGNED_WETH_IN);
        vm.expectRevert(
            abi.encodeWithSelector(
                BebopFallbackRouter__InvalidDataLength.selector, 33
            )
        );
        _swap(SIGNED_WETH_IN, noSelector);
    }

    function testConstructorRejectsZeroAddress() public {
        vm.expectRevert(BebopFallbackRouter__AddressZero.selector);
        new BebopFallbackRouter(
            IPoolManager(POOL_MANAGER),
            FLUIDV1_LIQUIDITY,
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER),
            BEBOP_SETTLEMENT,
            address(0)
        );
    }

    function testExecutorSwap() public {
        deal(WETH_ADDR, address(router), SIGNED_WETH_IN);

        executor.swap(SIGNED_WETH_IN, _executorData(), BOB);

        assertEq(IERC20(WBTC_ADDR).balanceOf(BOB), SIGNED_WBTC_OUT);
        _assertRouterDrained();
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
        assertEq(tokenOut, WBTC_ADDR);
        assertFalse(outputToRouter);
    }

    /// The length prefix says the Bebop data runs to the end, leaving no fallback.
    function testExecutorRejectsDataWithoutFallback() public {
        bytes memory bebopData = _bebopData();
        bytes memory data = abi.encodePacked(
            WETH_ADDR,
            WBTC_ADDR,
            BEBOP_SETTLEMENT,
            uint32(bebopData.length),
            bebopData
        );
        vm.expectRevert(
            abi.encodeWithSelector(
                FallbackExecutor__InvalidDataLength.selector, data.length
            )
        );
        executor.getTransferData(data);
    }

    function testExecutorRejectsMissingLengthPrefix() public {
        bytes memory data =
            abi.encodePacked(WETH_ADDR, WBTC_ADDR, BEBOP_SETTLEMENT);
        vm.expectRevert(
            abi.encodeWithSelector(
                FallbackExecutor__InvalidDataLength.selector, 60
            )
        );
        executor.getTransferData(data);
    }

    function _swap(uint256 amountIn, bytes memory bebopData) internal {
        router.swap(
            FallbackSwaps.swap(WETH_ADDR, WBTC_ADDR, amountIn, BOB),
            BEBOP_SETTLEMENT,
            bebopData,
            FallbackSwaps.uniswapV2(WETH_WBTC_POOL, 30)
        );
    }

    function _quoteFallback(uint256 amountIn) internal returns (uint256) {
        vm.prank(address(router));
        return router.quoteFallback(
            FallbackSwaps.swap(WETH_ADDR, WBTC_ADDR, amountIn, BOB),
            FallbackSwaps.uniswapV2(WETH_WBTC_POOL, 30)
        );
    }

    function _bebopData() internal pure returns (bytes memory) {
        return abi.encodePacked(
            PARTIAL_FILL_OFFSET, SIGNED_WETH_IN, SWAP_SINGLE_CALLDATA
        );
    }

    function _executorData() internal view returns (bytes memory) {
        bytes memory bebopData = _bebopData();
        return abi.encodePacked(
            WETH_ADDR,
            WBTC_ADDR,
            BEBOP_SETTLEMENT,
            uint32(bebopData.length),
            bebopData,
            FallbackSwaps.uniswapV2(WETH_WBTC_POOL, 30)
        );
    }

    function _expectFallbackSwap(
        uint256 amountIn,
        TychoFallbackRouter.FallbackReason reason
    ) internal {
        vm.expectEmit(address(router));
        emit TychoFallbackRouter.FallbackSwap(
            BEBOP_SETTLEMENT,
            WETH_ADDR,
            WBTC_ADDR,
            amountIn,
            TychoFallbackRouter.FallbackProtocol.UniswapV2,
            reason
        );
    }

    function _assertNoFallbackSwap(Vm.Log[] memory logs) internal view {
        for (uint256 i = 0; i < logs.length; i++) {
            assertFalse(
                logs[i].emitter == address(router)
                    && logs[i].topics[0]
                        == TychoFallbackRouter.FallbackSwap.selector
            );
        }
    }

    function _assertRouterDrained() internal view {
        assertEq(IERC20(WETH_ADDR).balanceOf(address(router)), 0);
        assertEq(IERC20(WBTC_ADDR).balanceOf(address(router)), 0);
    }
}
