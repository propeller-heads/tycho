pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import "@src/executors/HashflowExecutor.sol";
import "forge-std/Test.sol";
import {Constants} from "../Constants.sol";

contract HashflowUtils is Test {
    constructor() {}

    function encodeRfqtQuote(IHashflowRouter.RFQTQuote memory quote)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(
            quote.pool, // pool (20 bytes)
            quote.externalAccount, // externalAccount (20 bytes)
            quote.trader, // trader (20 bytes)
            quote.effectiveTrader, // effectiveTrader (20 bytes)
            quote.baseToken, // baseToken (20 bytes)
            quote.quoteToken, // quoteToken (20 bytes)
            quote.baseTokenAmount, // baseTokenAmount (32 bytes)
            quote.quoteTokenAmount, // quoteTokenAmount (32 bytes)
            quote.quoteExpiry, // quoteExpiry (32 bytes)
            quote.nonce, // nonce (32 bytes)
            quote.txid, // txid (32 bytes)
            quote.signature // signature data
        );
    }
}

contract HashflowExecutorECR20Test is Constants, TestUtils, HashflowUtils {
    using SafeERC20 for IERC20;

    HashflowExecutorExposed executor;
    uint256 forkBlock;

    IERC20 WETH = IERC20(WETH_ADDR);
    IERC20 USDC = IERC20(USDC_ADDR);

    function setUp() public {
        forkBlock = 23188416; // Using expiry date: 1755766775, ECR20
        vm.createSelectFork("mainnet", forkBlock);
        executor = new HashflowExecutorExposed(HASHFLOW_ROUTER);
    }

    function testDecodeParams() public view {
        // Synthetic quote instead of the realistic rfqtQuote() fixture: there
        // trader == effectiveTrader, so a decoder reading the wrong byte slice
        // still returns the expected value. Unique values per field make any
        // offset mistake fail an assertion; decoding never checks the
        // signature, so the values need not be real.
        IHashflowRouter.RFQTQuote memory expected_quote =
            IHashflowRouter.RFQTQuote({
                pool: address(0x1111111111111111111111111111111111111111),
                externalAccount: address(
                    0x2222222222222222222222222222222222222222
                ),
                trader: address(0x3333333333333333333333333333333333333333),
                effectiveTrader: address(
                    0x4444444444444444444444444444444444444444
                ),
                baseToken: address(0x5555555555555555555555555555555555555555),
                quoteToken: address(0x6666666666666666666666666666666666666666),
                effectiveBaseTokenAmount: 0,
                baseTokenAmount: 7777,
                quoteTokenAmount: 8888,
                quoteExpiry: 9999,
                nonce: 101010,
                txid: bytes32(uint256(0xabcdef)),
                signature: hex"0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f4041"
            });
        bytes memory encodedQuote = encodeRfqtQuote(expected_quote);
        (IHashflowRouter.RFQTQuote memory quote) =
            executor.decodeData(encodedQuote);

        assertEq(quote.pool, expected_quote.pool, "pool mismatch");
        assertEq(
            quote.externalAccount,
            expected_quote.externalAccount,
            "externalAccount mismatch"
        );
        assertEq(quote.trader, expected_quote.trader, "trader mismatch");
        assertEq(
            quote.effectiveTrader,
            expected_quote.effectiveTrader,
            "effectiveTrader mismatch"
        );
        assertEq(
            quote.baseToken, expected_quote.baseToken, "baseToken mismatch"
        );
        assertEq(
            quote.quoteToken, expected_quote.quoteToken, "quoteToken mismatch"
        );
        assertEq(
            quote.effectiveBaseTokenAmount,
            expected_quote.effectiveBaseTokenAmount,
            "effectiveBaseTokenAmount mismatch"
        );
        assertEq(
            quote.baseTokenAmount,
            expected_quote.baseTokenAmount,
            "baseTokenAmount mismatch"
        );
        assertEq(
            quote.quoteTokenAmount,
            expected_quote.quoteTokenAmount,
            "quoteTokenAmount mismatch"
        );
        assertEq(
            quote.quoteExpiry,
            expected_quote.quoteExpiry,
            "quoteExpiry mismatch"
        );
        assertEq(quote.nonce, expected_quote.nonce, "nonce mismatch");
        assertEq(quote.txid, expected_quote.txid, "txid mismatch");
        assertEq(
            quote.signature, expected_quote.signature, "signature mismatch"
        );
    }

    function testDecodeParamsInvalidDataLength() public {
        // The previous layout, without effectiveTrader, must not decode
        bytes memory oldFormatData = new bytes(325);
        vm.expectRevert(HashflowExecutor__InvalidDataLength.selector);
        executor.decodeData(oldFormatData);
    }

    function testGetTransferData() public {
        IHashflowRouter.RFQTQuote memory expected_quote = rfqtQuote();
        bytes memory encodedQuote = encodeRfqtQuote(expected_quote);

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(encodedQuote);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit),
            "transferType mismatch"
        );
        assertEq(receiver, HASHFLOW_ROUTER, "receiver mismatch");
        assertEq(tokenIn, expected_quote.baseToken, "baseToken mismatch");
        assertEq(tokenOut, expected_quote.quoteToken, "quoteToken mismatch");
        assertEq(outputToRouter, true, "outputToRouter mismatch");
    }

    function testSwapNoSlippage() public {
        address trader = address(ALICE);
        IHashflowRouter.RFQTQuote memory quote = rfqtQuote();
        uint256 amountIn = quote.baseTokenAmount;
        bytes memory encodedQuote = encodeRfqtQuote(quote);

        deal(WETH_ADDR, address(executor), amountIn);
        uint256 balanceBefore = USDC.balanceOf(trader);

        vm.prank(address(executor));
        IERC20(quote.baseToken).approve(HASHFLOW_ROUTER, amountIn);
        vm.stopPrank();

        vm.prank(trader);
        executor.swap(amountIn, encodedQuote, address(executor));

        uint256 balanceAfter = USDC.balanceOf(trader);
        assertGt(balanceAfter, balanceBefore);
        assertEq(balanceAfter - balanceBefore, quote.quoteTokenAmount);
    }

    function testSwapRouterAmountUnderQuoteAmount() public {
        address trader = address(ALICE);
        IHashflowRouter.RFQTQuote memory quote = rfqtQuote();
        uint256 amountIn = quote.baseTokenAmount - 1;
        bytes memory encodedQuote = encodeRfqtQuote(quote);

        deal(WETH_ADDR, address(executor), amountIn);
        uint256 balanceBefore = USDC.balanceOf(trader);

        vm.prank(address(executor));
        IERC20(quote.baseToken).approve(HASHFLOW_ROUTER, amountIn);
        vm.stopPrank();

        vm.prank(trader);
        executor.swap(amountIn, encodedQuote, address(executor));

        uint256 balanceAfter = USDC.balanceOf(trader);
        assertGt(balanceAfter, balanceBefore);
        assertLt(balanceAfter - balanceBefore, quote.quoteTokenAmount);
    }

    function testSwapRouterAmountOverQuoteAmount() public {
        address trader = address(ALICE);
        IHashflowRouter.RFQTQuote memory quote = rfqtQuote();
        uint256 amountIn = quote.baseTokenAmount + 1;
        bytes memory encodedQuote = encodeRfqtQuote(quote);

        deal(WETH_ADDR, address(executor), amountIn);
        uint256 balanceBefore = USDC.balanceOf(trader);

        vm.prank(address(executor));
        IERC20(quote.baseToken).approve(HASHFLOW_ROUTER, amountIn);
        vm.stopPrank();

        vm.prank(trader);
        executor.swap(amountIn, encodedQuote, address(executor));

        uint256 balanceAfter = USDC.balanceOf(trader);
        assertGt(balanceAfter, balanceBefore);
        assertEq(balanceAfter - balanceBefore, quote.quoteTokenAmount);
    }

    function rfqtQuote()
        internal
        view
        returns (IHashflowRouter.RFQTQuote memory)
    {
        return IHashflowRouter.RFQTQuote({
            pool: address(0x5d8853028fbF6a2da43c7A828cc5f691E9456B44),
            externalAccount: address(
                0x9bA0CF1588E1DFA905eC948F7FE5104dD40EDa31
            ),
            trader: address(ALICE),
            effectiveTrader: address(ALICE),
            baseToken: WETH_ADDR,
            quoteToken: USDC_ADDR,
            effectiveBaseTokenAmount: 0,
            baseTokenAmount: 1000000000000000000,
            quoteTokenAmount: 4286117034,
            quoteExpiry: 1755766775,
            nonce: 1755766744988,
            txid: bytes32(
                uint256(
                    0x12500006400064000186078c183380ffffffffffffff00296d737ff6ae950000
                )
            ),
            signature: hex"649d31cd74f1b11b4a3b32bd38c2525d78ce8f23bc2eaf7700899c3a396d3a137c861737dc780fa154699eafb3108a34cbb2d4e31a6f0623c169cc19e0fa296a1c"
        });
    }
}

