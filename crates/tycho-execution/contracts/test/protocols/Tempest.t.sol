pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";

/// @dev A block carrying a committed USDC/WETH lane, after the router upgrade at
/// block 25744018 that turned on `openTakerAccess`. Lane payloads persist in
/// registry storage after the commit, so the ladder is readable; only the
/// timestamp goes stale, which `_refreshLane` restamps.
uint256 constant TEMPEST_FORK_BLOCK = 25963848;

/// @dev Shared fork fixtures for Tempest: a swap only settles if the lane is
/// inside the router's freshness window. On chain the builder guarantees that by
/// ordering the maker's quote tx directly ahead of the fill; here it is set with
/// `vm.store`. No taker fixture is needed at this block because the venue's
/// `openTakerAccess` flag is on, which bypasses the `allowedTaker` check. The
/// gate itself is intact -- an OPERATOR can turn the flag off, and a blocked
/// taker is rejected regardless -- so this test proves the path works today,
/// not that settlement is permanently ungated.
abstract contract TempestFixtures is Constants {
    function _refreshLane(address tokenA, address tokenB) internal {
        bytes32 laneSlot = keccak256(
            abi.encode(TEMPEST_ROUTER, uint256(_lane(tokenA, tokenB)))
        );
        uint256 storedLane = uint256(vm.load(TEMPEST_REGISTRY, laneSlot));

        // Guards against the fork block having no committed ladder, which would
        // make every swap assertion below vacuous.
        require(
            (storedLane >> 216) & 0xff > 0, "no committed lane at fork block"
        );

        vm.store(
            TEMPEST_REGISTRY,
            laneSlot,
            bytes32(
                (uint256(uint32(block.timestamp)) << 224)
                    | (storedLane & ((uint256(1) << 224) - 1))
            )
        );
    }

    /// Mirrors `Tempest.laneFor`: keccak of the ascending-sorted packed pair.
    function _lane(address tokenA, address tokenB)
        internal
        pure
        returns (bytes32)
    {
        (address token0, address token1) =
            tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);
        return keccak256(abi.encodePacked(token0, token1));
    }
}

contract TempestRouterTest is TychoRouterTestSetup, TempestFixtures {
    function getForkBlock() public pure override returns (uint256) {
        return TEMPEST_FORK_BLOCK;
    }

    function testSingleSwap() public {
        uint256 amountIn = 0.1 ether;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_tempest_weth_usdc"
        );

        _refreshLane(WETH_ADDR, USDC_ADDR);

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
