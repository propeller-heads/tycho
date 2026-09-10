// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import "@src/executors/NativeExecutor.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {Constants} from "../Constants.sol";
import "forge-std/Test.sol";

// Mocks

contract MockNativeRouter {
    bool public called;
    uint256 public lastValue;
    bytes public lastCalldata;
    address public lastCaller;

    receive() external payable {}

    fallback() external payable {
        called = true;
        lastValue = msg.value;
        lastCalldata = msg.data;
        lastCaller = msg.sender;
    }
}

// Unit Tests

contract NativeExecutorUnitTest is Test, Constants {
    NativeExecutor executor;
    MockNativeRouter mockV6;

    address constant BAD_TARGET = address(0xdead);
    uint256 constant ACTUAL_SELLER_AMOUNT_OFFSET = 36;
    uint256 constant ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET = 68;

    function setUp() public {
        mockV6 = new MockNativeRouter();
        executor = new NativeExecutor(address(mockV6));
    }

    // Constructor tests

    function test_Constructor_StoresAddresses() public view {
        assertEq(executor.nativeRouterV6(), address(mockV6));
    }

    function test_Constructor_Reverts_ZeroAddress() public {
        vm.expectRevert(NativeExecutor__ZeroAddress.selector);
        new NativeExecutor(address(0));
    }

    function test_Constructor_Reverts_NotAContract() public {
        address eoa = makeAddr("eoa");

        vm.expectRevert(NativeExecutor__NotAContract.selector);
        new NativeExecutor(eoa);
    }

    // fundsExpectedAddress tests

    function test_FundsExpectedAddress_ReturnsMsgSender() public view {
        assertEq(executor.fundsExpectedAddress(hex""), address(this));
    }

    // getTransferData tests

    function test_GetTransferData_ERC20() public view {
        bytes memory payload = _tradePayload();
        bytes memory data =
            _encodeExecutorData(USDC_ADDR, ETH_ADDR, address(mockV6), payload);

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(data);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        assertEq(receiver, address(mockV6));
        assertEq(tokenIn, USDC_ADDR);
        assertEq(tokenOut, ETH_ADDR);
        assertTrue(outputToRouter);
    }

    function test_GetTransferData_NativeETH() public view {
        bytes memory payload = _tradePayload();
        bytes memory data =
            _encodeExecutorData(ETH_ADDR, USDC_ADDR, address(mockV6), payload);

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(data);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.TransferNativeInExecutor)
        );
        assertEq(receiver, address(0));
        assertEq(tokenIn, ETH_ADDR);
        assertEq(tokenOut, USDC_ADDR);
        assertTrue(outputToRouter);
    }

    function test_GetTransferData_Reverts_InvalidTarget() public {
        bytes memory payload = _tradePayload();
        bytes memory data =
            _encodeExecutorData(USDC_ADDR, ETH_ADDR, BAD_TARGET, payload);

        vm.expectRevert(NativeExecutor__InvalidTarget.selector);
        executor.getTransferData(data);
    }

    function test_GetTransferData_Reverts_TruncatedPayload() public {
        bytes memory payload =
            abi.encodePacked(executor.TRADE_RFQT_SELECTOR(), new bytes(95));
        bytes memory data =
            _encodeExecutorData(USDC_ADDR, ETH_ADDR, address(mockV6), payload);

        vm.expectRevert(NativeExecutor__InvalidDataLength.selector);
        executor.getTransferData(data);
    }

    // swap tests

    function test_Swap_ERC20_SendsZeroEth() public {
        bytes memory payload = _tradePayload();
        bytes memory data =
            _encodeExecutorData(USDC_ADDR, ETH_ADDR, address(mockV6), payload);

        executor.swap(1000000, data, address(0));

        assertTrue(mockV6.called());
        assertEq(mockV6.lastValue(), 0);
        assertEq(mockV6.lastCaller(), address(executor));
        assertEq(_wordAt(mockV6.lastCalldata(), ACTUAL_SELLER_AMOUNT_OFFSET), 0);
    }

    function test_Swap_ETH_ForwardsAmountIn() public {
        bytes memory payload = _tradePayload();
        bytes memory data =
            _encodeExecutorData(ETH_ADDR, USDC_ADDR, address(mockV6), payload);

        vm.deal(address(this), 1 ether);
        executor.swap{value: 1 ether}(1 ether, data, address(0));

        assertTrue(mockV6.called());
        assertEq(mockV6.lastValue(), 1 ether);
        assertEq(_wordAt(mockV6.lastCalldata(), ACTUAL_SELLER_AMOUNT_OFFSET), 0);
    }

    function test_Swap_ETH_Reverts_ZeroAmountIn() public {
        bytes memory payload = _tradePayload();
        bytes memory data =
            _encodeExecutorData(ETH_ADDR, USDC_ADDR, address(mockV6), payload);

        vm.expectRevert(NativeExecutor__InvalidAmountIn.selector);
        executor.swap(0, data, address(0));
    }

    function test_Swap_Reverts_ZeroSignedAmountIn() public {
        bytes memory data = _encodeExecutorData(
            USDC_ADDR, ETH_ADDR, address(mockV6), 0, _tradePayload()
        );

        vm.expectRevert(NativeExecutor__InvalidAmountIn.selector);
        executor.swap(1, data, address(0));
    }

    function test_Swap_Reverts_InvalidTarget() public {
        bytes memory payload = _tradePayload();
        bytes memory data =
            _encodeExecutorData(USDC_ADDR, ETH_ADDR, BAD_TARGET, payload);

        vm.expectRevert(NativeExecutor__InvalidTarget.selector);
        executor.swap(1000000, data, address(0));
    }

    function test_Swap_Reverts_InvalidSelector() public {
        // V4's tradeRFQT selector must not be accepted by the V6 executor.
        bytes4 badSelector = 0x0947c2d9;
        bytes memory payload = abi.encodeWithSelector(badSelector, hex"1234");
        bytes memory data =
            _encodeExecutorData(USDC_ADDR, ETH_ADDR, address(mockV6), payload);

        vm.expectRevert(NativeExecutor__InvalidPayload.selector);
        executor.swap(1000000, data, address(0));
    }

    function test_Swap_Reverts_ShortData() public {
        vm.expectRevert(NativeExecutor__InvalidDataLength.selector);
        executor.swap(1000000, hex"1234", address(0));
    }

    function test_Swap_ERC20_OverridesAmountOnUnderDelivery() public {
        uint256 signedAmountIn = 1_000_000;
        uint256 actualAmountIn = signedAmountIn - 1;
        bytes memory data = _encodeExecutorData(
            USDC_ADDR,
            ETH_ADDR,
            address(mockV6),
            signedAmountIn,
            _tradePayload()
        );

        executor.swap(actualAmountIn, data, address(0));

        assertTrue(mockV6.called());
        assertEq(
            _wordAt(mockV6.lastCalldata(), ACTUAL_SELLER_AMOUNT_OFFSET),
            actualAmountIn
        );
        assertEq(mockV6.lastValue(), 0);
    }

    function test_Swap_ETH_OverridesAmountAndForwardsActualValue() public {
        uint256 signedAmountIn = 1 ether;
        uint256 actualAmountIn = signedAmountIn - 1;
        bytes memory data = _encodeExecutorData(
            ETH_ADDR,
            USDC_ADDR,
            address(mockV6),
            signedAmountIn,
            _tradePayload()
        );

        vm.deal(address(this), actualAmountIn);
        executor.swap{value: actualAmountIn}(actualAmountIn, data, address(0));

        assertEq(
            _wordAt(mockV6.lastCalldata(), ACTUAL_SELLER_AMOUNT_OFFSET),
            actualAmountIn
        );
        assertEq(mockV6.lastValue(), actualAmountIn);
    }

    function test_Swap_ERC20_OverridesAmountOnOverDelivery() public {
        uint256 signedAmountIn = 1_000_000;
        uint256 actualAmountIn = signedAmountIn + 1;
        bytes memory data = _encodeExecutorData(
            USDC_ADDR,
            ETH_ADDR,
            address(mockV6),
            signedAmountIn,
            _tradePayload()
        );

        executor.swap(actualAmountIn, data, address(0));

        assertTrue(mockV6.called());
        assertEq(
            _wordAt(mockV6.lastCalldata(), ACTUAL_SELLER_AMOUNT_OFFSET),
            actualAmountIn
        );
        assertEq(
            _wordAt(mockV6.lastCalldata(), ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET), 0
        );
        assertEq(mockV6.lastValue(), 0);
    }

    function test_Swap_Reverts_PresetActualSellerAmount() public {
        bytes memory data = _encodeExecutorData(
            USDC_ADDR, ETH_ADDR, address(mockV6), 1_000_000, _tradePayload(1, 0)
        );

        vm.expectRevert(NativeExecutor__UnexpectedOverride.selector);
        executor.swap(1_000_000, data, address(0));
    }

    function test_Swap_Reverts_PresetActualMinOutputAmount() public {
        bytes memory data = _encodeExecutorData(
            USDC_ADDR, ETH_ADDR, address(mockV6), 1_000_000, _tradePayload(0, 1)
        );

        vm.expectRevert(NativeExecutor__UnexpectedOverride.selector);
        executor.swap(1_000_000, data, address(0));
    }

    // Helper

    function _tradePayload() internal view returns (bytes memory) {
        return _tradePayload(0, 0);
    }

    function _tradePayload(
        uint256 actualSellerAmount,
        uint256 actualMinOutputAmount
    ) internal view returns (bytes memory) {
        return abi.encodePacked(
            executor.TRADE_RFQT_SELECTOR(),
            bytes32(uint256(0x60)),
            bytes32(actualSellerAmount),
            bytes32(actualMinOutputAmount)
        );
    }

    function _wordAt(bytes memory data, uint256 offset)
        internal
        pure
        returns (uint256 value)
    {
        assembly ("memory-safe") {
            value := mload(add(add(data, 0x20), offset))
        }
    }

    function _encodeExecutorData(
        address tokenIn,
        address tokenOut,
        address target,
        bytes memory payload
    ) internal view returns (bytes memory) {
        uint256 signedAmountIn = tokenIn == ETH_ADDR ? 1 ether : 1_000_000;
        return
            _encodeExecutorData(
                tokenIn, tokenOut, target, signedAmountIn, payload
            );
    }

    function _encodeExecutorData(
        address tokenIn,
        address tokenOut,
        address target,
        uint256 signedAmountIn,
        bytes memory payload
    ) internal pure returns (bytes memory) {
        return abi.encodePacked(
            bytes20(tokenIn),
            bytes20(tokenOut),
            bytes20(target),
            bytes32(signedAmountIn),
            payload
        );
    }
}

