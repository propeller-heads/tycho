pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import "@permit2/src/interfaces/IAllowanceTransfer.sol";
import "@src/executors/UniswapV3Executor.sol";
import {Constants} from "../Constants.sol";
import {Permit2TestHelper} from "../Permit2TestHelper.sol";
import {Test} from "../../lib/forge-std/src/Test.sol";

contract UniswapV3ExecutorExposed is UniswapV3Executor {
    constructor() UniswapV3Executor() {}

    function decodeData(bytes calldata data)
        external
        pure
        returns (address target, bool zeroForOne)
    {
        return _decodeData(data);
    }

    function uniswapV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata /* data */
    ) external {
        // tokenIn is determined by which delta is positive (the token being sold).
        // Callback data no longer encodes tokenIn; the Dispatcher passes it directly.
        address tokenIn = amount0Delta > 0
            ? IUniswapV3Pool(msg.sender).token0()
            : IUniswapV3Pool(msg.sender).token1();

        (TransferManager.TransferType transferType, address receiver) =
            this.getCallbackTransferData(msg.data, tokenIn, msg.sender);
        assert(transferType == TransferManager.TransferType.Transfer);

        uint256 amount =
            amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);
        IERC20(tokenIn).transfer(receiver, amount);
        handleCallback(msg.data);
    }
}

contract UniswapV3ExecutorTest is Test, TestUtils, Constants {
    using SafeERC20 for IERC20;

    UniswapV3ExecutorExposed uniswapV3Exposed;
    UniswapV3ExecutorExposed pancakeV3Exposed;
    IERC20 DAI = IERC20(DAI_ADDR);

    function setUp() public {
        uint256 forkBlock = 17323404;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);

        uniswapV3Exposed = new UniswapV3ExecutorExposed();
        pancakeV3Exposed = new UniswapV3ExecutorExposed();
    }

    function testDecodeParams() public view {
        uint24 expectedPoolFee = 500;
        bytes memory data = abi.encodePacked(
            WETH_ADDR, DAI_ADDR, expectedPoolFee, address(3), false
        );

        (address target, bool zeroForOne) = uniswapV3Exposed.decodeData(data);

        assertEq(target, address(3));
        assertEq(zeroForOne, false);
    }

    function testGetTransferData() public {
        bytes memory params = "";

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = uniswapV3Exposed.getTransferData(params);

        assertEq(uint8(transferType), uint8(TransferManager.TransferType.None));
        assertEq(receiver, address(0));
        assertEq(tokenIn, address(0));
        assertEq(tokenOut, address(0));
        assertEq(outputToRouter, false);
    }

    function testGetCallbackTransferData() public {
        uint24 poolFee = 3000;
        uint256 amountOwed = 1000000000000000000;
        bytes memory protocolData = abi.encodePacked(
            WETH_ADDR, DAI_ADDR, poolFee, address(uniswapV3Exposed)
        );
        uint256 dataOffset = 3; // some offset
        uint256 dataLength = protocolData.length;

        bytes memory callbackData = abi.encodePacked(
            bytes4(0xfa461e33),
            int256(amountOwed), // amount0Delta
            int256(0), // amount1Delta
            dataOffset,
            dataLength,
            protocolData
        );
        (TransferManager.TransferType transferType, address receiver) = uniswapV3Exposed.getCallbackTransferData(
            callbackData, WETH_ADDR, address(this)
        );

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, address(this));
    }

    function testSwapIntegration() public {
        uint256 amountIn = 10 ** 18;
        deal(WETH_ADDR, address(uniswapV3Exposed), amountIn);

        uint256 expAmountOut = 1205_128428842122129186; //Swap 1 WETH for 1205.12 DAI
        bool zeroForOne = false;

        bytes memory data =
            encodeUniswapV3Swap(WETH_ADDR, DAI_ADDR, DAI_WETH_USV3, zeroForOne);

        uint256 balanceBefore = IERC20(DAI_ADDR).balanceOf(address(this));
        uniswapV3Exposed.swap(amountIn, data, address(this));
        uint256 amountOut =
            IERC20(DAI_ADDR).balanceOf(address(this)) - balanceBefore;

        assertGe(amountOut, expAmountOut);
        assertEq(IERC20(WETH_ADDR).balanceOf(address(uniswapV3Exposed)), 0);
        assertGe(IERC20(DAI_ADDR).balanceOf(address(this)), expAmountOut);
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory invalidParams = abi.encodePacked(
            WETH_ADDR,
            DAI_ADDR,
            uint24(500),
            TransferManager.TransferType.Transfer,
            address(3),
            false
        );

        vm.expectRevert(UniswapV3Executor__InvalidDataLength.selector);
        uniswapV3Exposed.decodeData(invalidParams);
    }

    function testUSV3Callback() public {
        uint24 poolFee = 3000;
        uint256 amountOwed = 1000000000000000000;
        deal(WETH_ADDR, address(uniswapV3Exposed), amountOwed);
        uint256 initialPoolReserve = IERC20(WETH_ADDR).balanceOf(DAI_WETH_USV3);

        bytes memory protocolData =
            abi.encodePacked(WETH_ADDR, DAI_ADDR, poolFee);
        uint256 dataOffset = 3; // some offset
        uint256 dataLength = protocolData.length;

        bytes memory callbackData = abi.encodePacked(
            bytes4(0xfa461e33),
            int256(amountOwed), // amount0Delta
            int256(0), // amount1Delta
            dataOffset,
            dataLength,
            protocolData
        );

        // transfer funds into the pool - this is taken cared of by the Dispatcher now
        vm.prank(address(uniswapV3Exposed));
        IERC20(WETH_ADDR).transfer(DAI_WETH_USV3, amountOwed);
        vm.startPrank(DAI_WETH_USV3);
        uniswapV3Exposed.handleCallback(callbackData);
        vm.stopPrank();

        uint256 finalPoolReserve = IERC20(WETH_ADDR).balanceOf(DAI_WETH_USV3);
        assertEq(finalPoolReserve - initialPoolReserve, amountOwed);
    }

    function encodeUniswapV3Swap(
        address tokenIn,
        address tokenOut,
        address target,
        bool zero2one
    ) internal view returns (bytes memory) {
        IUniswapV3Pool pool = IUniswapV3Pool(target);
        return abi.encodePacked(tokenIn, tokenOut, pool.fee(), target, zero2one);
    }
}

