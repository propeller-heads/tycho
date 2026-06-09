pragma solidity ^0.8.26;

import {TestUtils} from "../TestUtils.sol";
import {Constants} from "../Constants.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {
    RingSwapV2Executor,
    RingSwapV2Executor__InvalidDataLength
} from "@src/executors/RingSwapV2Executor.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    IUniswapV2Pair
} from "@uniswap-v2/contracts/interfaces/IUniswapV2Pair.sol";

interface IFewWrappedTokenWithUnderlying {
    function token() external view returns (address);
}

contract RingSwapV2ExecutorExposed is RingSwapV2Executor {
    function decodeParams(bytes calldata data)
        external
        pure
        returns (
            address target,
            address tokenIn,
            address tokenOut,
            address fwTokenIn,
            address fwTokenOut
        )
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

contract RingSwapV2ExecutorTest is Constants, TestUtils {
    uint256 internal constant RING_FORK_BLOCK = 25283712;

    address internal constant RING_WBTC_WETH_PAIR =
        0x00B06862dE00a7e67a2d6d3FbEEa592A32460de0;
    address internal constant RING_WETH_USDT_PAIR =
        0x147D15e009a63Ebed5196EA029679329204f98fd;
    address internal constant RING_USDC_WETH_PAIR =
        0x54222F404dcfAc705322045F01D100380b871450;
    address internal constant RING_DAI_WETH_PAIR =
        0x68C498Df05982d635914ee0Ae6501C749A78B473;
    address internal constant FW_WBTC =
        0x2078f336Fdd260f708BEc4a20c82b063274E1b23;
    address internal constant FW_USDC =
        0x0492560FA7Cfd6A85E50D8bE3F77318994F8f429;
    address internal constant FW_DAI =
        0x8A6fe57C08C84e0f4eE97aAe68a62e820a37d259;
    address internal constant FW_WETH =
        0xa250CC729Bb3323e7933022a67B52200fE354767;
    address internal constant FW_USDT =
        0xef87f4608e601E8564800265AeE1c1FfaDF73283;

    RingSwapV2ExecutorExposed ringSwapV2Exposed;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), RING_FORK_BLOCK);
        ringSwapV2Exposed = new RingSwapV2ExecutorExposed();
    }

    function testDecodeParams() public view {
        bytes memory params = abi.encodePacked(
            RING_DAI_WETH_PAIR, DAI_ADDR, WETH_ADDR, FW_DAI, FW_WETH
        );

        (
            address target,
            address tokenIn,
            address tokenOut,
            address fwTokenIn,
            address fwTokenOut
        ) = ringSwapV2Exposed.decodeParams(params);

        assertEq(target, RING_DAI_WETH_PAIR);
        assertEq(tokenIn, DAI_ADDR);
        assertEq(tokenOut, WETH_ADDR);
        assertEq(fwTokenIn, FW_DAI);
        assertEq(fwTokenOut, FW_WETH);
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory invalidParams =
            abi.encodePacked(RING_DAI_WETH_PAIR, DAI_ADDR, WETH_ADDR);

        vm.expectRevert(RingSwapV2Executor__InvalidDataLength.selector);
        ringSwapV2Exposed.decodeParams(invalidParams);
    }

    function testDecodeForwardIntegration() public view {
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_ring_swap_v2_forward");

        (
            address target,
            address tokenIn,
            address tokenOut,
            address fwTokenIn,
            address fwTokenOut
        ) = ringSwapV2Exposed.decodeParams(protocolData);

        assertEq(target, RING_DAI_WETH_PAIR);
        assertEq(tokenIn, DAI_ADDR);
        assertEq(tokenOut, WETH_ADDR);
        assertEq(fwTokenIn, FW_DAI);
        assertEq(fwTokenOut, FW_WETH);
    }

    function testDecodeReverseIntegration() public view {
        bytes memory protocolData =
            loadCallDataFromFile("test_encode_ring_swap_v2_reverse");

        (
            address target,
            address tokenIn,
            address tokenOut,
            address fwTokenIn,
            address fwTokenOut
        ) = ringSwapV2Exposed.decodeParams(protocolData);

        assertEq(target, RING_DAI_WETH_PAIR);
        assertEq(tokenIn, WETH_ADDR);
        assertEq(tokenOut, DAI_ADDR);
        assertEq(fwTokenIn, FW_WETH);
        assertEq(fwTokenOut, FW_DAI);
    }

    function testGetTransferDataUsesUnderlyingTokens() public {
        bytes memory params = abi.encodePacked(
            RING_DAI_WETH_PAIR, DAI_ADDR, WETH_ADDR, FW_DAI, FW_WETH
        );

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = ringSwapV2Exposed.getTransferData(params);

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, address(this));
        assertEq(tokenIn, DAI_ADDR);
        assertEq(tokenOut, WETH_ADDR);
        assertEq(outputToRouter, false);
    }

    function testFundsExpectedAddressUsesRouterContext() public view {
        bytes memory params = abi.encodePacked(
            RING_DAI_WETH_PAIR, DAI_ADDR, WETH_ADDR, FW_DAI, FW_WETH
        );

        address receiver = ringSwapV2Exposed.fundsExpectedAddress(params);

        assertEq(receiver, address(this));
    }

    function testFewTokenMappingMatchesUnderlyingTokens() public view {
        assertEq(IFewWrappedTokenWithUnderlying(FW_WBTC).token(), WBTC_ADDR);
        assertEq(IFewWrappedTokenWithUnderlying(FW_USDC).token(), USDC_ADDR);
        assertEq(IFewWrappedTokenWithUnderlying(FW_DAI).token(), DAI_ADDR);
        assertEq(IFewWrappedTokenWithUnderlying(FW_WETH).token(), WETH_ADDR);
        assertEq(IFewWrappedTokenWithUnderlying(FW_USDT).token(), USDT_ADDR);
    }

    function testSwapWbtcForWethWrapsSwapsAndUnwraps() public {
        _assertSwapWrapsSwapsAndUnwraps(
            RING_WBTC_WETH_PAIR, WBTC_ADDR, WETH_ADDR, FW_WBTC, FW_WETH, 0.01e8
        );
    }

    function testSwapWethForUsdtWrapsSwapsAndUnwraps() public {
        _assertSwapWrapsSwapsAndUnwraps(
            RING_WETH_USDT_PAIR,
            WETH_ADDR,
            USDT_ADDR,
            FW_WETH,
            FW_USDT,
            0.01 ether
        );
    }

    function testSwapUsdcForWethWrapsSwapsAndUnwraps() public {
        _assertSwapWrapsSwapsAndUnwraps(
            RING_USDC_WETH_PAIR, USDC_ADDR, WETH_ADDR, FW_USDC, FW_WETH, 100e6
        );
    }

    function testSwapDaiForWethWrapsSwapsAndUnwraps() public {
        _assertSwapWrapsSwapsAndUnwraps(
            RING_DAI_WETH_PAIR, DAI_ADDR, WETH_ADDR, FW_DAI, FW_WETH, 100 ether
        );
    }

    function testSwapWethForDaiWrapsSwapsAndUnwraps() public {
        _assertSwapWrapsSwapsAndUnwraps(
            RING_DAI_WETH_PAIR, WETH_ADDR, DAI_ADDR, FW_WETH, FW_DAI, 0.01 ether
        );
    }

    function _assertSwapWrapsSwapsAndUnwraps(
        address pair,
        address tokenIn,
        address tokenOut,
        address fwTokenIn,
        address fwTokenOut,
        uint256 amountIn
    ) internal {
        bool zeroForOne = fwTokenIn < fwTokenOut;
        uint256 expectedAmountOut =
            ringSwapV2Exposed.getAmountOut(pair, amountIn, zeroForOne);
        bytes memory params =
            abi.encodePacked(pair, tokenIn, tokenOut, fwTokenIn, fwTokenOut);

        deal(tokenIn, address(ringSwapV2Exposed), amountIn);

        uint256 balanceBefore = IERC20(tokenOut).balanceOf(BOB);
        ringSwapV2Exposed.swap(amountIn, params, BOB);
        uint256 balanceAfter = IERC20(tokenOut).balanceOf(BOB);

        assertGt(expectedAmountOut, 0);
        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
        assertEq(IERC20(tokenIn).balanceOf(address(ringSwapV2Exposed)), 0);
        assertEq(IERC20(fwTokenIn).balanceOf(address(ringSwapV2Exposed)), 0);
        assertEq(IERC20(fwTokenOut).balanceOf(address(ringSwapV2Exposed)), 0);
    }
}
