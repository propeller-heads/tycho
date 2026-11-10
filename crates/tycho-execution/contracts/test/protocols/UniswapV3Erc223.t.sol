// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {Dispatcher__UnapprovedExecutor} from "@src/Dispatcher.sol";
import {ClientFeeParams} from "@src/TychoRouterV3.sol";

/// @dev DEP (DEAPCoin) is an ERC223 token: `transfer` calls `tokenFallback`
///  on a contract recipient. The pool pays DEP out with `transfer` before it
///  calls the swap callback.
contract UniswapV3Erc223Test is TychoRouterTestSetup {
    address private constant _DEP = 0x1A3496C18d558bd9C6C8f609E1B129f67AB08163;
    address private constant _DEP_USDC_POOL =
        0xa5c75832790748e996FC139917F6Dc4B9F23C31E;
    uint256 private constant _AMOUNT_IN = 100_000000;
    // ALICE has EIP-7702 code at this block, so DEP calls tokenFallback on her.
    address private immutable _RECEIVER = makeAddr("depReceiver");

    function getForkBlock() public view override returns (uint256) {
        return 26380000;
    }

    function _encodeDepSwap(bool outputViaExecutor)
        private
        view
        returns (bytes memory)
    {
        bytes memory protocolData =
            encodeUniswapV3Swap(USDC_ADDR, _DEP, _DEP_USDC_POOL, false);
        if (outputViaExecutor) {
            protocolData = abi.encodePacked(protocolData, true);
        }
        return encodeSingleSwap(address(usv3Executor), protocolData);
    }

    function _clientFee(bytes memory swap)
        private
        view
        returns (ClientFeeParams memory)
    {
        return makeClientFeeParams(
            1,
            0,
            _AMOUNT_IN,
            USDC_ADDR,
            _DEP,
            1,
            1,
            _RECEIVER,
            swap,
            tychoRouterAddr,
            CLIENT_FEE_RECEIVER_PK
        );
    }

    function _swapAsAlice(ClientFeeParams memory feeParams, bytes memory swap)
        private
        returns (uint256 amountOut)
    {
        deal(USDC_ADDR, ALICE, _AMOUNT_IN);
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, _AMOUNT_IN);
        amountOut = tychoRouter.singleSwap(
            _AMOUNT_IN, USDC_ADDR, _DEP, 1, 1, _RECEIVER, feeParams, swap
        );
        vm.stopPrank();
    }

    function testDepOutputToRouterRevertsWithoutFlag() public {
        bytes memory swap = _encodeDepSwap(false);
        ClientFeeParams memory feeParams = _clientFee(swap);

        deal(USDC_ADDR, ALICE, _AMOUNT_IN);
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, _AMOUNT_IN);
        vm.expectRevert(
            abi.encodeWithSelector(
                Dispatcher__UnapprovedExecutor.selector, address(0)
            )
        );
        tychoRouter.singleSwap(
            _AMOUNT_IN, USDC_ADDR, _DEP, 1, 1, _RECEIVER, feeParams, swap
        );
        vm.stopPrank();
    }

    function testDepOutputToRouterWithFlag() public {
        bytes memory swap = _encodeDepSwap(true);

        uint256 amountOut = _swapAsAlice(_clientFee(swap), swap);

        assertGt(amountOut, 0);
        assertEq(IERC20(_DEP).balanceOf(_RECEIVER), amountOut);
        assertGt(
            tychoRouter.balanceOf(clientFeeReceiver, uint256(uint160(_DEP))), 0
        );
        assertEq(IERC20(_DEP).balanceOf(address(usv3Executor)), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), 0);
    }

    function testDepOutputToUserWithoutFlag() public {
        uint256 amountOut = _swapAsAlice(noClientFee(), _encodeDepSwap(false));

        assertEq(IERC20(_DEP).balanceOf(_RECEIVER), amountOut);
    }

    function testDepOutputToUserWithFlag() public {
        uint256 amountOutWithoutFlag;
        {
            uint256 snapshot = vm.snapshotState();
            amountOutWithoutFlag =
                _swapAsAlice(noClientFee(), _encodeDepSwap(false));
            vm.revertToState(snapshot);
        }

        uint256 amountOut = _swapAsAlice(noClientFee(), _encodeDepSwap(true));

        assertEq(amountOut, amountOutWithoutFlag);
        assertEq(IERC20(_DEP).balanceOf(_RECEIVER), amountOut);
        assertEq(IERC20(_DEP).balanceOf(address(usv3Executor)), 0);
    }
}
