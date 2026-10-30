// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "forge-std/console2.sol";
import "@src/executors/PancakeswapInfinityExecutor.sol";
import {Constants} from "../Constants.sol";
import {
    PancakeswapInfinityExecutorExposed,
    binSwapData,
    INFINITY_VAULT,
    INFINITY_CL_POOL_MANAGER,
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

/// Measures the executor's gas per Bin swap against the bins it crossed, on the live USDC/USDT
/// pool in both directions, to calibrate the simulation's `SWAP_BASE_GAS` and
/// `GAS_PER_BIN_CROSSED` (`pancakeswap_infinity_bin/state.rs`). Logs only, run with `-vv`; the
/// constants were fit by least squares to its output. Sizes past the pool's depth revert
/// `BinPool__OutOfLiquidity` and are logged as such.
contract PancakeswapInfinityBinGasTest is Constants, TestUtils {
    PancakeswapInfinityExecutorExposed executor;
    bytes32 poolId;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("base"), INFINITY_FORK_BLOCK);
        executor = new PancakeswapInfinityExecutorExposed(
            INFINITY_VAULT, INFINITY_CL_POOL_MANAGER, INFINITY_BIN_POOL_MANAGER
        );
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
    }

    function activeId() internal view returns (uint24 id) {
        (id,,) = IBinSlot0(INFINITY_BIN_POOL_MANAGER).getSlot0(poolId);
    }

    function measure(
        address tokenIn,
        address tokenOut,
        bool zeroForOne,
        uint256 amount
    ) internal {
        uint256 snapshot = vm.snapshotState();
        uint24 before = activeId();
        deal(tokenIn, address(executor), amount);
        uint256 g0 = gasleft();
        try executor.swap(
            amount, binSwapData(tokenIn, tokenOut, zeroForOne, 7), ALICE
        ) {
            uint256 used = g0 - gasleft();
            uint24 after_ = activeId();
            uint256 crossed =
                before > after_ ? before - after_ : after_ - before;
            console2.log(
                "%s amount %s gas %s",
                zeroForOne ? "x->y" : "y->x",
                amount,
                used
            );
            console2.log("  crossed %s", crossed);
        } catch {
            console2.log(
                "%s amount %s reverted", zeroForOne ? "x->y" : "y->x", amount
            );
        }
        vm.revertToState(snapshot);
    }

    function testMeasureBinSwapGas() public {
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
