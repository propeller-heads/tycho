// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";

address constant BSC_USDT = 0x55d398326f99059fF775485246999027B3197955;

// USDT/KGEN on BSC, a CLAlphaHook v1 pool (hook 0x72e0…). Its start time was 2025-10-07 11:00
// UTC; at 11:55 the owner moved it to 14:00, pausing the live pool until then.
address constant BSC_KGEN = 0xF3d5b4c34Ed623478cc5141861776E6cf7AE3A1E;
address constant CLALPHA_HOOK_V1 = 0x72e09eBd9b24F47730b651889a4eD984CBa53d90;
// Bitmap 0x55 (beforeInitialize, beforeAdd/RemoveLiquidity, beforeSwap) and tick spacing 10.
bytes32 constant KGEN_POOL_PARAMETERS = bytes32(uint256(0x0a0055));
uint24 constant KGEN_POOL_FEE = 67;

// 14:49 UTC, after trading resumed.
uint256 constant ALPHA_OPEN_BLOCK = 63799000;
// 12:57 UTC, inside the pause.
uint256 constant ALPHA_PAUSED_BLOCK = 63790000;

// NES/USDT on BSC, a CLAlphaHook v3 pool (hook 0xb0bb…), created by `initializePool` with its
// start time, 2026-09-10 08:00 UTC.
address constant BSC_NES = 0x097acf27503753e77bCA940277806156C5925B81;
address constant CLALPHA_HOOK_V3 = 0xb0bb171D333569CfD28a37F5c5DdDAAa90aD46af;
// Bitmap 0x45 (beforeInitialize, beforeAddLiquidity, beforeSwap) and tick spacing 2.
bytes32 constant NES_POOL_PARAMETERS = bytes32(uint256(0x020045));
uint24 constant NES_POOL_FEE = 67;

// 08:41 UTC, 40 minutes into trading.
uint256 constant ALPHA_V3_OPEN_BLOCK = 121040000;
// 07:59:59 UTC, the last block before the start time.
uint256 constant ALPHA_V3_NOT_STARTED_BLOCK = 121034427;

function usdtKgenSwapData() pure returns (bytes memory) {
    return abi.encodePacked(
        BSC_USDT,
        BSC_KGEN,
        true, // USDT sorts below KGEN
        uint8(0), // CL
        KGEN_POOL_FEE,
        KGEN_POOL_PARAMETERS,
        CLALPHA_HOOK_V1
    );
}

function usdtNesSwapData() pure returns (bytes memory) {
    return abi.encodePacked(
        BSC_USDT,
        BSC_NES,
        false, // USDT sorts above NES
        uint8(0), // CL
        NES_POOL_FEE,
        NES_POOL_PARAMETERS,
        CLALPHA_HOOK_V3
    );
}

// The Infinity executor needs nothing new for CLAlphaHook pools: the hook travels in the pool
// key and only gates on its start time.
abstract contract PancakeswapInfinityAlphaBase is TychoRouterTestSetup {
    function getChain() public pure override returns (string memory) {
        return "bsc";
    }

    function swapUsdt(address tokenOut, bytes memory swapData, uint256 amountIn)
        internal
        returns (uint256)
    {
        deal(BSC_USDT, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(BSC_USDT).approve(tychoRouterAddr, amountIn);
        uint256 out = tychoRouter.singleSwap(
            amountIn,
            BSC_USDT,
            tokenOut,
            2 ** 255,
            1,
            ALICE,
            noClientFee(),
            encodeSingleSwap(address(pancakeswapInfinityExecutor), swapData)
        );
        vm.stopPrank();
        return out;
    }

    function assertSwapsUsdt(address tokenOut, bytes memory swapData) internal {
        uint256 before = IERC20(tokenOut).balanceOf(ALICE);

        uint256 out = swapUsdt(tokenOut, swapData, 10e18);
        emit log_named_uint("out for 10 USDT", out);

        assertGt(out, 0);
        assertEq(IERC20(tokenOut).balanceOf(ALICE) - before, out);
        assertEq(IERC20(BSC_USDT).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(tokenOut).balanceOf(tychoRouterAddr), 0);
    }

    /// The hook's `PoolNotStarted()` reverts the whole swap while the pool is closed.
    function assertPoolNotStarted(address tokenOut, bytes memory swapData)
        internal
    {
        deal(BSC_USDT, ALICE, 10e18);
        vm.startPrank(ALICE);
        IERC20(BSC_USDT).approve(tychoRouterAddr, 10e18);
        try tychoRouter.singleSwap(
            10e18,
            BSC_USDT,
            tokenOut,
            2 ** 255,
            1,
            ALICE,
            noClientFee(),
            encodeSingleSwap(address(pancakeswapInfinityExecutor), swapData)
        ) {
            fail();
        } catch (bytes memory reason) {
            assertTrue(
                containsSelector(reason, bytes4(keccak256("PoolNotStarted()"))),
                "the revert must come from the hook's start-time gate"
            );
        }
        vm.stopPrank();
        assertEq(IERC20(BSC_USDT).balanceOf(ALICE), 10e18, "no funds moved");
    }

    function containsSelector(bytes memory data, bytes4 selector)
        internal
        pure
        returns (bool)
    {
        for (uint256 i = 0; i + 4 <= data.length; i++) {
            if (
                data[i] == selector[0] && data[i + 1] == selector[1]
                    && data[i + 2] == selector[2] && data[i + 3] == selector[3]
            ) {
                return true;
            }
        }
        return false;
    }
}

contract PancakeswapInfinityAlphaOpenTest is PancakeswapInfinityAlphaBase {
    function getForkBlock() public pure override returns (uint256) {
        return ALPHA_OPEN_BLOCK;
    }

    function testSwapsThroughAnOpenAlphaPool() public {
        assertSwapsUsdt(BSC_KGEN, usdtKgenSwapData());
    }
}

contract PancakeswapInfinityAlphaPausedTest is PancakeswapInfinityAlphaBase {
    function getForkBlock() public pure override returns (uint256) {
        return ALPHA_PAUSED_BLOCK;
    }

    function testSwapRevertsWhileTheAlphaPoolIsPaused() public {
        assertPoolNotStarted(BSC_KGEN, usdtKgenSwapData());
    }
}

contract PancakeswapInfinityAlphaV3OpenTest is PancakeswapInfinityAlphaBase {
    function getForkBlock() public pure override returns (uint256) {
        return ALPHA_V3_OPEN_BLOCK;
    }

    function testSwapsThroughAnOpenAlphaV3Pool() public {
        assertSwapsUsdt(BSC_NES, usdtNesSwapData());
    }
}

contract PancakeswapInfinityAlphaV3NotStartedTest is
    PancakeswapInfinityAlphaBase
{
    function getForkBlock() public pure override returns (uint256) {
        return ALPHA_V3_NOT_STARTED_BLOCK;
    }

    function testSwapRevertsBeforeTheAlphaV3PoolStarts() public {
        assertPoolNotStarted(BSC_NES, usdtNesSwapData());
    }
}
