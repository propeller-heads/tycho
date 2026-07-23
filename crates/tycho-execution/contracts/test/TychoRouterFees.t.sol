pragma solidity ^0.8.26;

import "./TychoRouterTestSetup.sol";
import {FeeCalculator} from "@src/FeeCalculator.sol";
import {
    Vault__UnexpectedNonZeroCount,
    Vault__UnexpectedInputDelta
} from "@src/Vault.sol";
import {
    TychoRouter__InvalidClientSignature,
    TychoRouter__ExpiredClientSignature,
    TychoRouter__FeesExceedOutput,
    ClientFeeParams
} from "@src/TychoRouter.sol";
import {FeeRecipient, FeeInput} from "../lib/FeeStructs.sol";
import {IFeeCalculator, CustomFees} from "@interfaces/IFeeCalculator.sol";

/// @dev Malicious FeeCalculator that claims one wei more in fees than the
///      swap produced. Mirrors the real implementation's [router, client]
///      return shape so the only deviation under test is the total amount.
contract OverchargingFeeCalculator is IFeeCalculator {
    function calculateFee(FeeInput memory feeInput)
        external
        pure
        returns (FeeRecipient[] memory feeRecipients)
    {
        feeRecipients = new FeeRecipient[](2);
        feeRecipients[0] = FeeRecipient({
            recipient: address(0xFEE), feeAmount: feeInput.actualAmountOut
        });
        feeRecipients[1] =
            FeeRecipient({recipient: feeInput.client, feeAmount: 1});
    }

    function mustOutputThroughRouter(uint32, address)
        external
        pure
        returns (bool)
    {
        return true;
    }

    function getAllClientFees(uint256, uint256)
        external
        pure
        returns (address[] memory clients, CustomFees[] memory fees)
    {
        return (new address[](0), new CustomFees[](0));
    }
}

