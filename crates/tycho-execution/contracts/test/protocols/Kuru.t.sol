pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {Constants} from "../Constants.sol";
import {TestUtils} from "../TestUtils.sol";
import {TychoRouterTestSetup} from "../TychoRouterTestSetup.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";
import {
    IKuruOrderBook,
    KuruExecutor,
    KuruExecutor__InvalidDataLength,
    KuruExecutor__TokenNotInMarket
} from "@src/executors/KuruExecutor.sol";

contract KuruExecutorExposed is KuruExecutor {
    function decodeParams(bytes calldata data)
        external
        pure
        returns (address market, address tokenIn, address tokenOut)
    {
        return _decodeData(data);
    }
}

/// MON/USDC market: base native MON (18 decimals), quote USDC (6 decimals),
/// pricePrecision 1e8, sizePrecision 1e10.
abstract contract KuruTestBase is Constants, TestUtils {
    address internal constant KURU_MON_USDC =
        0x065C9d28E428A0db40191a54d33d5b7c71a9C394;
    address internal constant MONAD_USDC =
        0x754704Bc059F8C67012fEd69BC8A327a5aafb603;
    uint256 internal constant FORK_BLOCK = 109040000;

    /// What the market credits for a market order at the current state. The
    /// market skips funds handling and returns the credited amount when
    /// called from address(0); the state is rolled back afterwards.
    function _quote(bool buy, uint96 size) internal returns (uint256 out) {
        uint256 snapshot = vm.snapshotState();
        vm.prank(address(0));
        out = buy
            ? IKuruOrderBook(KURU_MON_USDC)
                .placeAndExecuteMarketBuy(size, 0, false, true)
            : IKuruOrderBook(KURU_MON_USDC)
                .placeAndExecuteMarketSell(size, 0, false, true);
        vm.revertToState(snapshot);
    }
}

contract KuruExecutorTest is KuruTestBase {
    KuruExecutorExposed kuruExecutor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("monad"), FORK_BLOCK);
        kuruExecutor = new KuruExecutorExposed();
    }

    function testDecodeParams() public view {
        (address market, address tokenIn, address tokenOut) = kuruExecutor.decodeParams(
            abi.encodePacked(KURU_MON_USDC, ETH_ADDRESS, MONAD_USDC)
        );

        assertEq(market, KURU_MON_USDC);
        assertEq(tokenIn, ETH_ADDRESS);
        assertEq(tokenOut, MONAD_USDC);
    }

    function testDecodeParamsInvalidDataLength() public {
        vm.expectRevert(KuruExecutor__InvalidDataLength.selector);
        kuruExecutor.decodeParams(abi.encodePacked(KURU_MON_USDC, ETH_ADDRESS));
    }

    function testGetTransferDataNativeInput() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = kuruExecutor.getTransferData(
            abi.encodePacked(KURU_MON_USDC, ETH_ADDRESS, MONAD_USDC)
        );

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.TransferNativeInExecutor)
        );
        assertEq(receiver, address(0));
        assertEq(tokenIn, ETH_ADDRESS);
        assertEq(tokenOut, MONAD_USDC);
        assertTrue(outputToRouter);
    }

    function testGetTransferDataErc20Input() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = kuruExecutor.getTransferData(
            abi.encodePacked(KURU_MON_USDC, MONAD_USDC, ETH_ADDRESS)
        );

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, KURU_MON_USDC);
        assertEq(tokenIn, MONAD_USDC);
        assertEq(tokenOut, ETH_ADDRESS);
        assertTrue(outputToRouter);
    }

    function testFundsExpectedAddress() public view {
        assertEq(
            kuruExecutor.fundsExpectedAddress(
                abi.encodePacked(KURU_MON_USDC, MONAD_USDC, ETH_ADDRESS)
            ),
            address(this)
        );
    }

    /// Sells native MON: the executor sends the precision-rounded amount as
    /// msg.value and receives USDC, as the router does under delegatecall.
    function testSwapNativeInput() public {
        uint256 amountIn = 1000 ether + 123;
        uint256 expected = _quote(false, uint96(amountIn / 1e8));
        assertGt(expected, 0);
        vm.deal(address(kuruExecutor), amountIn);

        kuruExecutor.swap(
            amountIn,
            abi.encodePacked(KURU_MON_USDC, ETH_ADDRESS, MONAD_USDC),
            BOB
        );

        assertEq(IERC20(MONAD_USDC).balanceOf(address(kuruExecutor)), expected);
        // Input below one size unit stays with the caller.
        assertEq(address(kuruExecutor).balance, 123);
    }

    function testSwapRevertsForTokenNotInMarket() public {
        vm.expectRevert(KuruExecutor__TokenNotInMarket.selector);
        kuruExecutor.swap(
            1e18,
            abi.encodePacked(KURU_MON_USDC, address(0xdead), MONAD_USDC),
            BOB
        );
    }

    function testSwapErc20InputRevertsWhenBookCannotFill() public {
        uint256 amountIn = 1e15; // 1B USDC, far above the resting asks
        deal(MONAD_USDC, address(kuruExecutor), amountIn);
        vm.prank(address(kuruExecutor));
        IERC20(MONAD_USDC).approve(KURU_MON_USDC, amountIn);

        vm.expectRevert();
        kuruExecutor.swap(
            amountIn,
            abi.encodePacked(KURU_MON_USDC, MONAD_USDC, ETH_ADDRESS),
            BOB
        );
    }
}

