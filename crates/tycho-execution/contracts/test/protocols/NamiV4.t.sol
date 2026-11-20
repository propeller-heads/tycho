pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {AeroClDirectSwapper, AeroClReference} from "./AeroCl.t.sol";

interface INamiPool {
    function plugin() external view returns (address);

    function globalState()
        external
        view
        returns (
            uint160 price,
            int24 tick,
            uint16 lastFee,
            uint8 pluginConfig,
            uint16 communityFee,
            bool unlocked
        );
}

interface INamiPlugin {
    function getMevxRouter() external view returns (address);
}

/// Swaps directly on a Nami pool and pays from its own balance in the callback: the reference
/// for what the router must deliver. Algebra's `swap` has Uniswap V3's selector.
contract NamiDirectSwapper {
    uint160 constant MIN_SQRT_PRICE_LIMIT = 4295128740;
    uint160 constant MAX_SQRT_PRICE_LIMIT =
        1461446703485210103287273052203988822378723970341;

    function swap(address pool, bool zeroToOne, uint256 amountIn)
        external
        returns (uint256 amountOut)
    {
        (int256 amount0, int256 amount1) = IUniswapV3Pool(pool)
            .swap(
                address(this),
                zeroToOne,
                // Test amounts are far below int256's maximum.
                // forge-lint: disable-next-line(unsafe-typecast)
                int256(amountIn),
                zeroToOne ? MIN_SQRT_PRICE_LIMIT : MAX_SQRT_PRICE_LIMIT,
                ""
            );
        amountOut = uint256(-(zeroToOne ? amount1 : amount0));
    }

    function algebraSwapCallback(
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

/// Swaps on Nami's live Base WETH/USDC pool (an Algebra Integral pool with the Nami plugin)
/// through TychoRouterV3 with the Uniswap V3 executor: Algebra's `swap` shares Uniswap V3's
/// selector and the router answers `algebraSwapCallback` through its fallback. The calldata
/// comes from the Rust encoder through `calldata.txt`, and every output is checked against a
/// direct swap on the same fork.
contract TychoRouterForNamiV4Test is TychoRouterTestSetup, AeroClReference {
    address constant NAMI_WETH_USDC_POOL =
        0x0E8AF367DbA0c9CB4674D38539eF3465A25a7237;
    /// `beforeSwap` flag of the pool's plugin config.
    uint8 constant BEFORE_SWAP_FLAG = 1;
    bytes32 constant SWAP_FEE_TOPIC =
        keccak256("SwapFee(address,uint24,uint24)");

    function getChain() public pure override returns (string memory) {
        return "base";
    }

    /// The AERO CL fork block, after the plugin's MEVX router was set on this pool, so the
    /// plugin's `afterSwap` runs its full path.
    function getForkBlock() public pure override returns (uint256) {
        return AERO_CL_FORK_BLOCK;
    }

    /// The Rust encoder's bytes are the Uniswap V3 executor's layout with a zero fee field.
    function testEncoderLayout() public view {
        assertEq(
            loadCallDataFromFile("test_encode_nami_v4"),
            abi.encodePacked(
                BASE_WETH, BASE_USDC, uint24(0), NAMI_WETH_USDC_POOL, true
            )
        );
    }

    function testSingleNamiV4Integration() public {
        INamiPool pool = INamiPool(NAMI_WETH_USDC_POOL);
        (,,, uint8 pluginConfig,,) = pool.globalState();
        assertEq(pluginConfig & BEFORE_SWAP_FLAG, BEFORE_SWAP_FLAG);
        assertTrue(
            INamiPlugin(pool.plugin()).getMevxRouter() != address(0),
            "the fork block must have the MEVX router set"
        );

        uint256 amountIn = 0.01 ether;
        uint256 expectedAmountOut =
            _directNamiSwapOutput(BASE_WETH, true, amountIn);
        deal(BASE_WETH, ALICE, amountIn);
        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(BASE_WETH).approve(tychoRouterAddr, type(uint256).max);
        vm.recordLogs();
        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile("test_single_encoding_strategy_nami_v4")
        );
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(BASE_WETH).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_USDC).balanceOf(tychoRouterAddr), 0);
        assertEq(
            IERC20(BASE_USDC).balanceOf(ALICE) - balanceBefore,
            expectedAmountOut
        );
        assertGt(_pluginOverrideFee(), 0, "the plugin must set the fee");
    }

    /// cbBTC -> WETH on AERO CL, then WETH -> USDC on Nami: the Nami hop's input is the first
    /// hop's output, so the reference runs both hops in order on one snapshot.
    function testSequentialNamiV4Integration() public {
        uint256 amountIn = 0.001e8;
        uint256 snapshot = vm.snapshotState();
        AeroClDirectSwapper aeroSwapper = new AeroClDirectSwapper();
        deal(BASE_cbBTC, address(aeroSwapper), amountIn);
        uint256 wethOut =
            aeroSwapper.swap(AERO_CL_WETH_CBBTC_POOL, false, amountIn);
        NamiDirectSwapper namiSwapper = new NamiDirectSwapper();
        deal(BASE_WETH, address(namiSwapper), wethOut);
        uint256 expectedAmountOut =
            namiSwapper.swap(NAMI_WETH_USDC_POOL, true, wethOut);
        vm.revertToState(snapshot);
        deal(BASE_cbBTC, ALICE, amountIn);
        uint256 balanceBefore = IERC20(BASE_USDC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(BASE_cbBTC).approve(tychoRouterAddr, type(uint256).max);
        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile("test_sequential_encoding_strategy_nami_v4")
        );
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(BASE_cbBTC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(BASE_WETH).balanceOf(tychoRouterAddr), 0);
        assertEq(
            IERC20(BASE_USDC).balanceOf(ALICE) - balanceBefore,
            expectedAmountOut
        );
    }

    /// What a direct swap on the Nami pool pays out, without changing the fork's state.
    function _directNamiSwapOutput(
        address tokenIn,
        bool zeroToOne,
        uint256 amountIn
    ) internal returns (uint256 amountOut) {
        uint256 snapshot = vm.snapshotState();
        NamiDirectSwapper swapper = new NamiDirectSwapper();
        deal(tokenIn, address(swapper), amountIn);
        amountOut = swapper.swap(NAMI_WETH_USDC_POOL, zeroToOne, amountIn);
        vm.revertToState(snapshot);
    }

    /// The `overrideFee` of the pool's `SwapFee` event in the recorded logs.
    function _pluginOverrideFee() internal returns (uint256) {
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; i++) {
            if (
                logs[i].emitter == NAMI_WETH_USDC_POOL
                    && logs[i].topics[0] == SWAP_FEE_TOPIC
            ) {
                (uint24 overrideFee,) =
                    abi.decode(logs[i].data, (uint24, uint24));
                return overrideFee;
            }
        }
        revert("pool emitted no SwapFee");
    }
}
