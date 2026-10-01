// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {
    ClientFeeForwarder,
    ClientFeeForwarder__InvalidReceiver,
    ClientFeeForwarder__UnexpectedSender
} from "@src/client_fee_forwarder/ClientFeeForwarder.sol";
import {
    ClientFeeParams,
    TychoRouter__InvalidClientSignature
} from "@src/TychoRouterV3.sol";
import {IERC1271} from "@openzeppelin/contracts/interfaces/IERC1271.sol";

contract ClientFeeForwarderTest is TychoRouterTestSetup {
    event ClientFeeForwarded(
        address indexed token, uint256 amount, address indexed feeWallet
    );

    // 1%
    uint32 private constant _CLIENT_FEE_BPS = 1_000_000;

    // 1 WETH buys 2018817438608734439722 DAI on the USV2 pool. The client's 1%
    // is 20188174386087344397 DAI and ALICE receives the rest.
    uint256 private constant _DAI_AMOUNT_OUT = 2018817438608734439722;
    uint256 private constant _DAI_CLIENT_FEE = 20188174386087344397;
    uint256 private constant _DAI_TO_ALICE = 1998629264222647095325;

    address private _feeWallet = makeAddr("clientFeeWallet");
    ClientFeeForwarder private _forwarder;

    function setUp() public override {
        super.setUp();
        _forwarder = new ClientFeeForwarder(
            tychoRouterAddr, _feeWallet, _CLIENT_FEE_BPS
        );
    }

    function _wethDaiSwap() private view returns (bytes memory) {
        return encodeSingleSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );
    }

    function testSingleSwapForwardsClientFee() public {
        deal(WETH_ADDR, ALICE, 1 ether);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(address(_forwarder), 1 ether);

        vm.expectEmit();
        emit ClientFeeForwarded(DAI_ADDR, _DAI_CLIENT_FEE, _feeWallet);
        uint256 amountOut = _forwarder.singleSwap(
            1 ether,
            WETH_ADDR,
            DAI_ADDR,
            _DAI_AMOUNT_OUT,
            _DAI_AMOUNT_OUT * 9500 / 10000,
            ALICE,
            _wethDaiSwap()
        );
        vm.stopPrank();

        assertEq(amountOut, _DAI_TO_ALICE);
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), _DAI_TO_ALICE);
        assertEq(IERC20(DAI_ADDR).balanceOf(_feeWallet), _DAI_CLIENT_FEE);
        assertEq(IERC20(DAI_ADDR).balanceOf(address(_forwarder)), 0);
        assertEq(
            tychoRouter.balanceOf(
                address(_forwarder), uint256(uint160(DAI_ADDR))
            ),
            0
        );
        assertEq(IERC20(WETH_ADDR).balanceOf(address(_forwarder)), 0);
    }

    function testSingleSwapForwardsRouterShareOfClientFee() public {
        // The router keeps 10% of the client fee; the fee wallet gets the rest.
        vm.prank(FEE_SETTER);
        feeCalculator.setRouterFeeOnClientFee(10_000_000);

        deal(WETH_ADDR, ALICE, 1 ether);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(address(_forwarder), 1 ether);
        _forwarder.singleSwap(
            1 ether,
            WETH_ADDR,
            DAI_ADDR,
            _DAI_AMOUNT_OUT,
            _DAI_AMOUNT_OUT * 9500 / 10000,
            ALICE,
            _wethDaiSwap()
        );
        vm.stopPrank();

        uint256 routerShare = 2018817438608734439;
        assertEq(
            IERC20(DAI_ADDR).balanceOf(_feeWallet),
            _DAI_CLIENT_FEE - routerShare
        );
        assertEq(
            tychoRouter.balanceOf(
                routerFeeReceiver, uint256(uint160(DAI_ADDR))
            ),
            routerShare
        );
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), _DAI_TO_ALICE);
    }

    function testSingleSwapForwardsNativeEthFee() public {
        // WETH -> ETH unwrap: the fee arrives as native ETH.
        deal(WETH_ADDR, ALICE, 1 ether);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(address(_forwarder), 1 ether);
        _forwarder.singleSwap(
            1 ether,
            WETH_ADDR,
            ETH_ADDR,
            1 ether,
            0.98 ether,
            ALICE,
            encodeSingleSwap(
                address(nativeWrapExecutor), abi.encodePacked(uint8(0))
            )
        );
        vm.stopPrank();

        assertEq(_feeWallet.balance, 0.01 ether);
        assertEq(ALICE.balance, 0.99 ether);
        assertEq(address(_forwarder).balance, 0);
    }

    function testSingleSwapWithNativeEthInput() public {
        // ETH -> WETH wrap, funded with msg.value.
        deal(ALICE, 1 ether);
        vm.prank(ALICE);
        _forwarder.singleSwap{value: 1 ether}(
            1 ether,
            ETH_ADDR,
            WETH_ADDR,
            1 ether,
            0.98 ether,
            ALICE,
            encodeSingleSwap(
                address(nativeWrapExecutor), abi.encodePacked(uint8(1))
            )
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(_feeWallet), 0.01 ether);
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), 0.99 ether);
        assertEq(ALICE.balance, 0);
    }

    function testSingleSwapFromRustCalldata() public {
        deal(WETH_ADDR, ALICE, 1 ether);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(address(_forwarder), 1 ether);
        (bool success,) = address(_forwarder)
            .call(loadCallDataFromFile("test_client_fee_forwarder_single_swap"));
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), _DAI_TO_ALICE);
        assertEq(IERC20(DAI_ADDR).balanceOf(_feeWallet), _DAI_CLIENT_FEE);
    }

    function testRouterRejectsForwarderAsClientOutsideItsCall() public {
        // Anyone can name the forwarder as clientFeeReceiver, but the forwarder
        // only signs during its own router call.
        bytes memory swap = _wethDaiSwap();
        ClientFeeParams memory feeParams = ClientFeeParams({
            clientFeeBps: _CLIENT_FEE_BPS,
            clientFeeReceiver: address(_forwarder),
            maxClientContribution: 0,
            deadline: block.timestamp,
            clientSignature: ""
        });

        deal(WETH_ADDR, ALICE, 1 ether);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, 1 ether);
        vm.expectRevert(TychoRouter__InvalidClientSignature.selector);
        tychoRouter.singleSwap(
            1 ether,
            WETH_ADDR,
            DAI_ADDR,
            _DAI_AMOUNT_OUT,
            _DAI_AMOUNT_OUT * 9500 / 10000,
            ALICE,
            feeParams,
            swap
        );
        vm.stopPrank();
    }

    function testIsValidSignatureRejectsOutsideSwap() public {
        vm.prank(tychoRouterAddr);
        bytes4 result = _forwarder.isValidSignature(bytes32(0), "");
        assertTrue(result != IERC1271.isValidSignature.selector);
    }

    function testRevertsWhenReceiverIsRouter() public {
        // The router would credit ALICE's output to the forwarder's vault
        // balance, and the forwarder would send it to the fee wallet.
        bytes memory swap = _wethDaiSwap();
        vm.expectRevert(
            abi.encodeWithSelector(
                ClientFeeForwarder__InvalidReceiver.selector, tychoRouterAddr
            )
        );
        _forwarder.singleSwap(
            1 ether,
            WETH_ADDR,
            DAI_ADDR,
            _DAI_AMOUNT_OUT,
            _DAI_AMOUNT_OUT * 9500 / 10000,
            tychoRouterAddr,
            swap
        );
    }

    function testRevertsWhenReceiverIsForwarder() public {
        bytes memory swap = _wethDaiSwap();
        vm.expectRevert(
            abi.encodeWithSelector(
                ClientFeeForwarder__InvalidReceiver.selector,
                address(_forwarder)
            )
        );
        _forwarder.singleSwap(
            1 ether,
            WETH_ADDR,
            DAI_ADDR,
            _DAI_AMOUNT_OUT,
            _DAI_AMOUNT_OUT * 9500 / 10000,
            address(_forwarder),
            swap
        );
    }

    function testReceiveRejectsOtherSenders() public {
        deal(ALICE, 1 ether);
        vm.prank(ALICE);
        (bool success, bytes memory data) =
            address(_forwarder).call{value: 1 ether}("");
        assertFalse(success);
        assertEq(
            data,
            abi.encodeWithSelector(
                ClientFeeForwarder__UnexpectedSender.selector, ALICE
            )
        );
    }
}
