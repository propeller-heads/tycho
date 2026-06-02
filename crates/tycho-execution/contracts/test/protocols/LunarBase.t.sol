pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {Constants} from "../Constants.sol";
import {TestUtils} from "../TestUtils.sol";
import {TychoRouterTestSetup} from "../TychoRouterTestSetup.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";
import {
    ILunarBasePool,
    LunarBaseExecutor,
    LunarBaseExecutor__InvalidDataLength,
    LunarBaseExecutor__MsgValueMismatch
} from "@src/executors/LunarBaseExecutor.sol";

contract LunarBaseExecutorExposed is LunarBaseExecutor {
    function decodeParams(bytes calldata data)
        external
        pure
        returns (
            address pool,
            address tokenIn,
            address tokenOut,
            uint256 amountOutMinimum
        )
    {
        return _decodeData(data);
    }
}

interface ILunarBaseQuoter {
    function isWhitelisted(address account) external view returns (bool);

    function quoteExactIn(address tokenIn, address tokenOut, uint256 amountIn)
        external
        view
        returns (uint256 amountOut);
}

contract LunarBaseExecutorTest is Constants, TestUtils {
    address internal constant LUNARBASE_POOL =
        0x0000eFC4ec03a7c47D3a38A9Be7Ff1d52dD01b99;
    address internal constant NATIVE_TOKEN =
        0x0000000000000000000000000000000000000000;
    bytes32 internal constant POOL_ACCESS_SLOT =
        0x9832e62c6c6e13b4465b385de9f563995a2974f4f839176dcccf0b12e5c11200;

    LunarBaseExecutorExposed lunarBaseExecutor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), 46498514);
        lunarBaseExecutor = new LunarBaseExecutorExposed();
        _whitelistSwapCaller(address(lunarBaseExecutor));
    }

    function testDecodeParams() public view {
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, ETH_ADDRESS, BASE_USDC);

        (
            address pool,
            address tokenIn,
            address tokenOut,
            uint256 amountOutMinimum
        ) = lunarBaseExecutor.decodeParams(params);

        assertEq(pool, LUNARBASE_POOL);
        assertEq(tokenIn, ETH_ADDRESS);
        assertEq(tokenOut, BASE_USDC);
        assertEq(amountOutMinimum, 0);
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory invalidParams =
            abi.encodePacked(LUNARBASE_POOL, NATIVE_TOKEN);

        vm.expectRevert(LunarBaseExecutor__InvalidDataLength.selector);
        lunarBaseExecutor.decodeParams(invalidParams);
    }

    function testGetTransferDataNativeInput() public {
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, ETH_ADDRESS, BASE_USDC);

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = lunarBaseExecutor.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.TransferNativeInExecutor)
        );
        assertEq(receiver, address(0));
        assertEq(tokenIn, ETH_ADDRESS);
        assertEq(tokenOut, BASE_USDC);
        assertEq(outputToRouter, false);
    }

    function testGetTransferDataErc20Input() public {
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, BASE_USDC, ETH_ADDRESS);

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = lunarBaseExecutor.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, LUNARBASE_POOL);
        assertEq(tokenIn, BASE_USDC);
        assertEq(tokenOut, ETH_ADDRESS);
        assertEq(outputToRouter, false);
    }

    function testFundsExpectedAddressNativeInput() public view {
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, ETH_ADDRESS, BASE_USDC);

        assertEq(lunarBaseExecutor.fundsExpectedAddress(params), address(this));
    }

    function testFundsExpectedAddressErc20Input() public view {
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, BASE_USDC, ETH_ADDRESS);

        assertEq(lunarBaseExecutor.fundsExpectedAddress(params), address(this));
    }

    function testSwapNativeInputRevertsOnMsgValueMismatch() public {
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, ETH_ADDRESS, BASE_USDC);

        vm.expectRevert(LunarBaseExecutor__MsgValueMismatch.selector);
        lunarBaseExecutor.swap{value: 0.005 ether}(0.01 ether, params, BOB);
    }

    function testSwapNativeInput() public {
        uint256 amountIn = 0.01 ether;
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, ETH_ADDRESS, BASE_USDC);
        uint256 expectedAmountOut = _quoteAs(
            address(lunarBaseExecutor), NATIVE_TOKEN, BASE_USDC, amountIn
        );

        assertGt(expectedAmountOut, 0);
        vm.deal(address(this), amountIn);

        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(BOB);
        lunarBaseExecutor.swap{value: amountIn}(amountIn, params, BOB);
        uint256 balanceAfter = IERC20(BASE_USDC).balanceOf(BOB);

        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
    }

    function testSwapErc20Input() public {
        uint256 amountIn = 20e6;
        bytes memory params =
            abi.encodePacked(LUNARBASE_POOL, BASE_USDC, ETH_ADDRESS);
        uint256 expectedAmountOut = _quoteAs(
            address(lunarBaseExecutor), BASE_USDC, NATIVE_TOKEN, amountIn
        );

        assertGt(expectedAmountOut, 0);
        deal(BASE_USDC, address(lunarBaseExecutor), amountIn);
        vm.prank(address(lunarBaseExecutor));
        IERC20(BASE_USDC).approve(LUNARBASE_POOL, amountIn);

        uint256 balanceBefore = BOB.balance;
        lunarBaseExecutor.swap(amountIn, params, BOB);
        uint256 balanceAfter = BOB.balance;

        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
        assertEq(IERC20(BASE_USDC).balanceOf(address(lunarBaseExecutor)), 0);
    }

    function _quoteAs(
        address caller,
        address tokenIn,
        address tokenOut,
        uint256 amountIn
    ) internal returns (uint256 amountOut) {
        vm.prank(caller);
        amountOut = ILunarBaseQuoter(LUNARBASE_POOL)
            .quoteExactIn(tokenIn, tokenOut, amountIn);
    }

    function _whitelistSwapCaller(address caller) internal {
        vm.store(LUNARBASE_POOL, _whitelistSlot(caller), bytes32(uint256(1)));
        assertTrue(ILunarBaseQuoter(LUNARBASE_POOL).isWhitelisted(caller));
    }

    function _whitelistSlot(address account) internal pure returns (bytes32) {
        return keccak256(abi.encode(account, POOL_ACCESS_SLOT));
    }
}

