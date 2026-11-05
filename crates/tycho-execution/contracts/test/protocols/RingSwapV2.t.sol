pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {TestUtils} from "../TestUtils.sol";
import {Constants} from "../Constants.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {
    RingSwapV2Executor,
    RingSwapV2Executor__InvalidFewToken,
    RingSwapV2Executor__InvalidPair,
    RingSwapV2Executor__InvalidDataLength,
    RingSwapV2Executor__ZeroFewFactory,
    RingSwapV2Executor__ZeroRingSwapFactory
} from "@src/executors/RingSwapV2Executor.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    IUniswapV2Pair
} from "@uniswap-v2/contracts/interfaces/IUniswapV2Pair.sol";

interface IFewWrappedTokenWithUnderlying {
    function token() external view returns (address);
}

contract RingSwapV2ExecutorExposed is RingSwapV2Executor {
    constructor(address fewFactory_, address ringSwapFactory_)
        RingSwapV2Executor(fewFactory_, ringSwapFactory_)
    {}

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
    uint256 internal constant RING_BACKING_REGRESSION_BLOCK = 20243446;

    address internal constant RING_WBTC_WETH_PAIR =
        0x00B06862dE00a7e67a2d6d3FbEEa592A32460de0;
    address internal constant RING_WETH_USDT_PAIR =
        0x147D15e009a63Ebed5196EA029679329204f98fd;
    address internal constant RING_USDC_WETH_PAIR =
        0x54222F404dcfAc705322045F01D100380b871450;
    address internal constant RING_DAI_WETH_PAIR =
        0x68C498Df05982d635914ee0Ae6501C749A78B473;
    address internal constant RING_USDC_DAI_PAIR =
        0x3CF3B56fE4c7c7bEe743B8675eC470E1b02Bb468;
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
        ringSwapV2Exposed =
            new RingSwapV2ExecutorExposed(RING_FEW_FACTORY, RING_SWAP_FACTORY);
    }

    function testConstructorConfig() public view {
        assertEq(ringSwapV2Exposed.fewFactory(), RING_FEW_FACTORY);
        assertEq(ringSwapV2Exposed.ringSwapFactory(), RING_SWAP_FACTORY);
    }

    function testConstructorRevertsOnZeroFewFactory() public {
        vm.expectRevert(RingSwapV2Executor__ZeroFewFactory.selector);
        new RingSwapV2ExecutorExposed(address(0), RING_SWAP_FACTORY);
    }

    function testConstructorRevertsOnZeroRingSwapFactory() public {
        vm.expectRevert(RingSwapV2Executor__ZeroRingSwapFactory.selector);
        new RingSwapV2ExecutorExposed(RING_FEW_FACTORY, address(0));
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
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, FW_DAI);
        assertEq(tokenIn, DAI_ADDR);
        assertEq(tokenOut, WETH_ADDR);
        assertEq(outputToRouter, false);
    }

    function testGetTransferDataRejectsUnofficialPair() public {
        address maliciousPair = makeAddr("malicious-ring-pair");
        bytes memory params = abi.encodePacked(
            maliciousPair, DAI_ADDR, WETH_ADDR, FW_DAI, FW_WETH
        );

        vm.expectRevert(
            abi.encodeWithSelector(
                RingSwapV2Executor__InvalidPair.selector,
                maliciousPair,
                FW_DAI,
                FW_WETH
            )
        );
        ringSwapV2Exposed.getTransferData(params);
    }

    function testGetTransferDataRejectsZeroFewTokenForUnregisteredToken()
        public
    {
        address unregisteredToken = makeAddr("unregistered-token");
        bytes memory params = abi.encodePacked(
            address(0), unregisteredToken, WETH_ADDR, address(0), FW_WETH
        );

        vm.expectRevert(
            abi.encodeWithSelector(
                RingSwapV2Executor__InvalidFewToken.selector,
                unregisteredToken,
                address(0)
            )
        );
        ringSwapV2Exposed.getTransferData(params);
    }

    function testGetTransferDataRejectsZeroPairWhenFactoryReturnsZero() public {
        vm.mockCall(
            RING_SWAP_FACTORY,
            abi.encodeWithSignature(
                "getPair(address,address)", FW_DAI, FW_WETH
            ),
            abi.encode(address(0))
        );
        bytes memory params =
            abi.encodePacked(address(0), DAI_ADDR, WETH_ADDR, FW_DAI, FW_WETH);

        vm.expectRevert(
            abi.encodeWithSelector(
                RingSwapV2Executor__InvalidPair.selector,
                address(0),
                FW_DAI,
                FW_WETH
            )
        );
        ringSwapV2Exposed.getTransferData(params);
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

    function testSwapRejectsInvalidInputFewToken() public {
        bytes memory params = abi.encodePacked(
            RING_DAI_WETH_PAIR, DAI_ADDR, WETH_ADDR, FW_WETH, FW_WETH
        );

        vm.expectRevert(
            abi.encodeWithSelector(
                RingSwapV2Executor__InvalidFewToken.selector, DAI_ADDR, FW_WETH
            )
        );
        ringSwapV2Exposed.swap(100 ether, params, BOB);
    }

    function testSwapRejectsInvalidOutputFewToken() public {
        bytes memory params = abi.encodePacked(
            RING_DAI_WETH_PAIR, DAI_ADDR, WETH_ADDR, FW_DAI, FW_DAI
        );

        vm.expectRevert(
            abi.encodeWithSelector(
                RingSwapV2Executor__InvalidFewToken.selector, WETH_ADDR, FW_DAI
            )
        );
        ringSwapV2Exposed.swap(100 ether, params, BOB);
    }

    function testSwapRejectsUnofficialPairBeforeWrapping() public {
        uint256 amountIn = 100 ether;
        address maliciousPair = makeAddr("malicious-ring-pair");
        bytes memory params = abi.encodePacked(
            maliciousPair, DAI_ADDR, WETH_ADDR, FW_DAI, FW_WETH
        );

        deal(DAI_ADDR, address(ringSwapV2Exposed), amountIn);
        vm.prank(address(ringSwapV2Exposed));
        IERC20(DAI_ADDR).approve(FW_DAI, amountIn);

        vm.expectRevert(
            abi.encodeWithSelector(
                RingSwapV2Executor__InvalidPair.selector,
                maliciousPair,
                FW_DAI,
                FW_WETH
            )
        );
        ringSwapV2Exposed.swap(amountIn, params, BOB);

        assertEq(
            IERC20(DAI_ADDR).balanceOf(address(ringSwapV2Exposed)), amountIn
        );
        assertEq(IERC20(FW_DAI).balanceOf(maliciousPair), 0);
    }

    function testSwapRevertsWhenOutputFewTokenUnderlyingBackingIsInsufficient()
        public
    {
        vm.createSelectFork(vm.rpcUrl("mainnet"), RING_BACKING_REGRESSION_BLOCK);
        RingSwapV2ExecutorExposed executor =
            new RingSwapV2ExecutorExposed(RING_FEW_FACTORY, RING_SWAP_FACTORY);

        (uint112 reserve0, uint112 reserve1,) =
            IUniswapV2Pair(RING_USDC_DAI_PAIR).getReserves();
        uint256 usdcBacking = IERC20(USDC_ADDR).balanceOf(FW_USDC);
        assertLt(usdcBacking, reserve0);

        // DAI is pair token1 and USDC is pair token0. Pick the smallest input whose normal V2
        // output is greater than the USDC held by FW_USDC, so unwrapTo must fail.
        uint256 amountIn = ((usdcBacking + 1) * uint256(reserve1) * 10_000)
            / (9_970 * (uint256(reserve0) - usdcBacking - 1)) + 1;
        uint256 expectedOut =
            executor.getAmountOut(RING_USDC_DAI_PAIR, amountIn, false);
        assertGt(expectedOut, usdcBacking);

        deal(DAI_ADDR, address(executor), amountIn);
        vm.prank(address(executor));
        IERC20(DAI_ADDR).approve(FW_DAI, amountIn);

        bytes memory params = abi.encodePacked(
            RING_USDC_DAI_PAIR, DAI_ADDR, USDC_ADDR, FW_DAI, FW_USDC
        );
        (bool success,) = address(executor)
            .call(abi.encodeCall(executor.swap, (amountIn, params, BOB)));

        assertFalse(success);
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
        vm.prank(address(ringSwapV2Exposed));
        IERC20(tokenIn).approve(fwTokenIn, amountIn);

        uint256 balanceBefore = IERC20(tokenOut).balanceOf(BOB);
        ringSwapV2Exposed.swap(amountIn, params, BOB);
        uint256 balanceAfter = IERC20(tokenOut).balanceOf(BOB);

        assertGt(expectedAmountOut, 0);
        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
        assertEq(IERC20(tokenIn).balanceOf(address(ringSwapV2Exposed)), 0);
        assertEq(
            IERC20(tokenIn).allowance(address(ringSwapV2Exposed), fwTokenIn), 0
        );
        assertEq(IERC20(fwTokenIn).balanceOf(address(ringSwapV2Exposed)), 0);
        assertEq(IERC20(fwTokenOut).balanceOf(address(ringSwapV2Exposed)), 0);
    }
}