// Integration Tests

contract NativeExecutorForkTest is Test, Constants {
    uint256 private constant FORK_BLOCK = 25930213;
    uint32 private constant ACTUAL_SELLER_AMOUNT_OFFSET = 36;
    uint32 private constant ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET = 68;
    uint32 private constant SIGNED_SELLER_AMOUNT_OFFSET = 260;
    uint32 private constant SIGNED_MIN_OUTPUT_AMOUNT_OFFSET = 324;
    uint256 private constant SIGNED_AMOUNT_IN = 1_000_000;
    uint256 private constant SIGNED_MINIMUM_OUTPUT = 402_408_818_842_972;
    uint256 private constant QUOTE_TIMESTAMP = 1_788_841_206;

    NativeExecutor nativeExecutor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("mainnet"), FORK_BLOCK);
        nativeExecutor = new NativeExecutor(NATIVE_ROUTER_V6_ETHEREUM);
    }

    function _recordedQuotePayload()
        private
        pure
        returns (bytes memory payload)
    {
        // Recorded from Native's firm-quote API for signedAmountIn and this recipient.
        // Pinning the fork and timestamp keeps it deterministic and CI independent from the API.
        payload =
            hex"7083527c000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ef435b99c2108d0309211d0cc05b70d202c0b5df000000000000000000000000129b3d9a0a6e4beab88f5cb1e57995d72a6e24f1000000000000000000000000cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc200000000000000000000000000000000000000000000000000000000000f424000000000000000000000000000000000000000000000000000016dfd1ab39d5c00000000000000000000000000000000000000000000000000016dfd1ab39d5c000000000000000000000000000000000000000000000000000000006a9f8d3c00000000000000000000000000000000000000000000000035a49220053ede33000000000000000000000000000000000000000000000000000000006a9f8cf5000000000000000000000000000000000000000000000000000000006a9f8d1d00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ec6e372d2326451e8711e06e0d8b78f9000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000003600000000000000000000000006044eef7179034319e2c8636ea885b37cbfa9aba000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003e00000000000000000000000000000000000000000000000000000000000000041dc6662535476055fdc70cbf26235a36548260584b63670d587fff026324eacff3a91e0b6cb791bd8059d8701ad7065a8944385d96b4c6dd6910d74f9d06364a91c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000041fbb4628ced3cdc79524c223e42adae7c302475dbac6ac3eb119293a41c3f3cf5482d18a6fa8c2bcdc07ae6551068dfa1dba9cff189cc56f8bc6c26f8783c8d261c00000000000000000000000000000000000000000000000000000000000000";
    }

    function _recordedQuoteData(bytes memory payload)
        private
        view
        returns (bytes memory)
    {
        return abi.encodePacked(
            bytes20(USDC_ADDR),
            bytes20(WETH_ADDR),
            bytes20(NATIVE_ROUTER_V6_ETHEREUM),
            bytes32(SIGNED_AMOUNT_IN),
            payload
        );
    }

    function _payloadWord(bytes memory payload, uint32 offset)
        private
        pure
        returns (uint256 word)
    {
        assembly ("memory-safe") {
            word := mload(add(add(payload, 0x20), offset))
        }
    }

    function _fundAndApprove(uint256 amountIn) private {
        deal(USDC_ADDR, address(nativeExecutor), amountIn);
        vm.prank(address(nativeExecutor));
        IERC20(USDC_ADDR).approve(NATIVE_ROUTER_V6_ETHEREUM, amountIn);
    }

    function _executeRecordedQuote(uint256 actualAmountIn)
        private
        returns (uint256 amountOut)
    {
        IERC20 WETH = IERC20(WETH_ADDR);
        uint256 balanceBefore = WETH.balanceOf(ALICE);
        bytes memory payload = _recordedQuotePayload();

        assertEq(_payloadWord(payload, ACTUAL_SELLER_AMOUNT_OFFSET), 0);
        assertEq(_payloadWord(payload, ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET), 0);
        assertEq(
            _payloadWord(payload, SIGNED_SELLER_AMOUNT_OFFSET), SIGNED_AMOUNT_IN
        );
        assertEq(
            _payloadWord(payload, SIGNED_MIN_OUTPUT_AMOUNT_OFFSET),
            SIGNED_MINIMUM_OUTPUT
        );

        _fundAndApprove(actualAmountIn);
        vm.warp(QUOTE_TIMESTAMP);

        nativeExecutor.swap(actualAmountIn, _recordedQuoteData(payload), ALICE);

        amountOut = WETH.balanceOf(ALICE) - balanceBefore;
        assertEq(IERC20(USDC_ADDR).balanceOf(address(nativeExecutor)), 0);
    }

    function _scaledMinimumOutput(uint256 actualAmountIn)
        private
        pure
        returns (uint256)
    {
        return SIGNED_MINIMUM_OUTPUT * actualAmountIn / SIGNED_AMOUNT_IN;
    }

    function test_RecordedQuoteUnderDeliveryAgainstRealNativeRouter() public {
        uint256 actualAmountIn = SIGNED_AMOUNT_IN - 1;

        assertGe(
            _executeRecordedQuote(actualAmountIn),
            _scaledMinimumOutput(actualAmountIn)
        );
    }

    function test_RecordedQuoteOverDeliveryAgainstRealNativeRouter() public {
        uint256 actualAmountIn = SIGNED_AMOUNT_IN + 1;

        assertGe(
            _executeRecordedQuote(actualAmountIn),
            _scaledMinimumOutput(actualAmountIn)
        );
    }

    function test_RecordedQuoteRevertsAtUpwardDeviationLimit() public {
        uint256 actualAmountIn = SIGNED_AMOUNT_IN + SIGNED_AMOUNT_IN / 10;

        _fundAndApprove(actualAmountIn);
        vm.warp(QUOTE_TIMESTAMP);

        vm.expectRevert(bytes4(keccak256("AmountDeviationExceeds()")));
        nativeExecutor.swap(
            actualAmountIn, _recordedQuoteData(_recordedQuotePayload()), ALICE
        );

        assertEq(
            IERC20(USDC_ADDR).balanceOf(address(nativeExecutor)), actualAmountIn
        );
    }
}

