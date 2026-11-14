// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "forge-std/interfaces/IERC20.sol";
import {TestUtils} from "../TestUtils.sol";

/// Replays a binding quote Aqua0 signed for the live Arbitrum Tycho Router, through that router,
/// the Uniswap V4 executor and Aqua0's live hook, at the block it was signed for. The quote was
/// never executed on chain. Calldata comes from `test_single_encoding_strategy_aqua0`.
contract Aqua0RecordedQuoteTest is TestUtils {
    /// The quote's deadline is 1794591921; this block's timestamp is 1794591875.
    uint256 constant FORK_BLOCK = 523_932_429;
    address constant TYCHO_ROUTER = 0x924F147c50eA59f5180a26031A8b65B2aA1e81Cd;
    address constant USDG = 0x004B506865409877C9fA29bfb1ebA929984B9bbC;
    address constant USDC = 0xaf88d065e77c8cC2239327C5EDb3A432268e5831;
    address constant ALICE = 0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2;
    uint256 constant AMOUNT_IN = 10_000_000;
    /// The amount Aqua0 quoted for this fill.
    uint256 constant QUOTED_OUT = 9_971_227;
    /// `ExpiredDeadline(uint256,uint256)`, the hook's refusal of a quote past its deadline.
    bytes4 constant EXPIRED_DEADLINE = 0xc1f738dc;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("arbitrum"), FORK_BLOCK);
    }

    function testRecordedQuoteFillsAtTheQuotedAmount() public {
        deal(USDG, ALICE, AMOUNT_IN);
        uint256 before = IERC20(USDC).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(USDG).approve(TYCHO_ROUTER, AMOUNT_IN);
        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_aqua0");
        uint256 gasBefore = gasleft();
        (bool success,) = TYCHO_ROUTER.call(callData);
        emit log_named_uint("router call gas", gasBefore - gasleft());
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(USDC).balanceOf(ALICE) - before, QUOTED_OUT);
        assertEq(IERC20(USDG).balanceOf(ALICE), 0);
    }

    /// The hook refuses the same quote once its deadline has passed.
    function testRecordedQuoteIsRefusedAfterItsDeadline() public {
        deal(USDG, ALICE, AMOUNT_IN);
        vm.warp(1_794_591_922);

        vm.startPrank(ALICE);
        IERC20(USDG).approve(TYCHO_ROUTER, AMOUNT_IN);
        (bool success, bytes memory reason) = TYCHO_ROUTER.call(
            loadCallDataFromFile("test_single_encoding_strategy_aqua0")
        );
        vm.stopPrank();

        assertFalse(success, "an expired quote filled");
        assertTrue(
            _contains(reason, EXPIRED_DEADLINE), "refused for another reason"
        );
    }

    /// Whether `data` holds `selector` anywhere: the pool manager wraps the hook's revert.
    function _contains(bytes memory data, bytes4 selector)
        private
        pure
        returns (bool)
    {
        for (uint256 i = 0; i + 4 <= data.length; i++) {
            if (
                data[i] == selector[0] && data[i + 1] == selector[1]
                    && data[i + 2] == selector[2] && data[i + 3] == selector[3]
            ) return true;
        }
        return false;
    }
}