contract RingSwapV2ExecutorBscTest is TestUtils {
    uint256 internal constant BSC_RING_FORK_BLOCK = 46793446;

    address internal constant BSC_RING_SWAP_FACTORY =
        0x4De602A30Ad7fEf8223dcf67A9fB704324C4dd9B;
    address internal constant BSC_FEW_FACTORY =
        0xEeE400Eabfba8F60f4e6B351D8577394BeB972CD;
    address internal constant BSC_RING_WBNB_USDT_PAIR =
        0x653Cd6B4F72585647aC9F7086550CA7E5C8E8a4c;
    address internal constant BSC_WBNB =
        0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c;
    address internal constant BSC_USDT =
        0x55d398326f99059fF775485246999027B3197955;
    address internal constant BSC_FW_WBNB =
        0x7f0172b75d3823D8aF04feE3A3f6a14aBD68EFE1;
    address internal constant BSC_FW_USDT =
        0x95b5aEfbcC9e6E462f21E0847e2d90b89b7Cd028;

    address internal receiver = makeAddr("bsc-ring-receiver");
    RingSwapV2ExecutorExposed ringSwapV2Exposed;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("bsc"), BSC_RING_FORK_BLOCK);
        ringSwapV2Exposed = new RingSwapV2ExecutorExposed(
            BSC_FEW_FACTORY, BSC_RING_SWAP_FACTORY
        );
    }

    function testBscConstructorConfig() public view {
        assertEq(ringSwapV2Exposed.fewFactory(), BSC_FEW_FACTORY);
        assertEq(ringSwapV2Exposed.ringSwapFactory(), BSC_RING_SWAP_FACTORY);
    }

    function testBscCanonicalSwapDataUsesProtocolWillDebit() public {
        bytes memory params = abi.encodePacked(
            BSC_RING_WBNB_USDT_PAIR,
            BSC_WBNB,
            BSC_USDT,
            BSC_FW_WBNB,
            BSC_FW_USDT
        );

        (
            TransferManager.TransferType transferType,
            address transferReceiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = ringSwapV2Exposed.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(transferReceiver, BSC_FW_WBNB);
        assertEq(tokenIn, BSC_WBNB);
        assertEq(tokenOut, BSC_USDT);
        assertFalse(outputToRouter);
    }

    function testBscFewTokenMappingsMatchUnderlyingTokens() public view {
        assertEq(IFewWrappedTokenWithUnderlying(BSC_FW_WBNB).token(), BSC_WBNB);
        assertEq(IFewWrappedTokenWithUnderlying(BSC_FW_USDT).token(), BSC_USDT);
    }

    function testSwapBscWbnbForUsdtWrapsSwapsAndUnwraps() public {
        _assertBscSwapWrapsSwapsAndUnwraps(
            BSC_WBNB, BSC_USDT, BSC_FW_WBNB, BSC_FW_USDT, 0.001 ether
        );
    }

    function testSwapBscUsdtForWbnbWrapsSwapsAndUnwraps() public {
        _assertBscSwapWrapsSwapsAndUnwraps(
            BSC_USDT, BSC_WBNB, BSC_FW_USDT, BSC_FW_WBNB, 1 ether
        );
    }

    function testBscRejectsFakeFewTokenBeforeFundsMove() public {
        uint256 amountIn = 0.001 ether;
        address fakeFewToken = makeAddr("fake-bsc-few-token");
        bytes memory params = abi.encodePacked(
            BSC_RING_WBNB_USDT_PAIR,
            BSC_WBNB,
            BSC_USDT,
            fakeFewToken,
            BSC_FW_USDT
        );
        uint256 pairBalanceBefore =
            IERC20(BSC_FW_WBNB).balanceOf(BSC_RING_WBNB_USDT_PAIR);
        deal(BSC_WBNB, address(ringSwapV2Exposed), amountIn);

        vm.expectRevert(
            abi.encodeWithSelector(
                RingSwapV2Executor__InvalidFewToken.selector,
                BSC_WBNB,
                fakeFewToken
            )
        );
        ringSwapV2Exposed.swap(amountIn, params, receiver);

        assertEq(
            IERC20(BSC_WBNB).balanceOf(address(ringSwapV2Exposed)), amountIn
        );
        assertEq(
            IERC20(BSC_FW_WBNB).balanceOf(BSC_RING_WBNB_USDT_PAIR),
            pairBalanceBefore
        );
        assertEq(
            IERC20(BSC_WBNB)
                .allowance(address(ringSwapV2Exposed), fakeFewToken),
            0
        );
    }

    function testBscRejectsUnofficialPairBeforeFundsMove() public {
        uint256 amountIn = 0.001 ether;
        address unofficialPair = makeAddr("unofficial-bsc-ring-pair");
        bytes memory params = abi.encodePacked(
            unofficialPair, BSC_WBNB, BSC_USDT, BSC_FW_WBNB, BSC_FW_USDT
        );
        deal(BSC_WBNB, address(ringSwapV2Exposed), amountIn);

        vm.expectRevert(
            abi.encodeWithSelector(
                RingSwapV2Executor__InvalidPair.selector,
                unofficialPair,
                BSC_FW_WBNB,
                BSC_FW_USDT
            )
        );
        ringSwapV2Exposed.swap(amountIn, params, receiver);

        assertEq(
            IERC20(BSC_WBNB).balanceOf(address(ringSwapV2Exposed)), amountIn
        );
        assertEq(IERC20(BSC_FW_WBNB).balanceOf(unofficialPair), 0);
        assertEq(
            IERC20(BSC_WBNB).allowance(address(ringSwapV2Exposed), BSC_FW_WBNB),
            0
        );
    }

    function _assertBscSwapWrapsSwapsAndUnwraps(
        address tokenIn,
        address tokenOut,
        address fwTokenIn,
        address fwTokenOut,
        uint256 amountIn
    ) internal {
        bool zeroForOne = fwTokenIn < fwTokenOut;
        uint256 expectedAmountOut = ringSwapV2Exposed.getAmountOut(
            BSC_RING_WBNB_USDT_PAIR, amountIn, zeroForOne
        );
        bytes memory params = abi.encodePacked(
            BSC_RING_WBNB_USDT_PAIR, tokenIn, tokenOut, fwTokenIn, fwTokenOut
        );

        deal(tokenIn, address(ringSwapV2Exposed), amountIn);
        vm.prank(address(ringSwapV2Exposed));
        IERC20(tokenIn).approve(fwTokenIn, amountIn);

        uint256 balanceBefore = IERC20(tokenOut).balanceOf(receiver);
        ringSwapV2Exposed.swap(amountIn, params, receiver);
        uint256 balanceAfter = IERC20(tokenOut).balanceOf(receiver);

        assertGt(expectedAmountOut, 0);
        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
        assertEq(IERC20(tokenIn).balanceOf(address(ringSwapV2Exposed)), 0);
        assertEq(
            IERC20(tokenIn).allowance(address(ringSwapV2Exposed), fwTokenIn), 0
        );
        assertEq(IERC20(fwTokenIn).balanceOf(address(ringSwapV2Exposed)), 0);
        assertEq(IERC20(fwTokenOut).balanceOf(address(ringSwapV2Exposed)), 0);
    }
}

