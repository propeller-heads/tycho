pragma solidity ^0.8.26;

import {TestUtils} from "../TestUtils.sol";
import {
    UniswapV2Executor,
    TransferManager,
    UniswapV2Executor__InvalidDataLength,
    IUniswapV2Pair
} from "@src/executors/UniswapV2Executor.sol";
import {
    BASE_MAG7,
    BASE_USDC,
    Constants,
    DAI_ADDR,
    WETH_ADDR
} from "../Constants.sol";
import {Permit2TestHelper} from "../Permit2TestHelper.sol";
import {Test} from "../../lib/forge-std/src/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {
    DAI_WETH_UNIV2_POOL,
    USDC_MAG7_POOL
} from "../deployments/UniswapV2.sol";

contract UniswapV2ExecutorExposed is UniswapV2Executor {
    constructor(uint256 _feeBps) UniswapV2Executor(_feeBps) {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (address target, address tokenIn, address tokenOut)
    {
        return _decodeData(data);
    }

    function getAmountOut(address target, uint256 amountIn, bool zeroForOne)
        external
        view
        returns (uint256 amount)
    {
        IUniswapV2Pair pair = IUniswapV2Pair(target);
        (uint112 reserve0, uint112 reserve1,) = pair.getReserves();
        uint112 reserveIn = zeroForOne ? reserve0 : reserve1;
        uint112 reserveOut = zeroForOne ? reserve1 : reserve0;
        return _getAmountOut(amountIn, reserveIn, reserveOut);
    }
}

contract UniswapV2ExecutorTest is Constants, Permit2TestHelper, TestUtils {
    using SafeERC20 for IERC20;

    UniswapV2ExecutorExposed uniswapV2Exposed;
    UniswapV2ExecutorExposed sushiswapV2Exposed;
    UniswapV2ExecutorExposed pancakeswapV2Exposed;
    IERC20 weth = IERC20(WETH_ADDR);
    IERC20 dai = IERC20(DAI_ADDR);

    function setUp() public {
        uint256 forkBlock = 17323404;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        uniswapV2Exposed = new UniswapV2ExecutorExposed(30);
        sushiswapV2Exposed = new UniswapV2ExecutorExposed(30);
        pancakeswapV2Exposed = new UniswapV2ExecutorExposed(25);
    }

    function testDecodeParams() public view {
        bytes memory params =
            abi.encodePacked(address(2), address(3), address(4));

        (address target, address tokenIn, address tokenOut) =
            uniswapV2Exposed.decodeParams(params);

        assertEq(target, address(2));
        assertEq(tokenIn, address(3));
        assertEq(tokenOut, address(4));
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory invalidParams = abi.encodePacked(address(2), address(3));

        vm.expectRevert(UniswapV2Executor__InvalidDataLength.selector);
        uniswapV2Exposed.decodeParams(invalidParams);
    }

    function testGetTransferData() public {
        bytes memory params =
            abi.encodePacked(DAI_WETH_UNIV2_POOL, DAI_ADDR, WETH_ADDR);

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = uniswapV2Exposed.getTransferData(params);

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, DAI_WETH_UNIV2_POOL);
        assertEq(tokenIn, DAI_ADDR);
        assertEq(tokenOut, WETH_ADDR);
        assertEq(outputToRouter, false);
    }

    function testAmountOut() public view {
        uint256 amountOut =
            uniswapV2Exposed.getAmountOut(DAI_WETH_UNIV2_POOL, 10 ** 18, false);
        uint256 expAmountOut = 1847751195973566072891;
        assertEq(amountOut, expAmountOut);
    }

    // triggers a uint112 overflow on purpose
    function testAmountOutInt112Overflow() public view {
        address target = 0x0B9f5cEf1EE41f8CCCaA8c3b4c922Ab406c980CC;
        uint256 amountIn = 83638098812630667483959471576;

        uint256 amountOut =
            uniswapV2Exposed.getAmountOut(target, amountIn, true);

        assertGe(amountOut, 0);
    }

    function testSwapWithTransfer() public {
        uint256 amountIn = 10 ** 18;
        uint256 amountOut = 1847751195973566072891;
        bytes memory protocolData =
            abi.encodePacked(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);

        deal(WETH_ADDR, address(uniswapV2Exposed), amountIn);
        // transfer funds into the pool - this is taken cared of by the Dispatcher now
        vm.prank(address(uniswapV2Exposed));
        IERC20(WETH_ADDR).transfer(DAI_WETH_UNIV2_POOL, amountIn);

        uniswapV2Exposed.swap(amountIn, protocolData, BOB);

        uint256 finalBalance = dai.balanceOf(BOB);
        assertGe(finalBalance, amountOut);
    }

    function testSwapNoTransfer() public {
        uint256 amountIn = 10 ** 18;
        uint256 amountOut = 1847751195973566072891;
        bytes memory protocolData =
            abi.encodePacked(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);

        deal(WETH_ADDR, address(this), amountIn);
        IERC20(WETH_ADDR).transfer(address(DAI_WETH_UNIV2_POOL), amountIn);
        uniswapV2Exposed.swap(amountIn, protocolData, BOB);

        uint256 finalBalance = dai.balanceOf(BOB);
        assertGe(finalBalance, amountOut);
    }

    function testDecodeIntegration() public view {
        bytes memory protocolData =
            hex"88e6a0c2ddd26feeb64f039a2c41296fcb3f5640c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

        (address target, address tokenIn, address tokenOut) =
            uniswapV2Exposed.decodeParams(protocolData);

        assertEq(target, 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640);
        assertEq(tokenIn, 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2); // WETH
        assertEq(tokenOut, 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48); // USDC
    }

    function testSwapIntegration() public {
        // Load executor protocol data from file (pool + tokenIn + tokenOut = 60 bytes)
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_uniswap_v2");

        uint256 amountIn = 10 ** 18;
        uint256 amountOut = 1847751195973566072891;
        deal(WETH_ADDR, address(uniswapV2Exposed), amountIn);
        // transfer funds into the pool - this is taken cared of by the Dispatcher now
        vm.prank(address(uniswapV2Exposed));
        IERC20(WETH_ADDR).transfer(DAI_WETH_UNIV2_POOL, amountIn);
        uniswapV2Exposed.swap(amountIn, protocolData, BOB);

        uint256 finalBalance = dai.balanceOf(BOB);
        assertGe(finalBalance, amountOut);
    }

    // Base Network Tests
    // Make sure to set the RPC_URL to base network
    function testSwapBaseNetwork() public {
        vm.skip(true);
        vm.rollFork(26857267);
        uint256 amountIn = 10 * 10 ** 6;
        bytes memory protocolData =
            abi.encodePacked(USDC_MAG7_POOL, BASE_USDC, BASE_MAG7);

        deal(BASE_USDC, address(uniswapV2Exposed), amountIn);

        uniswapV2Exposed.swap(amountIn, protocolData, BOB);

        assertEq(IERC20(BASE_MAG7).balanceOf(BOB), 1379830606);
    }
}