contract TychoRouterForUniswapV3Test is TychoRouterTestSetup {
    function testSingleSwapUSV3Permit2() public {
        // Trade 1 WETH for DAI with 1 swap on Uniswap V3 using Permit2
        // Tests entire USV3 flow including callback
        // 1 WETH   ->   DAI
        //       (USV3)
        vm.startPrank(ALICE);
        uint256 amountIn = 10 ** 18;
        deal(WETH_ADDR, ALICE, amountIn);
        (
            IAllowanceTransfer.PermitSingle memory permitSingle,
            bytes memory signature
        ) = handlePermit2Approval(WETH_ADDR, tychoRouterAddr, amountIn);

        uint256 expAmountOut = 1205_128428842122129186; //Swap 1 WETH for 1205.12 DAI
        bool zeroForOne = false;
        bytes memory protocolData =
            encodeUniswapV3Swap(WETH_ADDR, DAI_ADDR, DAI_WETH_USV3, zeroForOne);
        bytes memory swap =
            encodeSingleSwap(address(usv3Executor), protocolData);

        tychoRouter.singleSwapPermit2(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            expAmountOut - 1,
            ALICE,
            noClientFee(),
            permitSingle,
            signature,
            swap
        );

        uint256 finalBalance = IERC20(DAI_ADDR).balanceOf(ALICE);
        assertGe(finalBalance, expAmountOut);

        vm.stopPrank();
    }

    // Base Network Test
    // Make sure to set the RPC_URL to base network
    function testSwapPancakeswapBaseNetwork() public {
        vm.skip(true);
        vm.rollFork(38001287);

        // Deploy the executor specifically on this Base fork
        UniswapV3ExecutorExposed basePancakeV3Exposed =
            new UniswapV3ExecutorExposed();

        uint256 amountIn = 1000 * 10 ** 6;
        bool zeroForOne = true;
        bytes memory protocolData = encodeUniswapV3Swap(
            BASE_USDC, BASE_cbBTC, PANCAKESWAPV3_cbBTC_USDC_POOL, zeroForOne
        );

        deal(BASE_USDC, address(basePancakeV3Exposed), amountIn);

        basePancakeV3Exposed.swap(amountIn, protocolData, BOB);

        // 1000 USDC ~= 0.0095 BTC -> 1 BTC ~= 105k USDC ✅
        assertEq(IERC20(BASE_cbBTC).balanceOf(BOB), 950567);
    }
}

contract TychoRouterForUniswapV3PolygonTest is TychoRouterTestSetup {
    function getChain() public pure override returns (string memory) {
        return "polygon";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 68000000;
    }

    function testSingleUniswapV3PolygonIntegration() public {
        deal(POLYGON_WETH, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(POLYGON_USDC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(POLYGON_WETH).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_uniswap_v3_polygon"
        );
        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = IERC20(POLYGON_USDC).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(POLYGON_WETH).balanceOf(tychoRouterAddr), 0);
        assertEq(balanceAfter, 2694197373);
    }
}

contract TychoRouterForUniswapV3BscTest is TychoRouterTestSetup {
    function getChain() public pure override returns (string memory) {
        return "bsc";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 40000000;
    }

    function testSingleUniswapV3BscIntegration() public {
        deal(BSC_WBNB, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(BSC_WETH).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(BSC_WBNB).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_uniswap_v3_bsc"
        );
        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = IERC20(BSC_WETH).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(BSC_WBNB).balanceOf(tychoRouterAddr), 0);
        assertEq(balanceAfter - balanceBefore, 167585949393846682);
    }
}

contract TychoRouterForUniswapV3ArbitrumTest is TychoRouterTestSetup {
    function getChain() public pure override returns (string memory) {
        return "arbitrum";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 280000000;
    }

    function testSingleUniswapV3ArbitrumIntegration() public {
        deal(ARBITRUM_WETH, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(ARBITRUM_USDC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(ARBITRUM_WETH).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_uniswap_v3_arbitrum"
        );
        (bool success,) = tychoRouterAddr.call(callData);

        uint256 balanceAfter = IERC20(ARBITRUM_USDC).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(ARBITRUM_WETH).balanceOf(tychoRouterAddr), 0);
        assertEq(balanceAfter, 3707959149);
    }
}
