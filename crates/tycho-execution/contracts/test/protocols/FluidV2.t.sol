// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {
    DexKey,
    FluidV2Executor,
    FluidV2Executor__InvalidCallback,
    FluidV2Executor__InvalidDataLength,
    FluidV2Executor__UnexpectedCallbackRecipient,
    FluidV2Executor__UnexpectedCallbackToken,
    IFluidDexV2Callbacks
} from "@src/executors/FluidV2Executor.sol";
import {Constants} from "../Constants.sol";
import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "forge-std/Test.sol";

uint256 constant FLUID_V2_FORK_BLOCK = 26331500;

/// @dev Called directly, the executor receives the dex callbacks itself and
/// performs the transfer the Dispatcher does on the router.
contract FluidV2ExecutorExposed is FluidV2Executor {
    using SafeERC20 for IERC20;

    constructor(address dexV2_, address liquidity_)
        FluidV2Executor(dexV2_, liquidity_)
    {}

    function decodeData(bytes calldata data)
        external
        pure
        returns (
            uint8 dexType,
            DexKey memory dexKey,
            bool swap0To1,
            bytes memory controllerData
        )
    {
        return _decodeData(data);
    }

    function startOperationCallback(bytes calldata)
        external
        returns (bytes memory)
    {
        return abi.decode(handleCallback(msg.data), (bytes));
    }

    function dexCallback(address token, address, uint256 amount) external {
        (, address receiver) =
            this.getCallbackTransferData(msg.data, token, msg.sender);
        IERC20(token).safeTransfer(receiver, amount);
        handleCallback(msg.data);
    }
}

abstract contract FluidV2Pools is Constants {
    uint24 internal constant STABLE_FEE = 100;
    uint24 internal constant STABLE_TICK_SPACING = 1;
    uint24 internal constant VOLATILE_FEE = 500;
    uint24 internal constant VOLATILE_TICK_SPACING = 10;

    function _swapData(
        uint8 dexType,
        address token0,
        address token1,
        uint24 fee,
        uint24 tickSpacing,
        bool swap0To1
    ) internal pure returns (bytes memory) {
        return abi.encodePacked(
            dexType, token0, token1, fee, tickSpacing, address(0), swap0To1
        );
    }

    function _usdcUsdt(uint8 dexType, bool swap0To1)
        internal
        view
        returns (bytes memory)
    {
        return _swapData(
            dexType,
            USDC_ADDR,
            USDT_ADDR,
            STABLE_FEE,
            STABLE_TICK_SPACING,
            swap0To1
        );
    }

    function _usdcEth(bool swap0To1) internal view returns (bytes memory) {
        return _swapData(
            3,
            USDC_ADDR,
            ETH_ADDR,
            VOLATILE_FEE,
            VOLATILE_TICK_SPACING,
            swap0To1
        );
    }
}

