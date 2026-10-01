pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {Constants} from "../Constants.sol";

/// @dev Camelot V3 (Algebra V1.9) pools on Arbitrum One are executed by the
/// unchanged UniswapV3Executor: the pool exposes the same `swap` entry point
/// and its `algebraSwapCallback` reaches the executor through the router's
/// selector-agnostic fallback. The calldata comes from the Rust encoder test
/// of the same name.
contract TychoRouterForCamelotV3ArbitrumTest is TychoRouterTestSetup {
    address constant CAMELOT_V3_WETH_USDC =
        0xB1026b8e7276e7AC75410F1fcbbe21796e8f7526;
    // AlgebraFactory.vaultAddress(): receives the community share of swap fees.
    address constant CAMELOT_V3_VAULT =
        0x58095979B412a366687cA05CbE85fF56241bE21f;

    function getChain() public pure override returns (string memory) {
        return "arbitrum";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 504411773;
    }

    function testSingleCamelotV3ArbitrumIntegration() public {
        deal(ARBITRUM_WETH, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(ARBITRUM_USDC).balanceOf(ALICE);
        uint256 poolWethBefore =
            IERC20(ARBITRUM_WETH).balanceOf(CAMELOT_V3_WETH_USDC);
        uint256 vaultWethBefore =
            IERC20(ARBITRUM_WETH).balanceOf(CAMELOT_V3_VAULT);

        vm.startPrank(ALICE);
        IERC20(ARBITRUM_WETH).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_camelot_v3_arbitrum"
        );
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        uint256 balanceAfter = IERC20(ARBITRUM_USDC).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(IERC20(ARBITRUM_WETH).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(ARBITRUM_WETH).balanceOf(ALICE), 0);
        // The pool keeps the input minus the community fee it forwards to the
        // vault.
        uint256 poolWethDelta = IERC20(ARBITRUM_WETH)
            .balanceOf(CAMELOT_V3_WETH_USDC) - poolWethBefore;
        uint256 vaultWethDelta =
            IERC20(ARBITRUM_WETH).balanceOf(CAMELOT_V3_VAULT) - vaultWethBefore;
        assertGt(vaultWethDelta, 0);
        assertEq(poolWethDelta + vaultWethDelta, 1 ether);
        assertEq(balanceAfter - balanceBefore, 2536742232);
    }
}
