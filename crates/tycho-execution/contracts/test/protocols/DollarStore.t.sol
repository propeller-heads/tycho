pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    DollarStoreExecutor,
    DollarStoreExecutor__InvalidDataLength,
    DollarStoreExecutor__ZeroStoreAddress
} from "../../src/executors/DollarStoreExecutor.sol";
import {TransferManager} from "../../src/TransferManager.sol";

contract DollarStoreExecutorExposed is DollarStoreExecutor {
    constructor(address store_) DollarStoreExecutor(store_) {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (address tokenIn, address tokenOut)
    {
        return _decodeData(data);
    }
}

interface IDollarStoreView {
    error AssetNotListed(address asset);
    error InsufficientLiquidity(
        address wantAsset, uint256 fillableAmountOut, uint256 requestedAmountOut
    );

    function getSwapQuote(address offerAsset, address wantAsset, uint256 amount)
        external
        view
        returns (uint256);
}

// Hub: ~1.46M USDC, ~0.62M USDS. USDG spoke: ~1.93M USDG. Every feed fresh
// and on peg.
uint256 constant DOLLARSTORE_FORK_BLOCK = 26424000;

contract DollarStoreExecutorTest is TestUtils, Constants {
    DollarStoreExecutorExposed executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), DOLLARSTORE_FORK_BLOCK);
        executor = new DollarStoreExecutorExposed(DOLLARSTORE);
    }

    function testConstructorConfig() public view {
        assertEq(address(executor.store()), DOLLARSTORE);
    }

    function testConstructorRevertsOnZeroAddress() public {
        vm.expectRevert(DollarStoreExecutor__ZeroStoreAddress.selector);
        new DollarStoreExecutorExposed(address(0));
    }

    function testDecodeParams() public view {
        (address tokenIn, address tokenOut) =
            executor.decodeParams(abi.encodePacked(USDC_ADDR, USDG_ADDR));
        assertEq(tokenIn, USDC_ADDR);
        assertEq(tokenOut, USDG_ADDR);
    }

    function testDecodeParamsRejectsShortData() public {
        vm.expectRevert(DollarStoreExecutor__InvalidDataLength.selector);
        executor.decodeParams(abi.encodePacked(USDC_ADDR));
    }

    function testDecodeParamsRejectsLongData() public {
        vm.expectRevert(DollarStoreExecutor__InvalidDataLength.selector);
        executor.decodeParams(abi.encodePacked(USDC_ADDR, USDG_ADDR, uint8(0)));
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(abi.encodePacked(USDC_ADDR, USDG_ADDR));
        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, DOLLARSTORE);
        assertEq(tokenIn, USDC_ADDR);
        assertEq(tokenOut, USDG_ADDR);
        assertFalse(outputToRouter);
    }

    function testFundsExpectedAddress() public view {
        assertEq(
            executor.fundsExpectedAddress(
                abi.encodePacked(USDC_ADDR, USDG_ADDR)
            ),
            address(this)
        );
    }

    function testSwapUsdcToUsdg() public {
        uint256 amountIn = 10_000e6;
        _fund(USDC_ADDR, amountIn);
        executor.swap(amountIn, abi.encodePacked(USDC_ADDR, USDG_ADDR), BOB);
        assertEq(IERC20(USDG_ADDR).balanceOf(BOB), amountIn);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(executor)), 0);
    }

    function testSwapUsdsToUsdcLeavesSubUnitDust() public {
        uint256 amountIn = 10_000e18 + 123;
        _fund(USDS_ADDR, amountIn);
        uint256 before = IERC20(USDC_ADDR).balanceOf(BOB);
        executor.swap(amountIn, abi.encodePacked(USDS_ADDR, USDC_ADDR), BOB);
        assertEq(IERC20(USDC_ADDR).balanceOf(BOB) - before, 10_000e6);
        assertEq(IERC20(USDS_ADDR).balanceOf(address(executor)), 123);
    }

    function testSwapRevertsWhenNotFullyFillable() public {
        uint256 max = IDollarStoreView(DOLLARSTORE)
            .getSwapQuote(USDC_ADDR, USDG_ADDR, type(uint256).max);
        _fund(USDC_ADDR, max + 1);
        // swapExactInput never queues: it fills everything or reverts.
        vm.expectRevert(
            abi.encodeWithSelector(
                IDollarStoreView.InsufficientLiquidity.selector,
                USDG_ADDR,
                max,
                max + 1
            )
        );
        executor.swap(max + 1, abi.encodePacked(USDC_ADDR, USDG_ADDR), BOB);
    }

    function testSwapRevertsForUnlistedToken() public {
        _fund(WETH_ADDR, 1 ether);
        vm.expectRevert(
            abi.encodeWithSelector(
                IDollarStoreView.AssetNotListed.selector, WETH_ADDR
            )
        );
        executor.swap(1 ether, abi.encodePacked(WETH_ADDR, USDC_ADDR), BOB);
    }

    function testDecodeIntegration() public view {
        (address tokenIn, address tokenOut) = executor.decodeParams(
            loadCallDataFromFile("test_encode_dollarstore_usdc_usdg")
        );
        assertEq(tokenIn, USDC_ADDR);
        assertEq(tokenOut, USDG_ADDR);
    }

    /// Mirrors ProtocolWillDebit: the router holds the input and approves the
    /// store; here the executor plays the router.
    function _fund(address token, uint256 amount) internal {
        deal(token, address(executor), amount);
        vm.prank(address(executor));
        IERC20(token).approve(DOLLARSTORE, amount);
    }
}

contract DollarStoreRouterTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return DOLLARSTORE_FORK_BLOCK;
    }

    function testSingleSwapUsdcToUsdg() public {
        uint256 amountIn = 10_000e6;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_dollarstore_usdc_usdg"
        );
        deal(USDC_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(USDG_ADDR).balanceOf(ALICE), amountIn);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDG_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    /// An 18-decimal input that is not whole units: the store takes the whole
    /// units, the remainder stays at the router, and the Dispatcher revokes
    /// the unused allowance.
    function testSingleSwapUsdsToUsdcWithSubUnitInput() public {
        uint256 amountIn = 10_000e18 + 123;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_dollarstore_usds_usdc_sub_unit"
        );
        deal(USDS_ADDR, ALICE, amountIn);
        uint256 usdcBefore = IERC20(USDC_ADDR).balanceOf(ALICE);
        vm.startPrank(ALICE);
        IERC20(USDS_ADDR).approve(tychoRouterAddr, amountIn);
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE) - usdcBefore, 10_000e6);
        assertEq(IERC20(USDS_ADDR).balanceOf(tychoRouterAddr), 123);
        assertEq(IERC20(USDS_ADDR).allowance(tychoRouterAddr, DOLLARSTORE), 0);
    }

    /// USDS -> USDC on the hub, then USDC -> USDG on the spoke: the second
    /// hop is funded only by the first hop's output.
    function testSequentialSwapUsdsToUsdcToUsdg() public {
        uint256 amountIn = 10_000e18;
        bytes memory callData = loadCallDataFromFile(
            "test_sequential_encoding_strategy_dollarstore_usds_usdc_usdg"
        );
        deal(USDS_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(USDS_ADDR).approve(tychoRouterAddr, amountIn);
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(USDG_ADDR).balanceOf(ALICE), 10_000e6);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDG_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}
