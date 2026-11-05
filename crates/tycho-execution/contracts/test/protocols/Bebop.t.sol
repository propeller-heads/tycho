pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/BebopExecutor.sol";
import {Constants} from "../Constants.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {Permit2TestHelper} from "../Permit2TestHelper.sol";
import {
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @dev BebopSettlement.swapSingle
bytes4 constant SWAP_SINGLE_SELECTOR = 0x4dcebcba;
/// @dev BebopSettlement.swapAggregate
bytes4 constant SWAP_AGGREGATE_SELECTOR = 0xa2f74893;
/// @dev BebopRouter.swap
bytes4 constant ROUTER_SWAP_SELECTOR = 0x9586d0e8;

contract MockBebopSettlement {
    using SafeERC20 for IERC20;

    IERC20 private constant _TOKEN_IN =
        IERC20(0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2);
    IERC20 private constant _TOKEN_OUT =
        IERC20(0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48);
    uint256 private constant _AMOUNT_OUT = 10_000_000;

    uint256 public filledTakerAmount;

    fallback() external {
        require(msg.sig == SWAP_AGGREGATE_SELECTOR, "unexpected selector");
        filledTakerAmount = uint256(bytes32(msg.data[68:100]));
        _TOKEN_IN.safeTransferFrom(msg.sender, address(this), filledTakerAmount);
        _TOKEN_OUT.safeTransfer(msg.sender, _AMOUNT_OUT);
    }
}

contract BebopExecutorExposed is BebopExecutor {
    constructor(address _bebopSettlement, address _bebopRouter)
        BebopExecutor(_bebopSettlement, _bebopRouter)
    {}

    function decodeData(bytes calldata data)
        external
        pure
        returns (
            address target,
            uint8 partialFillOffset,
            uint256 originalFilledTakerAmount,
            bytes memory bebopCalldata
        )
    {
        return _decodeData(data);
    }
}

contract BebopExecutorTest is Constants, Permit2TestHelper, TestUtils {
    using SafeERC20 for IERC20;

    BebopExecutorExposed bebopExecutor;

    IERC20 weth = IERC20(WETH_ADDR);
    IERC20 usdc = IERC20(USDC_ADDR);
    IERC20 dai = IERC20(DAI_ADDR);
    IERC20 wbtc = IERC20(WBTC_ADDR);
    IERC20 ondo = IERC20(ONDO_ADDR);
    IERC20 usdt = IERC20(USDT_ADDR);

    function testDecodeData() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 22667985);
        bebopExecutor = new BebopExecutorExposed(BEBOP_SETTLEMENT, BEBOP_ROUTER);

        bytes memory bebopCalldata = abi.encodePacked(
            SWAP_SINGLE_SELECTOR,
            hex"00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000068470140"
        );

        uint256 originalAmountIn = 200000000; // 200 USDC
        bytes memory params = abi.encodePacked(
            USDC_ADDR,
            ONDO_ADDR,
            BEBOP_SETTLEMENT,
            uint8(2),
            originalAmountIn,
            bebopCalldata
        );

        (
            address decodedTarget,
            uint8 decodedPartialFillOffset,
            uint256 decodedOriginalAmountIn,
            bytes memory decodedBebopCalldata
        ) = bebopExecutor.decodeData(params);

        assertEq(decodedTarget, BEBOP_SETTLEMENT, "target mismatch");
        assertEq(
            keccak256(decodedBebopCalldata),
            keccak256(bebopCalldata),
            "bebopCalldata mismatch"
        );
        assertEq(decodedPartialFillOffset, 2, "partialFillOffset mismatch");
        assertEq(
            decodedOriginalAmountIn,
            originalAmountIn,
            "originalAmountIn mismatch"
        );
    }

    function testGetTransferData() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 22667985);
        bebopExecutor = new BebopExecutorExposed(BEBOP_SETTLEMENT, BEBOP_ROUTER);

        bytes memory bebopCalldata = abi.encodePacked(
            SWAP_SINGLE_SELECTOR,
            hex"00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000068470140"
        );

        uint256 originalAmountIn = 200000000; // 200 USDC
        bytes memory params = abi.encodePacked(
            USDC_ADDR,
            ONDO_ADDR,
            BEBOP_SETTLEMENT,
            uint8(2),
            originalAmountIn,
            bebopCalldata
        );

        (
            TransferManager.TransferType transferType,
            address decodedReceiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = bebopExecutor.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit),
            "transferType mismatch"
        );
        assertEq(decodedReceiver, BEBOP_SETTLEMENT, "receiver mismatch");
        assertEq(tokenIn, USDC_ADDR, "tokenIn mismatch");
        assertEq(tokenOut, ONDO_ADDR, "tokenOut mismatch");
        assertEq(outputToRouter, true, "outputToRouter mismatch");
    }

    // Single Order Tests
    function testSingleOrder() public {
        // 1 weth -> wbtc
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);

        bebopExecutor = new BebopExecutorExposed(BEBOP_SETTLEMENT, BEBOP_ROUTER);

        // Quote made manually using the BebopExecutor as the taker and receiver
        bytes memory bebopCalldata =
            hex"4dcebcba00000000000000000000000000000000000000000000000000000000689b137a0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f000000000000000000000000bee3211ab312a8d065c4fef0247448e17a8da000000000000000000000000000000000000000000000000000279ead5d9683d8a5000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc20000000000000000000000002260fac5e5542a773aa44fbcfedf7c193bc2c5990000000000000000000000000000000000000000000000000de0b6b3a7640000000000000000000000000000000000000000000000000000000000000037337c0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f0000000000000000000000000000000000000000000000000000000000000000f71248bc6c123bbf12adc837470f75640000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000418e9b0fb72ed9b86f7a7345026269c02b9056efcdfb67a377c7ff6c4a62a4807a7671ae759edf29aea1b2cb8efc8659e3aedac72943cd3607985a1849256358641c00000000000000000000000000000000000000000000000000000000000000";
        address tokenIn = WETH_ADDR;
        address tokenOut = WBTC_ADDR;
        uint8 partialFillOffset = 12;
        uint256 amountIn = 1000000000000000000;
        uint256 expectedAmountOut = 3617660;

        deal(tokenIn, address(bebopExecutor), amountIn);

        bytes memory params = abi.encodePacked(
            tokenIn,
            tokenOut,
            BEBOP_SETTLEMENT,
            partialFillOffset,
            amountIn,
            bebopCalldata
        );

        uint256 initialTokenOutBalance =
            IERC20(tokenOut).balanceOf(address(bebopExecutor));
        vm.prank(address(bebopExecutor));
        IERC20(tokenIn).approve(BEBOP_SETTLEMENT, amountIn);
        bebopExecutor.swap(amountIn, params, address(bebopExecutor));

        assertEq(
            IERC20(tokenOut).balanceOf(address(bebopExecutor))
                - initialTokenOutBalance,
            expectedAmountOut,
            "wbtc should be at receiver"
        );
        assertEq(
            IERC20(tokenIn).balanceOf(address(bebopExecutor)),
            0,
            "weth left in executor"
        );
    }

    function testSingleOrder_PartialFill() public {
        // 0.5 weth -> wbtc with a quote for 1 weth
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23124275);

        bebopExecutor = new BebopExecutorExposed(BEBOP_SETTLEMENT, BEBOP_ROUTER);

        // Quote made manually using the BebopExecutor as the taker and receiver (the same as testSingleOrder)
        bytes memory bebopCalldata =
            hex"4dcebcba00000000000000000000000000000000000000000000000000000000689b137a0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f000000000000000000000000bee3211ab312a8d065c4fef0247448e17a8da000000000000000000000000000000000000000000000000000279ead5d9683d8a5000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc20000000000000000000000002260fac5e5542a773aa44fbcfedf7c193bc2c5990000000000000000000000000000000000000000000000000de0b6b3a7640000000000000000000000000000000000000000000000000000000000000037337c0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f0000000000000000000000000000000000000000000000000000000000000000f71248bc6c123bbf12adc837470f75640000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000418e9b0fb72ed9b86f7a7345026269c02b9056efcdfb67a377c7ff6c4a62a4807a7671ae759edf29aea1b2cb8efc8659e3aedac72943cd3607985a1849256358641c00000000000000000000000000000000000000000000000000000000000000";
        address tokenIn = WETH_ADDR;
        address tokenOut = WBTC_ADDR;
        uint8 partialFillOffset = 12;
        // filling only half of the quote
        uint256 amountIn = 1000000000000000000 / 2;
        uint256 expectedAmountOut = 3617660 / 2;

        deal(tokenIn, address(bebopExecutor), amountIn);

        bytes memory params = abi.encodePacked(
            tokenIn,
            tokenOut,
            BEBOP_SETTLEMENT,
            partialFillOffset,
            amountIn * 2, // this is the original amount in
            bebopCalldata
        );

        uint256 initialTokenOutBalance =
            IERC20(tokenOut).balanceOf(address(bebopExecutor));
        vm.prank(address(bebopExecutor));
        IERC20(tokenIn).approve(BEBOP_SETTLEMENT, amountIn);
        bebopExecutor.swap(amountIn, params, address(bebopExecutor));

        assertEq(
            IERC20(tokenOut).balanceOf(address(bebopExecutor))
                - initialTokenOutBalance,
            expectedAmountOut,
            "weth should be at receiver"
        );
        assertEq(
            IERC20(tokenIn).balanceOf(address(bebopExecutor)),
            0,
            "wbtc left in executor"
        );
    }

    // Aggregate Order Tests
    function testAggregateOrder() public {
        // 20k usdc -> ondo
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23126278);
        bebopExecutor = new BebopExecutorExposed(BEBOP_SETTLEMENT, BEBOP_ROUTER);

        // Quote made manually using the BebopExecutor as the taker and receiver
        bytes memory bebopCalldata =
            hex"a2f7489300000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000640000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000689b715d0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f000000000000000000000000000000000000000000000000000000000000016000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000220000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000000000000000000000000000003e000000000000000000000000000000000000000000000000000000000000004c00000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f00000000000000000000000000000000000000000000000000000000000005a0e0c07568b14a2d2c1b4d196000fc12bc00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000000000000051c72848c68a965f66fa7a88855f9f7784502a7f000000000000000000000000ce79b081c0c924cb67848723ed3057234d10fc6b00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000002a65384e777abcfe0000000000000000000000000000000000000000000000002a65384e777abcff0000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000001000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb480000000000000000000000000000000000000000000000000000000000000001000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb480000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000001000000000000000000000000faba6f8e4a5e8ab82f62fe7c39859fa577269be30000000000000000000000000000000000000000000000000000000000000001000000000000000000000000faba6f8e4a5e8ab82f62fe7c39859fa577269be300000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000236ddb7a7000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000002713a105900000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000001e7dc63f0c1d9d93df4000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000021960567af238bcfd0000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000041275c4b7c3df4bfa5c33da3443d817cc6ab568ec8b0fddc30445adff2e870cdcd7d8738e23b795c2fb1ee112e12716bcef1cf648bd1ded17ef10ae493d687322e1b0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004187ef3d632a640b09df5f39b2fb4c5b9afb7ab4f2782fee450b17e2363d27303b45ec55b154a63993106bfc28bb4accc10fb40f7927509fed554fac01a5d88bae1c00000000000000000000000000000000000000000000000000000000000000";
        address tokenIn = USDC_ADDR;
        address tokenOut = ONDO_ADDR;
        uint8 partialFillOffset = 2;
        // filling only half of the quote
        uint256 amountIn = 20000000000;
        // maker amounts from quote
        uint256 expectedAmountOut =
            (8999445165322964385268 + 9912843438638420000000);

        deal(tokenIn, address(bebopExecutor), amountIn);

        bytes memory params = abi.encodePacked(
            tokenIn,
            tokenOut,
            BEBOP_SETTLEMENT,
            partialFillOffset,
            amountIn,
            bebopCalldata
        );

        uint256 initialTokenOutBalance =
            IERC20(tokenOut).balanceOf(address(bebopExecutor));

        vm.prank(address(bebopExecutor));
        IERC20(tokenIn).approve(BEBOP_SETTLEMENT, amountIn);
        bebopExecutor.swap(amountIn, params, address(bebopExecutor));

        assertEq(
            IERC20(tokenOut).balanceOf(address(bebopExecutor))
                - initialTokenOutBalance,
            expectedAmountOut,
            "ondo should be at receiver"
        );
        assertEq(
            IERC20(tokenIn).balanceOf(address(bebopExecutor)),
            0,
            "usdc left in executor"
        );
    }

    function testAggregateOrder_PartialFill() public {
        // 10k usdc -> ondo with a quote for 20k usdc
        vm.createSelectFork(vm.rpcUrl("mainnet"), 23126278);
        bebopExecutor = new BebopExecutorExposed(BEBOP_SETTLEMENT, BEBOP_ROUTER);

        // Quote made manually using the BebopExecutor as the taker and receiver
        bytes memory bebopCalldata =
            hex"a2f7489300000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000640000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000689b715d0000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f000000000000000000000000000000000000000000000000000000000000016000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000220000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000000000000000000000000000003e000000000000000000000000000000000000000000000000000000000000004c00000000000000000000000005615deb798bb3e4dfa0139dfa1b3d433cc23b72f00000000000000000000000000000000000000000000000000000000000005a0e0c07568b14a2d2c1b4d196000fc12bc00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000000000000051c72848c68a965f66fa7a88855f9f7784502a7f000000000000000000000000ce79b081c0c924cb67848723ed3057234d10fc6b00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000002a65384e777abcfe0000000000000000000000000000000000000000000000002a65384e777abcff0000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000001000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb480000000000000000000000000000000000000000000000000000000000000001000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb480000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000001000000000000000000000000faba6f8e4a5e8ab82f62fe7c39859fa577269be30000000000000000000000000000000000000000000000000000000000000001000000000000000000000000faba6f8e4a5e8ab82f62fe7c39859fa577269be300000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000236ddb7a7000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000002713a105900000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000001e7dc63f0c1d9d93df4000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000021960567af238bcfd0000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000041275c4b7c3df4bfa5c33da3443d817cc6ab568ec8b0fddc30445adff2e870cdcd7d8738e23b795c2fb1ee112e12716bcef1cf648bd1ded17ef10ae493d687322e1b0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004187ef3d632a640b09df5f39b2fb4c5b9afb7ab4f2782fee450b17e2363d27303b45ec55b154a63993106bfc28bb4accc10fb40f7927509fed554fac01a5d88bae1c00000000000000000000000000000000000000000000000000000000000000";
        address tokenIn = USDC_ADDR;
        address tokenOut = ONDO_ADDR;
        uint8 partialFillOffset = 2;
        // filling only half of the quote
        uint256 amountIn = 20000000000 / 2;
        // maker amounts from quote
        uint256 expectedAmountOut =
            (8999445165322964385268 + 9912843438638420000000) / 2;

        deal(tokenIn, address(bebopExecutor), amountIn);

        bytes memory params = abi.encodePacked(
            tokenIn,
            tokenOut,
            BEBOP_SETTLEMENT,
            partialFillOffset,
            amountIn * 2, // this is the original amount from the quote
            bebopCalldata
        );

        uint256 initialTokenOutBalance =
            IERC20(tokenOut).balanceOf(address(bebopExecutor));
        vm.prank(address(bebopExecutor));
        IERC20(tokenIn).approve(BEBOP_SETTLEMENT, amountIn);
        bebopExecutor.swap(amountIn, params, address(bebopExecutor));

        assertEq(
            IERC20(tokenOut).balanceOf(address(bebopExecutor))
                - initialTokenOutBalance,
            expectedAmountOut,
            "ondo should be at receiver"
        );
        assertEq(
            IERC20(tokenIn).balanceOf(address(bebopExecutor)),
            1, // because of integer division, there is 1 usdc left in the executor
            "usdc left in executor"
        );
    }

    function testInvalidDataLength() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), 22667985);
        bebopExecutor = new BebopExecutorExposed(BEBOP_SETTLEMENT, BEBOP_ROUTER);

        // Create a mock bebop calldata
        // An arbitrary selector plus some mock data
        bytes memory bebopCalldata =
            abi.encodePacked(bytes4(0x47fb5891), hex"1234567890abcdef");

        // Create params with correct length first
        uint256 originalAmountIn = 1e18;
        bytes memory validParams = abi.encodePacked(
            WETH_ADDR,
            USDC_ADDR,
            BEBOP_SETTLEMENT,
            uint8(2),
            originalAmountIn,
            bebopCalldata
        );

        // Verify valid params work
        bebopExecutor.decodeData(validParams);

        // In the new format, adding extra bytes at the end doesn't fail
        // because bebopCalldata is variable length at the end
        // So test with extra bytes should not revert
        bytes memory paramsWithExtra = abi.encodePacked(validParams, hex"ff");
        // This should work as the extra byte becomes part of bebopCalldata
        bebopExecutor.decodeData(paramsWithExtra);

        // Try with insufficient data, should fail
        bytes memory tooShortParams = abi.encodePacked(WETH_ADDR, USDC_ADDR);
        // Missing rest of the data

        vm.expectRevert(BebopExecutor.BebopExecutor__InvalidDataLength.selector);
        bebopExecutor.decodeData(tooShortParams);
    }

    function testGetTransferDataRouterTarget() public {
        bebopExecutor = new BebopExecutorExposed(BEBOP_SETTLEMENT, BEBOP_ROUTER);

        bytes memory routerCalldata =
            abi.encodePacked(ROUTER_SWAP_SELECTOR, hex"00");
        bytes memory params = abi.encodePacked(
            USDC_ADDR,
            ONDO_ADDR,
            BEBOP_ROUTER,
            uint8(0),
            uint256(1e6),
            routerCalldata
        );

        (
            TransferManager.TransferType transferType,
            address decodedReceiver,,,
            bool outputToRouter
        ) = bebopExecutor.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit),
            "transferType mismatch"
        );
        assertEq(decodedReceiver, BEBOP_ROUTER, "receiver should be router");
        assertEq(outputToRouter, true, "outputToRouter mismatch");
    }

    function testGetTransferDataInvalidTarget() public {
        bebopExecutor = new BebopExecutorExposed(BEBOP_SETTLEMENT, BEBOP_ROUTER);

        bytes memory params = abi.encodePacked(
            USDC_ADDR,
            ONDO_ADDR,
            address(0xdead),
            uint8(0),
            uint256(1e6),
            abi.encodePacked(SWAP_SINGLE_SELECTOR)
        );

        vm.expectRevert(BebopExecutor.BebopExecutor__InvalidTarget.selector);
        bebopExecutor.getTransferData(params);
    }

    function testSwapInvalidTarget() public {
        bebopExecutor = new BebopExecutorExposed(BEBOP_SETTLEMENT, BEBOP_ROUTER);

        bytes memory params = abi.encodePacked(
            USDC_ADDR,
            ONDO_ADDR,
            address(0xdead),
            uint8(0),
            uint256(1e6),
            abi.encodePacked(SWAP_SINGLE_SELECTOR)
        );

        vm.expectRevert(BebopExecutor.BebopExecutor__InvalidTarget.selector);
        bebopExecutor.swap(1e6, params, address(this));
    }

    function testSwapInvalidSelectorOnSettlement() public {
        bebopExecutor = new BebopExecutorExposed(BEBOP_SETTLEMENT, BEBOP_ROUTER);

        // A selector the settlement does not expose
        bytes memory params = abi.encodePacked(
            USDC_ADDR,
            ONDO_ADDR,
            BEBOP_SETTLEMENT,
            uint8(0),
            uint256(1e6),
            abi.encodePacked(bytes4(0xdeadbeef))
        );

        vm.expectRevert(BebopExecutor.BebopExecutor__InvalidSelector.selector);
        bebopExecutor.swap(1e6, params, address(this));
    }

    function testSwapRouterSelectorOnSettlementReverts() public {
        bebopExecutor = new BebopExecutorExposed(BEBOP_SETTLEMENT, BEBOP_ROUTER);

        // The router selector is not valid when the target is the settlement
        bytes memory params = abi.encodePacked(
            USDC_ADDR,
            ONDO_ADDR,
            BEBOP_SETTLEMENT,
            uint8(0),
            uint256(1e6),
            abi.encodePacked(ROUTER_SWAP_SELECTOR)
        );

        vm.expectRevert(BebopExecutor.BebopExecutor__InvalidSelector.selector);
        bebopExecutor.swap(1e6, params, address(this));
    }
}

