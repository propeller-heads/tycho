// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {TychoRouterTestSetup} from "../TychoRouterTestSetup.sol";
import {
    BaibaiExecutor,
    IBaibaiEntrypoint
} from "@src/executors/BaibaiExecutor.sol";
import {TransferManager} from "@src/TransferManager.sol";

interface IBaibaiCurveQuote {
    function quote(address base, address tokenIn, uint256 amountIn)
        external
        view
        returns (uint256);
}

contract BaibaiTest is TychoRouterTestSetup {
    address constant BOOK = 0x604d9b9eB1e1571C78661a6C1088427EC9c8c6E5;
    address constant CUSTODIAN = 0xAaC48FEB93c5C97E0fb3c7C57E1633922A4ACDa3;
    address constant BASE = 0x4200000000000000000000000000000000000006;
    address constant QUOTE = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    string fixture;

    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 51191196;
    }

    function setUp() public override {
        super.setUp();
        fixture = vm.readFile(
            "../../tycho-simulation/tests/assets/baibai-quotes.json"
        );
        _curve(0);
        deal(BASE, CUSTODIAN, 1e24);
        deal(QUOTE, CUSTODIAN, 1e24);
    }

    function _curve(uint256 scenario) internal {
        bytes32[] memory slots = vm.parseJsonBytes32Array(fixture, ".slots");
        bytes32[] memory words = vm.parseJsonBytes32Array(
            fixture,
            string.concat(".scenarios[", vm.toString(scenario), "].words")
        );
        for (uint256 i; i < slots.length; ++i) {
            vm.store(BOOK, slots[i], words[i]);
        }
        vm.warp(vm.parseJsonUint(fixture, ".timestamp"));
    }

    /// @dev Independently validates the native simulator's golden values against deployed bytecode.
    function testQuoteFixture() public {
        for (uint256 scenario; scenario < 6; ++scenario) {
            _curve(scenario);
            for (uint256 i; i < 18; ++i) {
                string memory path = string.concat(
                    ".scenarios[",
                    vm.toString(scenario),
                    "].cases[",
                    vm.toString(i),
                    "]"
                );
                bool sell = vm.parseJsonBool(
                    fixture, string.concat(path, ".sell_base")
                );
                uint256 input = vm.parseUint(
                    vm.parseJsonString(fixture, string.concat(path, ".input"))
                );
                uint256 expected = vm.parseUint(
                    vm.parseJsonString(fixture, string.concat(path, ".output"))
                );
                assertEq(
                    IBaibaiCurveQuote(BOOK)
                        .quote(BASE, sell ? BASE : QUOTE, input),
                    expected
                );
            }
        }
    }

    /// @dev Output token and debit destination are derived for both swap directions.
    function testTransferData() public view {
        for (uint8 sell; sell < 2; ++sell) {
            (
                TransferManager.TransferType kind,
                address receiver,
                address tokenIn,
                address tokenOut,
                bool outputToRouter
            ) = baibaiExecutor.getTransferData(abi.encodePacked(BASE, sell));
            assertEq(
                uint8(kind),
                uint8(TransferManager.TransferType.ProtocolWillDebit)
            );
            assertEq(receiver, BAIBAI_ENTRYPOINT);
            assertEq(tokenIn, sell == 1 ? BASE : QUOTE);
            assertEq(tokenOut, sell == 1 ? QUOTE : BASE);
            assertFalse(outputToRouter);
        }
        assertEq(baibaiExecutor.fundsExpectedAddress(""), address(this));
    }

    /// @dev Rejects truncation, extra bytes, nonboolean direction and invalid base tokens.
    function testInvalidData() public {
        bytes[5] memory invalid = [
            bytes(""),
            abi.encodePacked(BASE),
            abi.encodePacked(BASE, uint16(1)),
            abi.encodePacked(BASE, uint8(2)),
            abi.encodePacked(QUOTE, uint8(0))
        ];
        for (uint256 i; i < invalid.length; ++i) {
            vm.expectRevert(BaibaiExecutor.InvalidData.selector);
            baibaiExecutor.getTransferData(invalid[i]);
        }
        vm.expectRevert(BaibaiExecutor.InvalidData.selector);
        baibaiExecutor.getTransferData(abi.encodePacked(address(0), uint8(1)));
    }

    /// @dev A fee change cannot silently execute against a gross native quote.
    function testNonzeroTakerFeeReverts() public {
        vm.mockCall(
            BAIBAI_ENTRYPOINT,
            abi.encodeCall(
                IBaibaiEntrypoint.takerFeeBps, (address(baibaiExecutor), BASE)
            ),
            abi.encode(uint16(1))
        );
        vm.expectRevert(BaibaiExecutor.NonzeroTakerFee.selector);
        baibaiExecutor.swap(1e16, abi.encodePacked(BASE, uint8(1)), BOB);
    }

    function _swap(bool sellBase, uint256 amount) internal {
        address input = sellBase ? BASE : QUOTE;
        address output = sellBase ? QUOTE : BASE;
        uint256 expected = IBaibaiCurveQuote(BOOK).quote(BASE, input, amount);
        assertGt(expected, 0);
        deal(input, BOB, amount);
        uint256 beforeBalance = IERC20(output).balanceOf(BOB);
        uint256 custodyBefore = IERC20(input).balanceOf(CUSTODIAN);
        vm.startPrank(BOB);
        IERC20(input).approve(tychoRouterAddr, amount);
        bytes memory swap = encodeSingleSwap(
            address(baibaiExecutor),
            abi.encodePacked(BASE, uint8(sellBase ? 1 : 0))
        );
        uint256 beforeGas = gasleft();
        uint256 received = tychoRouter.singleSwap(
            amount, input, output, expected, 1, BOB, noClientFee(), swap
        );
        emit log_named_uint("BaiBai router swap gas", beforeGas - gasleft());
        vm.stopPrank();
        assertEq(received, expected);
        assertEq(IERC20(output).balanceOf(BOB) - beforeBalance, expected);
        assertEq(IERC20(input).balanceOf(BOB), 0);
        assertEq(IERC20(input).balanceOf(CUSTODIAN) - custodyBefore, amount);
        assertEq(IERC20(input).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(output).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(input).allowance(tychoRouterAddr, BAIBAI_ENTRYPOINT), 0);
    }

    /// @dev Both directions pass through the real router, entrypoint and custodian.
    function testRouterBothDirections() public {
        _swap(true, 1e16);
        _swap(false, 10e6);
    }

    /// @dev Exercises all twelve knot storage slots on each side.
    function testRouterMaxKnots() public {
        _curve(4);
        _swap(true, 3.5 ether);
        _swap(false, 8700e6);
    }

    /// @dev Expiry reverts atomically before any custody balance or router approval persists.
    function testExpiredSwapReverts() public {
        _curve(5);
        _assertSellReverts();
    }

    /// @dev Under delegatecall the router, rather than the executor, is the fee identity.
    function testRouterFeeIdentity() public {
        vm.mockCall(
            BAIBAI_ENTRYPOINT,
            abi.encodeCall(
                IBaibaiEntrypoint.takerFeeBps, (tychoRouterAddr, BASE)
            ),
            abi.encode(uint16(1))
        );
        _assertSellReverts();
    }

    function _assertSellReverts() internal {
        deal(BASE, BOB, 1e16);
        vm.startPrank(BOB);
        IERC20(BASE).approve(tychoRouterAddr, 1e16);
        bytes memory swap = encodeSingleSwap(
            address(baibaiExecutor), abi.encodePacked(BASE, uint8(1))
        );
        uint256 custodyBefore = IERC20(BASE).balanceOf(CUSTODIAN);
        vm.expectRevert();
        tychoRouter.singleSwap(
            1e16, BASE, QUOTE, 1, 1, BOB, noClientFee(), swap
        );
        vm.stopPrank();
        assertEq(IERC20(BASE).balanceOf(BOB), 1e16);
        assertEq(IERC20(BASE).balanceOf(CUSTODIAN), custodyBefore);
        assertEq(IERC20(BASE).allowance(tychoRouterAddr, BAIBAI_ENTRYPOINT), 0);
    }
}
