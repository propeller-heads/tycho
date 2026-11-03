pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {SlipstreamsExecutorExposed} from "./Slipstreams.t.sol";

/// Swaps directly on a CL pool and pays from its own balance in the callback: the reference for
/// what an executor or the router must deliver.
contract AeroClDirectSwapper {
    uint160 constant MIN_SQRT_PRICE_LIMIT = 4295128740;
    uint160 constant MAX_SQRT_PRICE_LIMIT =
        1461446703485210103287273052203988822378723970341;

    function swap(address pool, bool zeroForOne, uint256 amountIn)
        external
        returns (uint256 amountOut)
    {
        (int256 amount0, int256 amount1) = IUniswapV3Pool(pool)
            .swap(
                address(this),
                zeroForOne,
                // Test amounts are far below int256's maximum.
                // forge-lint: disable-next-line(unsafe-typecast)
                int256(amountIn),
                zeroForOne ? MIN_SQRT_PRICE_LIMIT : MAX_SQRT_PRICE_LIMIT,
                ""
            );
        amountOut = uint256(-(zeroForOne ? amount1 : amount0));
    }

    function uniswapV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata
    ) external {
        if (amount0Delta > 0) {
            // Positive, so the cast is exact.
            // forge-lint: disable-next-line(unsafe-typecast)
            uint256 amount = uint256(amount0Delta);
            require(
                IERC20(IUniswapV3Pool(msg.sender).token0())
                    .transfer(msg.sender, amount)
            );
        }
        if (amount1Delta > 0) {
            // Positive, so the cast is exact.
            // forge-lint: disable-next-line(unsafe-typecast)
            uint256 amount = uint256(amount1Delta);
            require(
                IERC20(IUniswapV3Pool(msg.sender).token1())
                    .transfer(msg.sender, amount)
            );
        }
    }
}

abstract contract AeroClReference is TestUtils {
    /// Base block with AERO v1.1 CL pools trading; the aero_cl simulation fixture uses it too.
    uint256 internal constant AERO_CL_FORK_BLOCK = 52924436;
    address internal constant AERO_CL_WETH_CBBTC_POOL =
        0xbB5507DaaE822ba89C4dA61BbfDf4b49a1666BAc;
    address internal constant AERO_CL_WETH_USDC_POOL =
        0x4A7af46bC8aBc00048c6c7920a1488DAF568366c;
    address internal constant AERO_CL_USDC_CBBTC_POOL =
        0x784E539F15Ca2D19449a6311c5dAb6cF37d8f1ba;

    /// What a direct swap on `pool` pays out, without changing the fork's state.
    function _directSwapOutput(
        address pool,
        address tokenIn,
        bool zeroForOne,
        uint256 amountIn
    ) internal returns (uint256 amountOut) {
        uint256 snapshot = vm.snapshotState();
        AeroClDirectSwapper swapper = new AeroClDirectSwapper();
        deal(tokenIn, address(swapper), amountIn);
        amountOut = swapper.swap(pool, zeroForOne, amountIn);
        vm.revertToState(snapshot);
    }
}

contract AeroClExecutorTest is Constants, AeroClReference {
    SlipstreamsExecutorExposed slipstreamsExposed;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), AERO_CL_FORK_BLOCK);
        slipstreamsExposed = new SlipstreamsExecutorExposed();
    }

    function _wethToCbBtc() internal pure returns (bytes memory) {
        return abi.encodePacked(
            BASE_WETH, BASE_cbBTC, uint24(10), AERO_CL_WETH_CBBTC_POOL, true
        );
    }

    /// The pool's swap hook charges its fee inside the swap; the executor receives exactly what
    /// a direct swap would, and the pool takes the whole input.
    function testSwapDeliversThePoolsOutput() public {
        uint256 amountIn = 0.01 ether;
        uint256 expectedAmountOut = _directSwapOutput(
            AERO_CL_WETH_CBBTC_POOL, BASE_WETH, true, amountIn
        );
        deal(BASE_WETH, address(slipstreamsExposed), amountIn);

        uint256 balanceBefore = IERC20(BASE_cbBTC).balanceOf(BOB);
        slipstreamsExposed.swap(amountIn, _wethToCbBtc(), BOB);

        assertEq(
            IERC20(BASE_cbBTC).balanceOf(BOB) - balanceBefore, expectedAmountOut
        );
        assertEq(IERC20(BASE_WETH).balanceOf(address(slipstreamsExposed)), 0);
    }

    function testSwapIntegration() public {
        bytes memory protocolData = loadCallDataFromFile("test_encode_aero_cl");
        assertEq(protocolData, _wethToCbBtc());
        uint256 amountIn = 0.01 ether;
        uint256 expectedAmountOut = _directSwapOutput(
            AERO_CL_WETH_CBBTC_POOL, BASE_WETH, true, amountIn
        );
        deal(BASE_WETH, address(slipstreamsExposed), amountIn);

        uint256 balanceBefore = IERC20(BASE_cbBTC).balanceOf(BOB);
        slipstreamsExposed.swap(amountIn, protocolData, BOB);

        assertEq(
            IERC20(BASE_cbBTC).balanceOf(BOB) - balanceBefore, expectedAmountOut
        );
    }
}

contract TychoRouterForAeroClTest is TychoRouterTestSetup, AeroClReference {
    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return AERO_CL_FORK_BLOCK;
    }

    function testSingleAeroClIntegration() public {
        uint256 amountIn = 0.01 ether;
        uint256 expectedAmountOut = _directSwapOutput(
            AERO_CL_WETH_CBBTC_POOL, BASE_WETH, true, amountIn
        );
        deal(BASE_WETH, ALICE, amountIn);
        uint256 balanceBefore = IERC20(BASE_cbBTC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(BASE_WETH).approve(tychoRouterAddr, type(uint256).max);
        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile("test_single_encoding_strategy_aero_cl")
        );
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(BASE_WETH).balanceOf(tychoRouterAddr), 0);
        assertEq(
            IERC20(BASE_cbBTC).balanceOf(ALICE) - balanceBefore,
            expectedAmountOut
        );
    }

    /// WETH -> USDC -> cbBTC through two AERO CL pools. The second hop's input is the first
    /// hop's output, so the reference runs both hops in order on one snapshot.
    function testSequentialAeroClIntegration() public {
        uint256 amountIn = 0.01 ether;
        uint256 snapshot = vm.snapshotState();
        AeroClDirectSwapper swapper = new AeroClDirectSwapper();
        deal(BASE_WETH, address(swapper), amountIn);
        uint256 usdcOut = swapper.swap(AERO_CL_WETH_USDC_POOL, true, amountIn);
        uint256 expectedAmountOut =
            swapper.swap(AERO_CL_USDC_CBBTC_POOL, true, usdcOut);
        vm.revertToState(snapshot);
        deal(BASE_WETH, ALICE, amountIn);
        uint256 balanceBefore = IERC20(BASE_cbBTC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(BASE_WETH).approve(tychoRouterAddr, type(uint256).max);
        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile("test_sequential_encoding_strategy_aero_cl")
        );
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(BASE_WETH).balanceOf(tychoRouterAddr), 0);
        assertEq(
            IERC20(BASE_cbBTC).balanceOf(ALICE) - balanceBefore,
            expectedAmountOut
        );
    }
}