contract HashflowExecutorNativeTest is Constants, HashflowUtils {
    using SafeERC20 for IERC20;

    HashflowExecutorExposed executor;
    uint256 forkBlock;

    IERC20 WETH = IERC20(WETH_ADDR);
    IERC20 USDC = IERC20(USDC_ADDR);

    function setUp() public {
        forkBlock = 23188504; // Using expiry date: 1755767859, Native
        vm.createSelectFork("mainnet", forkBlock);
        executor = new HashflowExecutorExposed(HASHFLOW_ROUTER);
    }

    function testSwapNoSlippage() public {
        address trader = address(ALICE);
        IHashflowRouter.RFQTQuote memory quote = rfqtQuote();
        uint256 amountIn = quote.baseTokenAmount;
        bytes memory encodedQuote = encodeRfqtQuote(quote);

        vm.deal(address(executor), amountIn);
        uint256 balanceBefore = USDC.balanceOf(trader);

        vm.prank(trader);
        executor.swap(amountIn, encodedQuote, address(executor));

        uint256 balanceAfter = USDC.balanceOf(trader);
        assertGt(balanceAfter, balanceBefore);
        assertEq(balanceAfter - balanceBefore, quote.quoteTokenAmount);
    }

    function rfqtQuote()
        internal
        view
        returns (IHashflowRouter.RFQTQuote memory)
    {
        return IHashflowRouter.RFQTQuote({
            pool: address(0x713DC4Df480235dBe2fB766E7120Cbd4041Dcb58),
            externalAccount: address(
                0x111BB8c3542F2B92fb41B8d913c01D3788431111
            ),
            trader: address(ALICE),
            effectiveTrader: address(ALICE),
            baseToken: address(0x0000000000000000000000000000000000000000),
            quoteToken: USDC_ADDR,
            effectiveBaseTokenAmount: 0,
            baseTokenAmount: 10000000000000000,
            quoteTokenAmount: 42586008,
            quoteExpiry: 1755767859,
            nonce: 1755767819299,
            txid: bytes32(
                uint256(
                    0x1250000640006400018380fd594810ffffffffffffff00296d83e467cddd0000
                )
            ),
            signature: hex"63c1c9c7d6902d1d4d2ae82777015433ef08366dde1c579a8c4cbc01059166064246f61f15b2cb130be8f2b28ea40d2c3586ef0133647fefa30003e70ffbd6131b"
        });
    }
}