contract TychoRouterForBebopTest is TychoRouterTestSetup {
    // Override the fork block for Bebop tests
    function getForkBlock() public pure override returns (uint256) {
        return 24290115;
    }

    function testSingleBebopIntegration() public {
        // The calldata swaps 200 usdc for ondo
        address user = 0xd2068e04Cf586f76EEcE7BA5bEB779D7bB1474A1;
        deal(USDC_ADDR, user, 200000000); // 200 usdc
        uint256 expAmountOut = 582464275842264783022; // Expected ondo amount from quote

        uint256 ondoBefore = IERC20(ONDO_ADDR).balanceOf(user);
        vm.startPrank(user);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_bebop");

        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");

        uint256 ondoReceived = IERC20(ONDO_ADDR).balanceOf(user) - ondoBefore;
        assertEq(ondoReceived, expAmountOut);
        assertEq(
            IERC20(USDC_ADDR).balanceOf(tychoRouterAddr),
            0,
            "usdc left in router"
        );

        vm.stopPrank();
    }

    function testBebopAggregateIntegration() public {
        // The calldata swaps 20k usdc for ondo using multiple market makers
        address user = 0xd2068e04Cf586f76EEcE7BA5bEB779D7bB1474A1;
        deal(USDC_ADDR, user, 20000000000); // 20k usdc
        uint256 expAmountOut = 58302581300158475047842; // Expected ondo amount from quote

        uint256 ondoBefore = IERC20(ONDO_ADDR).balanceOf(user);
        vm.startPrank(user);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_bebop_aggregate"
        );

        (bool success,) = tychoRouterAddr.call(callData);

        assertTrue(success, "Call Failed");

        uint256 ondoReceived = IERC20(ONDO_ADDR).balanceOf(user) - ondoBefore;
        assertEq(ondoReceived, expAmountOut);
        assertEq(
            IERC20(USDC_ADDR).balanceOf(tychoRouterAddr),
            0,
            "usdc left in router"
        );

        vm.stopPrank();
    }

    function testBebopPartialFillThroughRouter() public {
        address user = 0xd2068e04Cf586f76EEcE7BA5bEB779D7bB1474A1;
        uint256 runtimeAmountIn = 10 ether;
        uint256 expectedAmountOut = 10_000_000;
        vm.etch(BEBOP_SETTLEMENT, type(MockBebopSettlement).runtimeCode);
        deal(WETH_ADDR, user, runtimeAmountIn);
        deal(USDC_ADDR, BEBOP_SETTLEMENT, expectedAmountOut);

        uint256 outputBalanceBefore = IERC20(USDC_ADDR).balanceOf(user);
        vm.startPrank(user);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, runtimeAmountIn);
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_bebop_partial_fill"
        );
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(
            MockBebopSettlement(BEBOP_SETTLEMENT).filledTakerAmount(),
            runtimeAmountIn,
            "provider received the wrong filled taker amount"
        );
        assertEq(
            IERC20(USDC_ADDR).balanceOf(user) - outputBalanceBefore,
            expectedAmountOut,
            "user received the wrong output amount"
        );
        assertEq(
            IERC20(WETH_ADDR).balanceOf(BEBOP_SETTLEMENT),
            runtimeAmountIn,
            "settlement received the wrong input amount"
        );
        assertEq(
            IERC20(WETH_ADDR).balanceOf(tychoRouterAddr),
            0,
            "weth left in router"
        );
    }
}
