// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import "forge-std/console2.sol";
import {InfinityPoolKey} from "@src/executors/PancakeswapInfinityExecutor.sol";
import {
    binSwapData,
    INFINITY_BIN_POOL_MANAGER,
    INFINITY_FORK_BLOCK,
    BASE_USDT
} from "./PancakeswapInfinity.t.sol";

interface IBinSlot0 {
    function getSlot0(bytes32 id)
        external
        view
        returns (uint24 activeId, uint24 protocolFee, uint24 lpFee);
}

/// Pins the simulation's Bin gas model, `SWAP_BASE_GAS + GAS_PER_BIN_CROSSED * bins crossed`
/// (`pancakeswap_infinity_bin/state.rs`), against swaps through the TychoRouter on the live
/// USDC/USDT pool in both directions, crossing 0 to 21 bins. The model prices a `singleSwap`
/// paid by `transferFrom`: the executor is a callback protocol, so the gas estimator adds
/// nothing for the user's transfer and the quote has to carry it. The constants are the least
/// squares fit to this test's output; run with `-vv` to see the measurements.
contract PancakeswapInfinityBinGasTest is TychoRouterTestSetup {
    uint256 constant SWAP_BASE_GAS = 202_000;
    uint256 constant GAS_PER_BIN_CROSSED = 12_100;
    /// Residual the fit allows per swap. The USDC side costs about 6k more than the USDT side,
    /// which the one base absorbs. The bins on this pool are contiguous, so the active id before
    /// minus after is the number of bins crossed.
    uint256 constant TOLERANCE = 6_000;

    bytes32 poolId;

    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return INFINITY_FORK_BLOCK;
    }

    function setUp() public override {
        super.setUp();
        poolId = keccak256(
            abi.encode(
                InfinityPoolKey({
                    currency0: BASE_USDC,
                    currency1: BASE_USDT,
                    hooks: address(0),
                    poolManager: INFINITY_BIN_POOL_MANAGER,
                    fee: 7,
                    parameters: bytes32(uint256(10) << 16)
                })
            )
        );
        // ALICE holds both tokens, as a trader would, so no balance slot goes between zero and
        // non-zero inside the measured call.
        deal(BASE_USDC, ALICE, 1_000e6);
        deal(BASE_USDT, ALICE, 1_000e6);
    }

    function activeId() internal view returns (uint24 id) {
        (id,,) = IBinSlot0(INFINITY_BIN_POOL_MANAGER).getSlot0(poolId);
    }

    /// Swaps `amount` through the router and checks the gas against the model when the pool
    /// can fill it; past its depth the pool must revert BinPool__OutOfLiquidity.
    function measure(
        address tokenIn,
        address tokenOut,
        bool zeroForOne,
        uint256 amount
    ) internal {
        uint256 snapshot = vm.snapshotState();
        uint24 before = activeId();
        bytes memory swap = encodeSingleSwap(
            address(pancakeswapInfinityExecutor),
            binSwapData(tokenIn, tokenOut, zeroForOne, 7)
        );

        vm.startPrank(ALICE);
        IERC20(tokenIn).approve(tychoRouterAddr, amount);
        uint256 g0 = gasleft();
        try tychoRouter.singleSwap(
            amount, tokenIn, tokenOut, 1, 1, ALICE, noClientFee(), swap
        ) {
            uint256 used = g0 - gasleft();
            uint24 after_ = activeId();
            uint256 crossed =
                before > after_ ? before - after_ : after_ - before;
            uint256 modelled = SWAP_BASE_GAS + GAS_PER_BIN_CROSSED * crossed;
            console2.log(
                "%s amount %s gas %s",
                zeroForOne ? "x->y" : "y->x",
                amount,
                used
            );
            console2.log("  crossed %s modelled %s", crossed, modelled);
            assertApproxEqAbs(used, modelled, TOLERANCE, "gas off the model");
        } catch (bytes memory reason) {
            console2.log(
                "%s amount %s reverted", zeroForOne ? "x->y" : "y->x", amount
            );
            assertEq(
                bytes4(reason),
                bytes4(keccak256("BinPool__OutOfLiquidity()")),
                "not an out-of-liquidity revert"
            );
        }
        vm.stopPrank();
        vm.revertToState(snapshot);
    }

    function testBinSwapGasFitsTheModel() public {
        uint256[10] memory amounts = [
            uint256(1e6),
            3e6,
            10e6,
            30e6,
            50e6,
            100e6,
            150e6,
            200e6,
            250e6,
            300e6
        ];
        for (uint256 i = 0; i < amounts.length; i++) {
            measure(BASE_USDC, BASE_USDT, true, amounts[i]);
        }
        for (uint256 i = 0; i < amounts.length; i++) {
            measure(BASE_USDT, BASE_USDC, false, amounts[i]);
        }
    }
}