contract HashflowExecutorEffectiveTraderTest is Constants, HashflowUtils {
    HashflowExecutorExposed executor;
    uint256 forkBlock;

    IERC20 USDC = IERC20(USDC_ADDR);

    function setUp() public {
        // The quote below lives from its nonce (a maker timestamp) to its
        // expiry, ~45s; the router rejects it outside that window ("Nonce too
        // high" before, "Quote expired" after). This block is inside it.
        forkBlock = 25975788; // Using expiry date: 1789390008
        vm.createSelectFork("mainnet", forkBlock);
        executor = new HashflowExecutorExposed(HASHFLOW_ROUTER);
    }

    function testSwapEffectiveTraderNotCaller() public {
        // The maker signed this quote with an effectiveTrader nobody controls
        // (0x…DeaDBeef). The router reads effectiveTrader only to scope quote
        // nonces, so the swap executes exactly like one whose effectiveTrader
        // equals the trader.
        IHashflowRouter.RFQTQuote memory quote = IHashflowRouter.RFQTQuote({
            pool: address(0x478Eca1b93865dcA0b9f325935eb123C8a4aF011),
            externalAccount: address(
                0xBEE3211ab312a8D065c4FeF0247448e17A8da000
            ),
            trader: address(ALICE),
            effectiveTrader: address(
                0x00000000000000000000000000000000DeaDBeef
            ),
            baseToken: WETH_ADDR,
            quoteToken: USDC_ADDR,
            effectiveBaseTokenAmount: 0,
            baseTokenAmount: 1000000000000000000,
            quoteTokenAmount: 2510840211,
            quoteExpiry: 1789390008,
            nonce: 1789389963406,
            txid: bytes32(
                uint256(
                    0x125000064000640000e43f2da72000ffffffffffffff0031418d1897fb6d0000
                )
            ),
            signature: hex"00e59648df10ba85d06a4c6b1eda60d3b48ac658bbfbd969f6b4ff5b7cdce13c3650ce5f8a0ab917164316e2d75886045d5ed7f5dea08a8de2f689e3c37cb7231c"
        });
        uint256 amountIn = quote.baseTokenAmount;
        bytes memory encodedQuote = encodeRfqtQuote(quote);

        deal(WETH_ADDR, address(executor), amountIn);
        uint256 balanceBefore = USDC.balanceOf(ALICE);

        vm.prank(address(executor));
        IERC20(quote.baseToken).approve(HASHFLOW_ROUTER, amountIn);
        vm.stopPrank();

        vm.prank(ALICE);
        executor.swap(amountIn, encodedQuote, address(executor));

        uint256 balanceAfter = USDC.balanceOf(ALICE);
        assertEq(balanceAfter - balanceBefore, quote.quoteTokenAmount);
    }
}

contract HashflowExecutorExposed is HashflowExecutor {
    constructor(address _hashflowRouter) HashflowExecutor(_hashflowRouter) {}

    function decodeData(bytes calldata data)
        external
        pure
        returns (IHashflowRouter.RFQTQuote memory quote)
    {
        return _decodeData(data);
    }
}

contract TychoRouterSingleSwapTestForHashflow is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 25975860;
    }

    function testHashflowIntegration() public {
        // Performs a swap from USDC to WBTC using Hashflow RFQ
        //
        //   USDC ───(Hashflow RFQ)──> WBTC

        // The Hashflow order expects:
        // - 4308094737 USDC input -> 5542168 WBTC output
        // The maker signed it with a random effectiveTrader (0x...deadbeef)

        uint256 amountIn = 4308094737;
        uint256 expectedAmountOut = 5542168;
        deal(USDC_ADDR, ALICE, amountIn);
        uint256 balanceBefore = IERC20(WBTC_ADDR).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData =
            loadCallDataFromFile("test_single_encoding_strategy_hashflow");
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(WBTC_ADDR).balanceOf(ALICE);

        assertTrue(success, "Call Failed");
        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }
}
