// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {TychoRouter__InvalidReceiver} from "@src/TychoRouterV3.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    IAllowanceTransfer
} from "@permit2/src/interfaces/IAllowanceTransfer.sol";
import {
    IUniswapV2Pair
} from "@uniswap-v2/contracts/interfaces/IUniswapV2Pair.sol";
import "./TychoRouterTestSetup.sol";

/**
 * @title TychoRouterChecksTest
 * @notice Entry checks of the router, and swaps that must leave the router's
 *         own balance untouched
 */
contract TychoRouterChecksTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 24480104;
    }

    function testMsgValueDoesNotMatchAmountIn() public {
        // Alice sends 1 ETH less than her declared input amount - revert.
        uint256 amountIn = 1 ether;
        deal(ALICE, amountIn);

        bytes memory swap = new bytes(0); // IRRELEVANT - should fail before this

        vm.startPrank(ALICE);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__MsgValueDoesNotMatchAmountIn.selector,
                amountIn - 1,
                amountIn
            )
        );
        tychoRouter.singleSwap{value: amountIn - 1}(
            amountIn,
            ETH_ADDR,
            DAI_ADDR,
            1, // expected amount out
            1, // min amount out
            ALICE, // receiver
            noClientFee(),
            swap
        );
        vm.stopPrank();
    }

    function testMsgValueWithErc20Input() public {
        // Alice sends ETH with a WETH input. The router would keep the ETH, so
        // it reverts.
        uint256 amountIn = 1 ether;
        deal(ALICE, amountIn);
        deal(WETH_ADDR, ALICE, amountIn);

        bytes memory swap = encodeSingleSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__MsgValueDoesNotMatchAmountIn.selector, amountIn, 0
            )
        );
        tychoRouter.singleSwap{value: amountIn}(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            1, // expected amount out
            1, // min amount out
            ALICE, // receiver
            noClientFee(),
            swap
        );
        vm.stopPrank();
    }

    function testNativeInputWithoutMsgValue() public {
        // Alice declares an ETH input but sends no ETH. The swap would spend
        // ETH the router holds, so it reverts.
        uint256 amountIn = 1 ether;
        deal(tychoRouterAddr, amountIn);

        vm.startPrank(ALICE);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__MsgValueDoesNotMatchAmountIn.selector, 0, amountIn
            )
        );
        tychoRouter.singleSwap(
            amountIn,
            ETH_ADDR,
            RETH_ADDR,
            1, // expected amount out
            1, // min amount out
            ALICE, // receiver
            noClientFee(),
            _rocketpoolEthRethSwap()
        );
        vm.stopPrank();

        assertEq(tychoRouterAddr.balance, amountIn);
    }

    function testPermit2NativeInput() public {
        // The Permit2 methods are not payable, so a native ETH input would
        // spend ETH the router holds. They revert instead.
        uint256 amountIn = 1 ether;
        deal(tychoRouterAddr, amountIn);
        IAllowanceTransfer.PermitSingle memory permitSingle;

        vm.startPrank(ALICE);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__MsgValueDoesNotMatchAmountIn.selector, 0, amountIn
            )
        );
        tychoRouter.singleSwapPermit2(
            amountIn,
            ETH_ADDR,
            RETH_ADDR,
            1, // expected amount out
            1, // min amount out
            ALICE, // receiver
            noClientFee(),
            permitSingle,
            "",
            _rocketpoolEthRethSwap()
        );
        vm.stopPrank();

        assertEq(tychoRouterAddr.balance, amountIn);
    }

    function testReceiverIsRouter() public {
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        bytes memory swap = encodeSingleSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__InvalidReceiver.selector, tychoRouterAddr
            )
        );
        tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            1, // expected amount out
            1, // min amount out
            tychoRouterAddr, // receiver
            noClientFee(),
            swap
        );
        vm.stopPrank();
    }

    function _rocketpoolEthRethSwap() private view returns (bytes memory swap) {
        swap = encodeSingleSwap(
            address(rocketpoolExecutor),
            abi.encodePacked(
                uint8(1) // isDeposit = true
            )
        );
    }

    function testTransferNativeInExecutorUserSentETH() public {
        // ETH -> rETH via Rocketpool deposit, with the ETH sent as msg.value.
        // The router holds ETH it does not owe to this swap.
        uint256 amountIn = 1 ether;
        uint256 routerEthBalance = 2 ether;
        deal(ALICE, amountIn);
        deal(tychoRouterAddr, routerEthBalance);

        vm.startPrank(ALICE);
        uint256 amountOut = tychoRouter.singleSwap{value: amountIn}(
            amountIn,
            ETH_ADDR,
            RETH_ADDR,
            1, // expected amount out
            1, // min amount out
            ALICE, // receiver
            noClientFee(),
            _rocketpoolEthRethSwap()
        );
        vm.stopPrank();

        assertEq(amountOut, 863321713651918410);
        assertEq(IERC20(RETH_ADDR).balanceOf(ALICE), amountOut);
        assertEq(tychoRouterAddr.balance, routerEthBalance);
    }

    function testSequentialSwapNativeETHIntermediate() public {
        // Output of first swap is native ETH. Second swap uses that ETH for a
        // native ETH input swap without touching the ETH the router already holds.
        // Sequential swap: USDC --(USV4)--> ETH --(rocket)--> rETH
        bytes[] memory swaps = new bytes[](2);

        bytes memory pool = abi.encodePacked(
            ETH_ADDR, // intermediary token (ETH_ADDRESS)
            bytes3(uint24(3000)), // fee
            int24(60), // tick spacing
            address(0), // hook
            bytes2(uint16(0)), // hook data length
            bytes("") // hook data
        );

        bytes memory protocolData = abi.encodePacked(
            USDC_ADDR,
            ETH_ADDR, // ETH_ADDRESS
            false, // zeroForOne
            false,
            pool
        );

        swaps[0] = encodeSingleSwap(address(usv4Executor), protocolData);
        swaps[1] = _rocketpoolEthRethSwap();

        uint256 amountIn = 1000e6; // 1000 USDC
        uint256 routerEthBalance = 3 ether;

        deal(USDC_ADDR, ALICE, amountIn);
        deal(tychoRouterAddr, routerEthBalance);

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);

        uint256 amountOut = tychoRouter.sequentialSwap(
            amountIn,
            USDC_ADDR,
            RETH_ADDR,
            1, // expected amount out
            1, // min amount out
            ALICE, // receiver
            noClientFee(),
            pleEncode(swaps)
        );
        vm.stopPrank();

        assertEq(amountOut, 431378612147468451);
        assertEq(IERC20(RETH_ADDR).balanceOf(ALICE), amountOut);
        assertEq(tychoRouterAddr.balance, routerEthBalance);
    }

    function testCyclicalSwapTransferFrom() public {
        // Profitable arbitrage: USDC -> WETH (USV3) -> USDC (USV2).
        // Extra USDC in the USV2 pool makes WETH worth more USDC there.
        uint256 amountIn = 1_000_000_000; // 1000 USDC
        uint256 poolAmountOut = 1050886787; // 1050.8 USDC

        deal(
            USDC_ADDR,
            USDC_WETH_USV2,
            IERC20(USDC_ADDR).balanceOf(USDC_WETH_USV2) + 500_000 * 1e6
        );
        IUniswapV2Pair(USDC_WETH_USV2).sync();

        deal(USDC_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(address(tychoRouterAddr), amountIn);

        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(
            address(usv3Executor),
            encodeUniswapV3Swap(USDC_ADDR, WETH_ADDR, USDC_WETH_USV3, true)
        );
        swaps[1] = encodeSequentialSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(USDC_WETH_USV2, WETH_ADDR, USDC_ADDR)
        );

        uint256 amountOut = tychoRouter.sequentialSwap(
            amountIn,
            USDC_ADDR,
            USDC_ADDR,
            amountIn, // expected amount out
            amountIn * 9800 / 10000, // min amount out
            ALICE, // receiver
            noClientFee(),
            pleEncode(swaps)
        );
        vm.stopPrank();

        assertEq(amountOut, poolAmountOut);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), poolAmountOut);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}