contract TychoRouterForKuruTest is TychoRouterTestSetup, KuruTestBase {
    KuruExecutor kuruExecutor;

    function getChain() public pure override returns (string memory) {
        return "monad";
    }

    function getForkBlock() public pure override returns (uint256) {
        return FORK_BLOCK;
    }

    function setUp() public override {
        super.setUp();

        kuruExecutor = new KuruExecutor();
        address[] memory executors = new address[](1);
        executors[0] = address(kuruExecutor);

        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.warp(block.timestamp + tychoRouter.DELAY_EXECUTOR_ACTIVATION());
    }

    function testSingleSwapNativeInput() public {
        uint256 amountIn = 1000 ether;
        uint256 expected = _quote(false, uint96(amountIn / 1e8));
        bytes memory swap = encodeSingleSwap(
            address(kuruExecutor),
            abi.encodePacked(KURU_MON_USDC, ETH_ADDRESS, MONAD_USDC)
        );

        vm.deal(BOB, amountIn);
        uint256 balanceBefore = IERC20(MONAD_USDC).balanceOf(BOB);

        vm.prank(BOB);
        uint256 amountOut = tychoRouter.singleSwap{value: amountIn}(
            amountIn, ETH_ADDRESS, MONAD_USDC, 1, 1, BOB, noClientFee(), swap
        );

        assertEq(amountOut, expected);
        assertEq(IERC20(MONAD_USDC).balanceOf(BOB) - balanceBefore, expected);
        assertEq(tychoRouterAddr.balance, 0);
    }

    function testSingleSwapErc20Input() public {
        uint256 amountIn = 50e6;
        uint256 expected = _quote(true, uint96(amountIn * 100));
        bytes memory swap = encodeSingleSwap(
            address(kuruExecutor),
            abi.encodePacked(KURU_MON_USDC, MONAD_USDC, ETH_ADDRESS)
        );

        deal(MONAD_USDC, BOB, amountIn);
        uint256 balanceBefore = BOB.balance;

        vm.startPrank(BOB);
        IERC20(MONAD_USDC).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn, MONAD_USDC, ETH_ADDRESS, 1, 1, BOB, noClientFee(), swap
        );
        vm.stopPrank();

        assertEq(amountOut, expected);
        assertEq(BOB.balance - balanceBefore, expected);
        assertEq(IERC20(MONAD_USDC).balanceOf(tychoRouterAddr), 0);
        assertEq(
            IERC20(MONAD_USDC).allowance(tychoRouterAddr, KURU_MON_USDC), 0
        );
    }
}