contract TychoRouterForRingSwapV2Test is TychoRouterTestSetup {
    uint256 internal constant RING_FORK_BLOCK = 25283712;

    address internal constant RING_DAI_WETH_PAIR =
        0x68C498Df05982d635914ee0Ae6501C749A78B473;
    address internal constant FW_DAI =
        0x8A6fe57C08C84e0f4eE97aAe68a62e820a37d259;
    address internal constant FW_WETH =
        0xa250CC729Bb3323e7933022a67B52200fE354767;

    function getForkBlock() public pure override returns (uint256) {
        return RING_FORK_BLOCK;
    }

    function testSingleSwap() public {
        uint256 amountIn = 100 ether;
        deal(DAI_ADDR, ALICE, amountIn);
        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_ring_swap_v2");

        uint256 balanceBefore = IERC20(WETH_ADDR).balanceOf(ALICE);
        vm.startPrank(ALICE);
        IERC20(DAI_ADDR).approve(tychoRouterAddr, type(uint256).max);
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        uint256 amountOut = IERC20(WETH_ADDR).balanceOf(ALICE) - balanceBefore;
        assertTrue(success, "Call Failed");
        assertGt(amountOut, 0);
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), 0);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(DAI_ADDR).allowance(tychoRouterAddr, FW_DAI), 0);
        assertEq(IERC20(FW_DAI).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(FW_WETH).balanceOf(tychoRouterAddr), 0);
    }

    function testSequentialSwapIntoRingSwapV2() public {
        uint256 amountIn = 100e6;
        deal(USDC_ADDR, ALICE, amountIn);
        bytes memory callData = loadCallDataFromFile(
            "test_sequential_encoding_strategy_uniswap_v2_ring_swap_v2"
        );

        uint256 balanceBefore = IERC20(WETH_ADDR).balanceOf(ALICE);
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, type(uint256).max);
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        uint256 amountOut = IERC20(WETH_ADDR).balanceOf(ALICE) - balanceBefore;
        assertTrue(success, "Call Failed");
        assertGt(amountOut, 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(DAI_ADDR).allowance(tychoRouterAddr, FW_DAI), 0);
        assertEq(IERC20(FW_DAI).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(FW_WETH).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testSingleSwapRejectsUnofficialRingPairWithoutLeavingApproval()
        public
    {
        uint256 amountIn = 100 ether;
        address maliciousPair = makeAddr("malicious-ring-pair");
        bytes memory protocolData = abi.encodePacked(
            maliciousPair, DAI_ADDR, WETH_ADDR, FW_DAI, FW_WETH
        );
        bytes memory swap =
            encodeSingleSwap(address(ringSwapV2Executor), protocolData);

        deal(DAI_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(DAI_ADDR).approve(tychoRouterAddr, amountIn);
        vm.expectRevert(
            abi.encodeWithSelector(
                RingSwapV2Executor__InvalidPair.selector,
                maliciousPair,
                FW_DAI,
                FW_WETH
            )
        );
        tychoRouter.singleSwap(
            amountIn, DAI_ADDR, WETH_ADDR, 1, 1, ALICE, noClientFee(), swap
        );
        vm.stopPrank();

        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), amountIn);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(DAI_ADDR).allowance(tychoRouterAddr, FW_DAI), 0);
        assertEq(IERC20(FW_DAI).balanceOf(maliciousPair), 0);
    }

    function testSingleSwapRejectsFakeFewTokenWithoutLeavingApproval() public {
        uint256 amountIn = 100 ether;
        address fakeFewToken = makeAddr("fake-few-token");
        bytes memory protocolData = abi.encodePacked(
            RING_DAI_WETH_PAIR, DAI_ADDR, WETH_ADDR, fakeFewToken, FW_WETH
        );
        bytes memory swap =
            encodeSingleSwap(address(ringSwapV2Executor), protocolData);

        deal(DAI_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(DAI_ADDR).approve(tychoRouterAddr, amountIn);
        vm.expectRevert(
            abi.encodeWithSelector(
                RingSwapV2Executor__InvalidFewToken.selector,
                DAI_ADDR,
                fakeFewToken
            )
        );
        tychoRouter.singleSwap(
            amountIn, DAI_ADDR, WETH_ADDR, 1, 1, ALICE, noClientFee(), swap
        );
        vm.stopPrank();

        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), amountIn);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(DAI_ADDR).allowance(tychoRouterAddr, fakeFewToken), 0);
    }
}