contract TychoRouterNativeIntegrationTest is TychoRouterTestSetup {
    function setUp() public override {
        super.setUp();
        // Execute within the recorded V6 quotes' validity window.
        vm.warp(1_788_841_206);
    }

    function getForkBlock() public pure override returns (uint256) {
        // The two firm quotes below were recorded against this block for the
        // deterministic Tycho Router address deployed by TychoRouterTestSetup.
        return 25930213;
    }

    function test_RecordedQuoteERC20InputThroughTychoRouter() public {
        IERC20 USDC = IERC20(USDC_ADDR);
        uint256 amountIn = 3000000000;
        uint256 amountOut = 1_207_228_929_311_270_300;

        deal(address(USDC), ALICE, amountIn);
        uint256 balanceBefore = BOB.balance;

        vm.startPrank(ALICE);
        USDC.approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_native");

        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        uint256 balanceAfter = BOB.balance;
        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, amountOut);
        assertEq(USDC.balanceOf(tychoRouterAddr), 0);
        assertEq(tychoRouterAddr.balance, 0);
    }

    function test_RecordedQuoteNativeInputThroughTychoRouter() public {
        IERC20 USDC = IERC20(USDC_ADDR);
        uint256 amountIn = 1 ether;
        uint256 amountOut = 2_483_340_204;

        deal(ALICE, amountIn);

        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_native_eth_input"
        );
        uint256 balanceBefore = USDC.balanceOf(BOB);

        vm.prank(ALICE);
        (bool success,) = tychoRouterAddr.call{value: amountIn}(callData);

        assertTrue(success, "Call Failed");
        assertEq(USDC.balanceOf(BOB) - balanceBefore, amountOut);
        assertEq(USDC.balanceOf(tychoRouterAddr), 0);
        assertEq(tychoRouterAddr.balance, 0);
    }
}