contract TychoRouterFeesTest is TychoRouterTestSetup {
    event FeesTaken(address indexed token, FeeRecipient[] fees);

    function testSingleSwapWithAllFeeTypes() public {
        // Set up fees: 1% router fee on output, 2% client fee, 10% router fee on client fee
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(routerFeeReceiver);
        feeCalculator.setRouterFeeOnOutput(1_000_000); // 1%
        feeCalculator.setRouterFeeOnClientFee(10_000_000); // 10%
        vm.stopPrank();

        // Trade 1 WETH for DAI with 1 swap on Uniswap V2
        uint256 amountIn = 1 ether;

        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(address(tychoRouterAddr), amountIn);

        // When fees are present, encode receiver as TychoRouter (not ALICE)
        bytes memory protocolData =
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);

        bytes memory swap =
            encodeSingleSwap(address(usv2Executor), protocolData);

        // quotedAmountOut = expected gross output; fees are calculated on this amount
        uint256 quotedAmountOut = 2018817438608734439722;

        // Flow with fees:
        // 1. Swap sends full output to router (2018817438608734439722 DAI)
        // 2. takeFees deducts fees and credits fee recipients' vaults
        // 3. Router transfers amountOut (after fees) to ALICE's address
        // 4. ALICE receives 1958252915450472406531 DAI in her address
        // 5. Fee recipients have fees in their vaults

        // Expected fees with all three fee types:
        // 1. clientFee = 2018817438608734439722 * 2_000_000 / 100_000_000 = 40376348772174688794
        //    routerFeeOnClientFee = (2018817438608734439722 * 2_000_000 * 10_000_000) / 100_000_000^2
        //                        = 4037634877217468879
        //    clientPortion = 40376348772174688794 - 4037634877217468879 = 36338713894957219915
        // 2. routerFeeOnOutput = 2018817438608734439722 * 1_000_000 / 100_000_000 = 20188174386087344397
        //    totalRouterFee = 4037634877217468879 + 20188174386087344397 = 24225809263304813276
        // 3. amountOut = 2018817438608734439722 - 36338713894957219915 - 24225809263304813276
        //    = 1958252915450472406531
        uint256 expectedRouterFee = 24225809263304813276;
        uint256 expectedClientFee = 36338713894957219915;
        uint256 expectedAmountOut = 1958252915450472406531;

        // minAmountOut is 5% below quotedAmountOut; total fees are ~3% of
        // quotedAmountOut, so the post-fee output must still clear the min
        // (the slippage check runs on the post-fee output).
        uint256 minAmountOut = quotedAmountOut * 9500 / 10000;
        ClientFeeParams memory feeParams = makeClientFeeParams(
            2_000_000,
            0,
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            quotedAmountOut,
            minAmountOut,
            ALICE,
            swap,
            tychoRouterAddr,
            CLIENT_FEE_RECEIVER_PK
        );
        FeeRecipient[] memory expectedFees = new FeeRecipient[](2);
        expectedFees[0] = FeeRecipient({
            recipient: routerFeeReceiver, feeAmount: expectedRouterFee
        });
        expectedFees[1] = FeeRecipient({
            recipient: clientFeeReceiver, feeAmount: expectedClientFee
        });
        vm.expectEmit();
        emit FeesTaken(DAI_ADDR, expectedFees);

        uint256 swapOutput = tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            quotedAmountOut,
            minAmountOut,
            ALICE,
            feeParams,
            swap
        );
        vm.stopPrank();

        assertEq(swapOutput, expectedAmountOut);

        // Check router fee receiver vault balance
        uint256 routerFeeReceiverBalance = tychoRouter.balanceOf(
            routerFeeReceiver, uint256(uint160(DAI_ADDR))
        );
        assertEq(routerFeeReceiverBalance, expectedRouterFee);

        // Check client fee receiver vault balance
        uint256 clientFeeReceiverBalance = tychoRouter.balanceOf(
            clientFeeReceiver, uint256(uint160(DAI_ADDR))
        );
        assertEq(clientFeeReceiverBalance, expectedClientFee);

        // Check ALICE received correct amount in her address (not vault)
        uint256 userBalance = IERC20(DAI_ADDR).balanceOf(ALICE);
        assertEq(userBalance, expectedAmountOut);
    }

    function testSingleSwapWithClientFees() public {
        // Tests swapping WETH -> DAI on a USV2 pool with fees and client contribution
        // Swap is 1 WETH for 2018.8 DAI (2018817438608734439722)
        // Client takes 1% ->  20.18 DAI (20188174386087344397)

        deal(WETH_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(DAI_ADDR).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_single_swap_with_client_fees");
        uint256 expectedFeeAmount = 20188174386087344397;
        FeeRecipient[] memory expectedFees = new FeeRecipient[](2);
        expectedFees[0] = FeeRecipient({
            recipient: feeCalculator.getRouterFeeReceiver(), feeAmount: 0
        });
        expectedFees[1] = FeeRecipient({
            recipient: clientFeeReceiver, feeAmount: expectedFeeAmount
        });
        vm.expectEmit();
        emit FeesTaken(DAI_ADDR, expectedFees);
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(DAI_ADDR).balanceOf(ALICE);
        assertTrue(success, "Call Failed");
        uint256 expectedAmountOut = 1998629264222647095325;
        assertEq(balanceAfter - balanceBefore, expectedAmountOut);

        // Check client fee receiver vault balance (BOB)
        uint256 clientFeeReceiverBalance = tychoRouter.balanceOf(
            clientFeeReceiver, uint256(uint160(DAI_ADDR))
        );
        assertEq(clientFeeReceiverBalance, expectedFeeAmount);
    }

    function testSingleSwapWithFeesAndContribution() public {
        // Tests swapping WETH -> DAI on a USV2 pool with fees and client contribution
        // Swap is 1 WETH for      2018.8 DAI (2018817438608734439722, gross output)
        // quotedAmountOut = 2000e18; fees are 1% of actualAmountOut each
        // Tycho Router takes 1% -> 20.19 DAI (20188174386087344397)
        // Client takes 1% ->       20.19 DAI (20188174386087344397)
        // Remaining = 2018.8 - 40.38 = 1978.42 < 2000 so client contributes ~21.56 DAI (max 22)

        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(routerFeeReceiver);
        feeCalculator.setRouterFeeOnOutput(1_000_000); // 1%
        vm.stopPrank();

        deal(WETH_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(DAI_ADDR).balanceOf(ALICE);

        // deal contribution to client
        vm.startPrank(clientFeeReceiver);
        uint256 contribution = 22_000000000000000000;
        deal(DAI_ADDR, clientFeeReceiver, contribution);
        IERC20(DAI_ADDR).approve(tychoRouterAddr, contribution);
        tychoRouter.deposit(DAI_ADDR, contribution);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);
        bytes memory callData = loadCallDataFromFile(
            "test_single_swap_with_fees_and_client_contribution"
        );
        uint256 expectedFeeAmount = 20188174386087344397;
        FeeRecipient[] memory expectedFees = new FeeRecipient[](2);
        expectedFees[0] = FeeRecipient({
            recipient: routerFeeReceiver, feeAmount: expectedFeeAmount
        });
        expectedFees[1] = FeeRecipient({
            recipient: clientFeeReceiver, feeAmount: expectedFeeAmount
        });
        vm.expectEmit();
        emit FeesTaken(DAI_ADDR, expectedFees);
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(DAI_ADDR).balanceOf(ALICE);
        assertTrue(success, "Call Failed");
        uint256 expectedAmountOut = 2000_000000000000000000;
        assertEq(balanceAfter - balanceBefore, expectedAmountOut);
        // Check router fee receiver vault balance
        uint256 routerFeeReceiverBalance = tychoRouter.balanceOf(
            routerFeeReceiver, uint256(uint160(DAI_ADDR))
        );
        assertEq(routerFeeReceiverBalance, expectedFeeAmount);

        // Check client fee receiver vault balance
        uint256 clientFeeReceiverBalance = tychoRouter.balanceOf(
            clientFeeReceiver, uint256(uint160(DAI_ADDR))
        );
        // there are leftover funds from the contribution so this value is not only the expectedFeeAmount
        assertGt(clientFeeReceiverBalance, expectedFeeAmount);
    }

    function testSequentialSwapWithClientFees() public {
        // Performs a sequential swap from WETH to USDC through WBTC using USV2 pools
        //
        //   WETH ───(USV2)──> WBTC ───(USV2)──> USDC
        //   1 WETH -> 1951856272 USDC
        // Client takes 1% (19518562 USDC)

        deal(WETH_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_sequential_swap_strategy_with_fees");
        uint256 expectedFeeAmount = 19518562;
        FeeRecipient[] memory expectedFees = new FeeRecipient[](2);
        expectedFees[0] = FeeRecipient({
            recipient: feeCalculator.getRouterFeeReceiver(), feeAmount: 0
        });
        expectedFees[1] = FeeRecipient({
            recipient: clientFeeReceiver, feeAmount: expectedFeeAmount
        });
        vm.expectEmit();
        emit FeesTaken(USDC_ADDR, expectedFees);
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(USDC_ADDR).balanceOf(ALICE);
        assertTrue(success, "Call Failed");
        uint256 expectedAmountOut = 1932337710;
        assertEq(balanceAfter - balanceBefore, expectedAmountOut);

        // Check client fee receiver vault balance
        uint256 clientFeeReceiverBalance = tychoRouter.balanceOf(
            clientFeeReceiver, uint256(uint160(USDC_ADDR))
        );
        assertEq(clientFeeReceiverBalance, expectedFeeAmount);
    }

    function testRejectsExpiredClientSignature() public {
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes memory protocolData =
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);
        bytes memory swap =
            encodeSingleSwap(address(usv2Executor), protocolData);

        ClientFeeParams memory feeParams = ClientFeeParams({
            clientFeeBps: 100,
            clientFeeReceiver: vm.addr(CLIENT_FEE_RECEIVER_PK),
            maxClientContribution: 0,
            deadline: block.timestamp - 1,
            clientSignature: new bytes(0)
        });
        feeParams.clientSignature = signClientFee(
            feeParams,
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            1,
            1,
            ALICE,
            swap,
            tychoRouterAddr,
            CLIENT_FEE_RECEIVER_PK
        );

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__ExpiredClientSignature.selector,
                feeParams.deadline,
                block.timestamp
            )
        );
        tychoRouter.singleSwap(
            amountIn, WETH_ADDR, DAI_ADDR, 1, 1, ALICE, feeParams, swap
        );
        vm.stopPrank();
    }

    function testRejectsWrongSigner() public {
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes memory protocolData =
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);
        bytes memory swap =
            encodeSingleSwap(address(usv2Executor), protocolData);

        ClientFeeParams memory feeParams = ClientFeeParams({
            clientFeeBps: 100,
            clientFeeReceiver: vm.addr(CLIENT_FEE_RECEIVER_PK),
            maxClientContribution: 0,
            deadline: block.timestamp + 1 hours,
            clientSignature: new bytes(0)
        });
        // Sign with ALICE's key instead of the clientFeeReceiver's key
        feeParams.clientSignature = signClientFee(
            feeParams,
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            1,
            1,
            ALICE,
            swap,
            tychoRouterAddr,
            ALICE_PK
        );

        vm.expectRevert(TychoRouter__InvalidClientSignature.selector);
        tychoRouter.singleSwap(
            amountIn, WETH_ADDR, DAI_ADDR, 1, 1, ALICE, feeParams, swap
        );
        vm.stopPrank();
    }

    function testRejectsManipulatedFee() public {
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes memory protocolData =
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);
        bytes memory swap =
            encodeSingleSwap(address(usv2Executor), protocolData);

        // Sign params with 100 bps
        ClientFeeParams memory feeParams = ClientFeeParams({
            clientFeeBps: 100,
            clientFeeReceiver: vm.addr(CLIENT_FEE_RECEIVER_PK),
            maxClientContribution: 0,
            deadline: block.timestamp + 1 hours,
            clientSignature: new bytes(0)
        });
        feeParams.clientSignature = signClientFee(
            feeParams,
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            1,
            1,
            ALICE,
            swap,
            tychoRouterAddr,
            CLIENT_FEE_RECEIVER_PK
        );
        // Manipulate: bump fee from 100 to 200 bps after signing
        feeParams.clientFeeBps = 200;

        vm.expectRevert(TychoRouter__InvalidClientSignature.selector);
        tychoRouter.singleSwap(
            amountIn, WETH_ADDR, DAI_ADDR, 1, 1, ALICE, feeParams, swap
        );
        vm.stopPrank();
    }

    function testWrongChainSignature() public {
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes memory protocolData =
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);
        bytes memory swap =
            encodeSingleSwap(address(usv2Executor), protocolData);

        ClientFeeParams memory feeParams = ClientFeeParams({
            clientFeeBps: 100,
            clientFeeReceiver: vm.addr(CLIENT_FEE_RECEIVER_PK),
            maxClientContribution: 0,
            deadline: block.timestamp + 1 hours,
            clientSignature: new bytes(0)
        });
        // Sign for a different chain — should not verify on the current chain
        feeParams.clientSignature = signClientFeeForChain(
            feeParams,
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            1,
            1,
            ALICE,
            swap,
            tychoRouterAddr,
            block.chainid + 1,
            CLIENT_FEE_RECEIVER_PK
        );

        vm.expectRevert(TychoRouter__InvalidClientSignature.selector);
        tychoRouter.singleSwap(
            amountIn, WETH_ADDR, DAI_ADDR, 1, 1, ALICE, feeParams, swap
        );
        vm.stopPrank();
    }

    function testSplitSwapWithClientFees() public {
        // Performs a split swap from WETH to USDC though WBTC and DAI using USV2 pools
        //
        //         ┌──(USV2)──> WBTC ───(USV2)──> USDC
        //   WETH ─┤
        //         └──(USV2)──> DAI  ───(USV2)──> USDC
        //  1 WETH -> 991384372 + 1004476082 = 1995860454 USDC
        // Client takes 1% (19958604)

        deal(WETH_ADDR, ALICE, 1 ether);
        uint256 balanceBefore = IERC20(USDC_ADDR).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, type(uint256).max);

        bytes memory callData =
            loadCallDataFromFile("test_split_swap_strategy_with_fees");
        uint256 expectedFeeAmount = 19958604;
        FeeRecipient[] memory expectedFees = new FeeRecipient[](2);
        expectedFees[0] = FeeRecipient({
            recipient: feeCalculator.getRouterFeeReceiver(), feeAmount: 0
        });
        expectedFees[1] = FeeRecipient({
            recipient: clientFeeReceiver, feeAmount: expectedFeeAmount
        });
        vm.expectEmit();
        emit FeesTaken(USDC_ADDR, expectedFees);
        (bool success,) = tychoRouterAddr.call(callData);

        vm.stopPrank();

        uint256 balanceAfter = IERC20(USDC_ADDR).balanceOf(ALICE);
        assertTrue(success, "Call Failed");
        uint256 expectedAmountOut = 1975901850;
        assertEq(balanceAfter - balanceBefore, expectedAmountOut);

        // Check client fee receiver vault balance (BOB)
        uint256 clientFeeReceiverBalance = tychoRouter.balanceOf(
            clientFeeReceiver, uint256(uint160(USDC_ADDR))
        );
        assertEq(clientFeeReceiverBalance, expectedFeeAmount);
    }

    function testSingleSwapFeeOnTransferTokenSTA() public {
        // STA is a fee token that takes a fee in ALL transfers (by protocols or direct from user to user)
        address STA_ADDR = address(0xa7DE087329BFcda5639247F96140f9DAbe3DeED1);
        address STA_WETH_UNIV2_POOL = 0x59F96b8571E3B11f859A09Eaf5a790A138FC64D0;

        uint256 amountIn = 1 ether;

        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(address(tychoRouterAddr), amountIn);

        bytes memory protocolData =
            encodeUniswapV2Swap(STA_WETH_UNIV2_POOL, WETH_ADDR, STA_ADDR);

        bytes memory swap =
            encodeSingleSwap(address(usv2Executor), protocolData);

        // router actually received 1271775641957229539568553 STA after pool fee
        uint256 quotedSTA = 1271775641957229539568553;
        ClientFeeParams memory feeParams = makeClientFeeParams(
            10_000,
            20,
            amountIn,
            WETH_ADDR,
            STA_ADDR,
            quotedSTA,
            quotedSTA * 9800 / 10000,
            ALICE,
            swap,
            tychoRouterAddr,
            CLIENT_FEE_RECEIVER_PK
        );

        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            STA_ADDR,
            quotedSTA,
            quotedSTA * 9800 / 10000,
            ALICE,
            feeParams,
            swap
        );

        // Pool transfer to router 1284621860562858120776317
        // router actually received 1271775641957229539568553
        // client takes 127177564195722953956
        // so ALICE should get 1271648464393033816614597
        // but actually gets 1258931979749103478448451

        assertEq(amountOut, 1258931979749103478448451);
        assertEq(IERC20(STA_ADDR).balanceOf(ALICE), amountOut);
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), 0);

        vm.stopPrank();
    }

    function testSingleSwapAppliesTxOriginCustomFee() public {
        // ALICE (tx.origin) has a custom 1% router fee that overrides the 2% default.
        // No client signature is provided, so tx.origin is used for the fee lookup.
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(routerFeeReceiver);
        feeCalculator.setRouterFeeOnOutput(2_000_000); // 2% default
        feeCalculator.setCustomRouterFeeOnOutput(ALICE, 1_000_000); // 1% override for ALICE
        vm.stopPrank();

        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        // ALICE is both msg.sender and tx.origin
        vm.startPrank(ALICE, ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes memory protocolData =
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);
        bytes memory swap =
            encodeSingleSwap(address(usv2Executor), protocolData);

        // quotedAmountOut = expected gross output; 1% fee is calculated on this
        uint256 quotedAmountOut = 2018817438608734439722;
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            quotedAmountOut,
            quotedAmountOut * 9800 / 10000,
            ALICE,
            noClientFee(),
            swap
        );
        vm.stopPrank();

        // 1 WETH -> 2018817438608734439722 DAI
        // ALICE's custom 1% fee is used, not the 2% default
        // 2% would be 40376348772174688794; 1% confirms the override
        uint256 expectedRouterFee = 20188174386087344397;
        uint256 expectedAmountOut = 1998629264222647095325;

        assertEq(amountOut, expectedAmountOut);
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), expectedAmountOut);
        assertEq(
            tychoRouter.balanceOf(
                routerFeeReceiver, uint256(uint160(DAI_ADDR))
            ),
            expectedRouterFee
        );
        // No client fee — address(0) vault stays empty
        assertEq(
            tychoRouter.balanceOf(address(0), uint256(uint160(DAI_ADDR))), 0
        );
    }

    function testSingleSwapSignatureTakesPrecedenceOverTxOrigin() public {
        // ALICE (tx.origin) has a custom 2% router fee.
        // A signed client fee for clientFeeReceiver is also provided.
        // The signed clientFeeReceiver's fee (default, 0%) should be used, not ALICE's 2%.
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(routerFeeReceiver);
        feeCalculator.setCustomRouterFeeOnOutput(ALICE, 2_000_000); // 2% for tx.origin
        vm.stopPrank();

        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE, ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);

        bytes memory protocolData =
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);
        bytes memory swap =
            encodeSingleSwap(address(usv2Executor), protocolData);

        uint256 expectedAmountOut = 1900 * 1e18;

        // Signed params: clientFeeReceiver has no custom fee (uses default 0%)
        ClientFeeParams memory feeParams = makeClientFeeParams(
            0,
            0,
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            expectedAmountOut,
            expectedAmountOut * 9800 / 10000,
            ALICE,
            swap,
            tychoRouterAddr,
            CLIENT_FEE_RECEIVER_PK
        );

        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            expectedAmountOut,
            expectedAmountOut * 9800 / 10000,
            ALICE,
            feeParams,
            swap
        );
        vm.stopPrank();

        // clientFeeReceiver (default, 0% router fee) was used — no router fee collected
        uint256 routerFeeBalance = tychoRouter.balanceOf(
            routerFeeReceiver, uint256(uint160(DAI_ADDR))
        );
        assertEq(
            routerFeeBalance,
            0,
            "ALICE tx.origin fee must not apply when signature present"
        );

        // Full output minus zero fees goes to ALICE
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), amountOut);
    }

    function testFeesExceedingOutputRevert() public {
        // A malicious or buggy FeeCalculator claiming more fees than the swap
        // produced must revert instead of underflowing the fee accounting.
        OverchargingFeeCalculator badCalc = new OverchargingFeeCalculator();
        vm.startPrank(FEE_SETTER);
        tychoRouter.setFeeCalculator(address(badCalc));
        vm.warp(block.timestamp + tychoRouter.DELAY_FEE_CALCULATOR_ACTIVATION());
        tychoRouter.activateFeeCalculator();
        vm.stopPrank();

        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, amountIn);
        bytes memory protocolData =
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);
        bytes memory swap =
            encodeSingleSwap(address(usv2Executor), protocolData);

        uint256 actualAmountOut = 2018817438608734439722;
        uint256 expectedAmountOut = 2000 * 1e18;
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__FeesExceedOutput.selector,
                actualAmountOut + 1,
                actualAmountOut
            )
        );
        tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            expectedAmountOut,
            expectedAmountOut * 9800 / 10000,
            ALICE,
            noClientFee(),
            swap
        );
        vm.stopPrank();
    }
}