contract FluidV2ExecutorTest is Test, FluidV2Pools {
    FluidV2ExecutorExposed executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FLUID_V2_FORK_BLOCK);
        executor = new FluidV2ExecutorExposed(FLUID_DEX_V2, FLUIDV1_LIQUIDITY);
    }

    function testConstructorRejectsZeroAddress() public {
        vm.expectRevert();
        new FluidV2Executor(address(0), FLUIDV1_LIQUIDITY);
        vm.expectRevert();
        new FluidV2Executor(FLUID_DEX_V2, address(0));
    }

    function testDecodeData() public view {
        bytes memory data = abi.encodePacked(_usdcEth(false), hex"beef");

        (
            uint8 dexType,
            DexKey memory dexKey,
            bool swap0To1,
            bytes memory controllerData
        ) = executor.decodeData(data);

        assertEq(dexType, 3);
        assertEq(dexKey.token0, USDC_ADDR);
        assertEq(dexKey.token1, ETH_ADDR);
        assertEq(dexKey.fee, VOLATILE_FEE);
        assertEq(dexKey.tickSpacing, VOLATILE_TICK_SPACING);
        assertEq(dexKey.controller, address(0));
        assertFalse(swap0To1);
        assertEq(controllerData, hex"beef");
    }

    function testDecodeDataTooShort() public {
        vm.expectRevert(FluidV2Executor__InvalidDataLength.selector);
        executor.decodeData(new bytes(67));
    }

    function testGetTransferDataErc20() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(_usdcUsdt(4, false));

        assertEq(uint8(transferType), uint8(TransferManager.TransferType.None));
        assertEq(receiver, address(0));
        assertEq(tokenIn, USDT_ADDR);
        assertEq(tokenOut, USDC_ADDR);
        assertFalse(outputToRouter);
    }

    function testGetTransferDataNativeIn() public view {
        (TransferManager.TransferType transferType,, address tokenIn,,) =
            executor.getTransferData(_usdcEth(false));

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.TransferNativeInExecutor)
        );
        assertEq(tokenIn, ETH_ADDR);
    }

    function testGetCallbackTransferDataStartOperation() public view {
        bytes memory data = abi.encodeCall(
            IFluidDexV2Callbacks.startOperationCallback, (hex"")
        );

        (TransferManager.TransferType transferType, address receiver) =
            executor.getCallbackTransferData(data, USDC_ADDR, FLUID_DEX_V2);

        assertEq(uint8(transferType), uint8(TransferManager.TransferType.None));
        assertEq(receiver, address(0));
    }

    function testGetCallbackTransferDataPaysDexOrLiquidity() public view {
        address[2] memory recipients = [FLUID_DEX_V2, FLUIDV1_LIQUIDITY];
        for (uint256 i = 0; i < recipients.length; i++) {
            bytes memory data = abi.encodeCall(
                IFluidDexV2Callbacks.dexCallback,
                (USDC_ADDR, recipients[i], 1e6)
            );

            (TransferManager.TransferType transferType, address receiver) =
                executor.getCallbackTransferData(data, USDC_ADDR, FLUID_DEX_V2);

            assertEq(
                uint8(transferType),
                uint8(TransferManager.TransferType.Transfer)
            );
            assertEq(receiver, recipients[i]);
        }
    }

    function testGetCallbackTransferDataRejectsOtherRecipient() public {
        bytes memory data = abi.encodeCall(
            IFluidDexV2Callbacks.dexCallback, (USDC_ADDR, BOB, 1e6)
        );

        vm.expectRevert(
            abi.encodeWithSelector(
                FluidV2Executor__UnexpectedCallbackRecipient.selector, BOB
            )
        );
        executor.getCallbackTransferData(data, USDC_ADDR, FLUID_DEX_V2);
    }

    function testGetCallbackTransferDataRejectsOtherToken() public {
        bytes memory data = abi.encodeCall(
            IFluidDexV2Callbacks.dexCallback, (USDT_ADDR, FLUID_DEX_V2, 1e6)
        );

        vm.expectRevert(
            abi.encodeWithSelector(
                FluidV2Executor__UnexpectedCallbackToken.selector, USDT_ADDR
            )
        );
        executor.getCallbackTransferData(data, USDC_ADDR, FLUID_DEX_V2);
    }

    function testGetCallbackTransferDataRejectsOtherCaller() public {
        bytes memory data = abi.encodeCall(
            IFluidDexV2Callbacks.dexCallback, (USDC_ADDR, FLUID_DEX_V2, 1e6)
        );

        vm.expectRevert(FluidV2Executor__InvalidCallback.selector);
        executor.getCallbackTransferData(data, USDC_ADDR, BOB);
    }

    function testVerifyCallbackRejectsOtherSender() public {
        bytes memory data = abi.encodeCall(
            IFluidDexV2Callbacks.dexCallback, (USDC_ADDR, FLUID_DEX_V2, 1e6)
        );

        vm.expectRevert(FluidV2Executor__InvalidCallback.selector);
        executor.verifyCallback(data);
    }

    function testVerifyCallbackRejectsOtherSelector() public {
        vm.prank(FLUID_DEX_V2);
        vm.expectRevert(FluidV2Executor__InvalidCallback.selector);
        executor.verifyCallback(abi.encodePacked(bytes4(0x12345678)));
    }

    // Expected outputs are the swaps' results at FLUID_V2_FORK_BLOCK; D3 and D4
    // pools of the same tokens price differently, so each pins its dex type.
    function testSwapD3() public {
        _assertErc20Swap(
            _usdcUsdt(3, true), USDC_ADDR, USDT_ADDR, 10e6, 10_007_725
        );
    }

    function testSwapD3OneToZero() public {
        _assertErc20Swap(
            _usdcUsdt(3, false), USDT_ADDR, USDC_ADDR, 10e6, 9_990_168
        );
    }

    function testSwapD4() public {
        _assertErc20Swap(
            _usdcUsdt(4, true), USDC_ADDR, USDT_ADDR, 10e6, 9_997_172
        );
    }

    function testSwapD4OneToZero() public {
        _assertErc20Swap(
            _usdcUsdt(4, false), USDT_ADDR, USDC_ADDR, 10e6, 10_000_641
        );
    }

    function testSellNative() public {
        uint256 amountIn = 0.001 ether;
        deal(address(executor), amountIn);
        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(BOB);

        executor.swap(amountIn, _usdcEth(false), BOB);

        assertEq(IERC20(USDC_ADDR).balanceOf(BOB) - balanceBefore, 2_265_698);
        assertEq(address(executor).balance, 0);
    }

    function testBuyNative() public {
        uint256 amountIn = 10e6;
        deal(USDC_ADDR, address(executor), amountIn);
        uint256 balanceBefore = BOB.balance;

        executor.swap(amountIn, _usdcEth(true), BOB);

        assertEq(BOB.balance - balanceBefore, 4_408_600_502_206_338);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(executor)), 0);
    }

    function _assertErc20Swap(
        bytes memory data,
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 expectedOut
    ) internal {
        deal(tokenIn, address(executor), amountIn);
        uint256 balanceBefore = IERC20(tokenOut).balanceOf(BOB);

        executor.swap(amountIn, data, BOB);

        assertEq(IERC20(tokenOut).balanceOf(BOB) - balanceBefore, expectedOut);
        assertEq(IERC20(tokenIn).balanceOf(address(executor)), 0);
    }
}

