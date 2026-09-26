pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

/// @dev The block in which the maker committed a USDC/WETH lane, so the lane's
/// `updateTimestamp` equals this block's timestamp and the router's freshness
/// window passes without the test restamping anything. On chain the builder
/// gets the same result by ordering the maker's quote tx directly ahead of the
/// fill.
///
/// Picked after both of the venue's migrations: `openTakerAccess` was turned on
/// at block 25744018, and the router switched `PrioUpdateRegistry` at 25989123.
/// This block runs implementation 0x94c4c2a0 against registry
/// 0xda7afeed021e...6b81, which is what is live today. No taker fixture is
/// needed because `openTakerAccess` bypasses the `allowedTaker` check -- the
/// gate itself is intact, so this proves the path works today, not that
/// settlement is permanently ungated.
uint256 constant TEMPEST_FORK_BLOCK = 26044074;

contract TempestRouterTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return TEMPEST_FORK_BLOCK;
    }

    function testSingleSwap() public {
        uint256 amountIn = 0.1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_tempest_weth_usdc"
        );

        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);

        uint256 usdcBalanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);
        uint256 wethBalanceBefore = IERC20(WETH_ADDR).balanceOf(ALICE);
        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");
        assertGt(IERC20(USDC_ADDR).balanceOf(ALICE), usdcBalanceBefore);
        assertEq(
            wethBalanceBefore - IERC20(WETH_ADDR).balanceOf(ALICE), amountIn
        );
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}