contract TychoRouterForLunarBaseTest is TychoRouterTestSetup {
    address internal constant LUNARBASE_POOL =
        0x0000eFC4ec03a7c47D3a38A9Be7Ff1d52dD01b99;
    address internal constant NATIVE_TOKEN =
        0x0000000000000000000000000000000000000000;
    bytes32 internal constant POOL_ACCESS_SLOT =
        0x9832e62c6c6e13b4465b385de9f563995a2974f4f839176dcccf0b12e5c11200;

    LunarBaseExecutor lunarBaseExecutor;

    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 46242605;
    }

    function setUp() public override {
        super.setUp();

        lunarBaseExecutor = new LunarBaseExecutor();
        _whitelistSwapCaller(tychoRouterAddr);
        address[] memory executors = new address[](1);
        executors[0] = address(lunarBaseExecutor);

        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.warp(block.timestamp + tychoRouter.DELAY_EXECUTOR_ACTIVATION());
    }

    function testSingleSwapNativeInputThroughRouter() public {
        uint256 amountIn = 4_142_411_222_470_969;
        uint256 expectedAmountOut =
            _quoteAs(tychoRouterAddr, NATIVE_TOKEN, BASE_USDC, amountIn);
        bytes memory swap = encodeSingleSwap(
            address(lunarBaseExecutor),
            abi.encodePacked(LUNARBASE_POOL, ETH_ADDRESS, BASE_USDC)
        );

        vm.deal(BOB, amountIn);
        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(BOB);

        vm.prank(BOB);
        uint256 amountOut = tychoRouter.singleSwap{value: amountIn}(
            amountIn, ETH_ADDRESS, BASE_USDC, 1, BOB, noClientFee(), swap
        );

        uint256 balanceAfter = IERC20(BASE_USDC).balanceOf(BOB);
        assertEq(amountOut, expectedAmountOut);
        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
    }

    function testSingleSwapErc20InputThroughRouter() public {
        uint256 amountIn = 8_281_118;
        uint256 expectedAmountOut =
            _quoteAs(tychoRouterAddr, BASE_USDC, NATIVE_TOKEN, amountIn);
        bytes memory swap = encodeSingleSwap(
            address(lunarBaseExecutor),
            abi.encodePacked(LUNARBASE_POOL, BASE_USDC, ETH_ADDRESS)
        );

        deal(BASE_USDC, BOB, amountIn);
        uint256 balanceBefore = BOB.balance;

        vm.startPrank(BOB);
        IERC20(BASE_USDC).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn, BASE_USDC, ETH_ADDRESS, 1, BOB, noClientFee(), swap
        );
        vm.stopPrank();

        uint256 balanceAfter = BOB.balance;
        assertEq(amountOut, expectedAmountOut);
        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
        assertEq(IERC20(BASE_USDC).balanceOf(tychoRouterAddr), 0);
    }

    function _quoteAs(
        address caller,
        address tokenIn,
        address tokenOut,
        uint256 amountIn
    ) internal returns (uint256 amountOut) {
        vm.prank(caller);
        amountOut = ILunarBaseQuoter(LUNARBASE_POOL)
            .quoteExactIn(tokenIn, tokenOut, amountIn);
    }

    function _whitelistSwapCaller(address caller) internal {
        vm.store(LUNARBASE_POOL, _whitelistSlot(caller), bytes32(uint256(1)));
        assertTrue(ILunarBaseQuoter(LUNARBASE_POOL).isWhitelisted(caller));
    }

    function _whitelistSlot(address account) internal pure returns (bytes32) {
        return keccak256(abi.encode(account, POOL_ACCESS_SLOT));
    }
}