contract TychoRouterForFluidV2Test is TychoRouterTestSetup, FluidV2Pools {
    using SafeERC20 for IERC20;

    function getForkBlock() public pure override returns (uint256) {
        return FLUID_V2_FORK_BLOCK;
    }

    function testSingleSwapD3() public {
        _assertRouterSwap(
            "test_single_encoding_strategy_fluid_v2_d3", 10e6, 10_007_725
        );
    }

    function testSingleSwapD4() public {
        _assertRouterSwap(
            "test_single_encoding_strategy_fluid_v2_d4", 10e6, 9_997_172
        );
    }

    function testSingleSwapNativeBuy() public {
        // By this block ALICE carries an EIP-7702 delegation that forwards any
        // ETH it receives, which would hide the payout from the router.
        vm.etch(ALICE, "");
        uint256 amountIn = 10e6;
        deal(USDC_ADDR, ALICE, amountIn);
        uint256 balanceBefore = ALICE.balance;
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).forceApprove(tychoRouterAddr, amountIn);

        (bool success, bytes memory result) = tychoRouterAddr.call(
            loadCallDataFromFile(
                "test_single_encoding_strategy_fluid_v2_native"
            )
        );

        assertTrue(success, "Call Failed");
        uint256 amountOut = abi.decode(result, (uint256));
        assertEq(amountOut, 4_408_600_502_206_338);
        assertEq(ALICE.balance - balanceBefore, amountOut);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(tychoRouterAddr.balance, 0);
    }

    function testSingleSwapNativeSell() public {
        uint256 amountIn = 0.001 ether;
        deal(ALICE, amountIn);
        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);
        bytes memory swap =
            encodeSingleSwap(address(fluidV2Executor), _usdcEth(false));

        vm.prank(ALICE);
        uint256 amountOut = tychoRouter.singleSwap{value: amountIn}(
            amountIn, ETH_ADDR, USDC_ADDR, 1, 1, ALICE, noClientFee(), swap
        );

        assertEq(amountOut, 2_265_698);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE) - balanceBefore, amountOut);
        assertEq(tychoRouterAddr.balance, 0);
    }

    function _assertRouterSwap(
        string memory testName,
        uint256 amountIn,
        uint256 expectedOut
    ) internal {
        deal(USDC_ADDR, ALICE, amountIn);
        uint256 balanceBefore = IERC20(USDT_ADDR).balanceOf(ALICE);
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).forceApprove(tychoRouterAddr, amountIn);

        (bool success, bytes memory result) =
            tychoRouterAddr.call(loadCallDataFromFile(testName));

        assertTrue(success, "Call Failed");
        uint256 amountOut = abi.decode(result, (uint256));
        assertEq(amountOut, expectedOut);
        assertEq(IERC20(USDT_ADDR).balanceOf(ALICE) - balanceBefore, amountOut);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDT_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}
