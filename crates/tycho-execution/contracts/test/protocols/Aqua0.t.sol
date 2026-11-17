// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "forge-std/interfaces/IERC20.sol";
import {TestUtils} from "../TestUtils.sol";
import {TychoRouterV3} from "../../src/TychoRouterV3.sol";

/// Replays a binding quote Aqua0 signed for the live Arbitrum Tycho Router, through that router,
/// the Uniswap V4 executor and Aqua0's live hook, at the block it was signed for. The quote was
/// never executed on chain. Calldata comes from `test_single_encoding_strategy_aqua0`.
contract Aqua0RecordedQuoteTest is TestUtils {
    // The recording: the block the quote was signed at, its deadline, the router it names and
    // the amount it quoted.
    uint256 constant FORK_BLOCK = 523_932_429;
    uint256 constant QUOTE_DEADLINE = 1_794_591_921;
    address constant TYCHO_ROUTER = 0x1B82e8C16Ce8E17535C9B6b8AD107f61b0b192B5;
    uint256 constant QUOTED_OUT = 9_971_227;

    address constant USDG = 0x004B506865409877C9fA29bfb1ebA929984B9bbC;
    address constant USDC = 0xaf88d065e77c8cC2239327C5EDb3A432268e5831;
    address constant ALICE = 0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2;
    address constant UNISWAP_V4_EXECUTOR =
        0xdb696336F7A5F9048252664A3475C194dAe0e62f;
    address constant FEE_CALCULATOR = 0x9ca59BD78aAB97Db9692E43684b12072e910F0b7;
    address constant PERMIT2 = 0x000000000022D473030F116dDEE9F6B43aC78BA3;
    uint256 constant AMOUNT_IN = 10_000_000;

    // The hook's refusals, which the pool manager wraps.
    /// `ExpiredDeadline(uint256,uint256)`: the quote is past its deadline.
    bytes4 constant EXPIRED_DEADLINE = 0xc1f738dc;
    /// `NonceUsed(uint256)`: the quote has already filled once.
    bytes4 constant NONCE_USED = 0x1be5cb2f;
    /// `JitExecutorMismatch(address,address)`: the swap comes from another router.
    bytes4 constant EXECUTOR_MISMATCH = 0xa2f78b35;

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
        vm.warp(QUOTE_DEADLINE + 1);

        (bool success, bytes memory reason) = _send(TYCHO_ROUTER);

        assertFalse(success, "an expired quote filled");
        assertTrue(
            _contains(reason, EXPIRED_DEADLINE), "refused for another reason"
        );
    }

    /// A quote fills once: sending the same calldata again is refused.
    function testRecordedQuoteFillsOnlyOnce() public {
        deal(USDG, ALICE, 2 * AMOUNT_IN);

        (bool first,) = _send(TYCHO_ROUTER);
        (bool second, bytes memory reason) = _send(TYCHO_ROUTER);

        assertTrue(first, "the first fill failed");
        assertFalse(second, "the quote filled twice");
        assertTrue(_contains(reason, NONCE_USED), "refused for another reason");
        assertEq(IERC20(USDG).balanceOf(ALICE), AMOUNT_IN);
    }

    /// A quote fills only through the router it names: another Tycho router with the same V4
    /// executor is refused.
    function testRecordedQuoteIsRefusedThroughAnotherRouter() public {
        deal(USDG, ALICE, AMOUNT_IN);
        address[] memory executors = new address[](1);
        executors[0] = UNISWAP_V4_EXECUTOR;
        address other = address(
            new TychoRouterV3(
                PERMIT2,
                FEE_CALCULATOR,
                executors,
                new address[](0),
                address(this),
                address(this),
                address(this)
            )
        );

        (bool success, bytes memory reason) = _send(other);

        assertFalse(success, "the quote filled through another router");
        assertTrue(
            _contains(reason, EXECUTOR_MISMATCH), "refused for another reason"
        );
        assertEq(IERC20(USDG).balanceOf(ALICE), AMOUNT_IN);
    }

    /// Alice approves `router` for one fill and sends the recorded calldata to it.
    function _send(address router)
        private
        returns (bool success, bytes memory reason)
    {
        vm.startPrank(ALICE);
        IERC20(USDG).approve(router, AMOUNT_IN);
        (success, reason) = router.call(
            loadCallDataFromFile("test_single_encoding_strategy_aqua0")
        );
        vm.stopPrank();
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
