// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {TychoRouterTestSetup} from "../TychoRouterTestSetup.sol";
import {TestUtils} from "../TestUtils.sol";
import {ClientFeeParams} from "@src/TychoRouterV3.sol";
import {TychoRouter__NegativeSlippage} from "@src/TychoRouterV3.sol";
import {
    KipseliExecutor,
    IKipseliRouter
} from "@src/executors/KipseliExecutor.sol";
import {TransferManager} from "@src/TransferManager.sol";

/// @dev Executor tests that need no signed quote: decoding of the swap data, the transfer data,
///      the length check and the call the executor makes to the Kipseli Router.
contract KipseliExecutorTest is TestUtils {
    /// Any address: these tests never reach a deployed Kipseli Router.
    address constant KIPSELI_ROUTER = address(0x4B1B5E11);
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    /// The sender the placeholder quote names.
    address constant TEST_ROUTER = 0x6bc529DC7B81A031828dDCE2BC419d01FF268C66;
    /// The placeholder quote's timestamp.
    uint256 constant QUOTE_TIMESTAMP = 1_795_612_000;

    KipseliExecutor executor;

    function setUp() public {
        executor = new KipseliExecutor(KIPSELI_ROUTER);
    }

    /// @dev Placeholder verification data: fee 25, originRate 1e18, the test router, and a
    ///      65-byte signature of 0xab bytes.
    function _verificationData() internal pure returns (bytes memory) {
        bytes memory signature = new bytes(65);
        for (uint256 i; i < 65; ++i) {
            signature[i] = 0xab;
        }
        return abi.encode(uint256(25), uint256(1e18), TEST_ROUTER, signature);
    }

    function testConstructorRejectsZeroRouter() public {
        vm.expectRevert(KipseliExecutor.KipseliExecutor__InvalidRouter.selector);
        new KipseliExecutor(address(0));
    }

    /// @dev The swap data hosted Fynd's `pamm:kipseli` encoder writes for each direction is the
    ///      tokens, the timestamp word and the verification data, which the executor reads back
    ///      and forwards to the Kipseli Router with a one-wei floor.
    function testDecodesSwapData() public {
        _assertDecodes(WETH, USDC);
        _assertDecodes(USDC, WETH);
    }

    function _assertDecodes(address expectedTokenIn, address expectedTokenOut)
        internal
    {
        bytes memory data = abi.encodePacked(
            expectedTokenIn,
            expectedTokenOut,
            QUOTE_TIMESTAMP,
            _verificationData()
        );

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
        assertEq(receiver, KIPSELI_ROUTER, "the Kipseli Router pulls");
        assertEq(tokenIn, expectedTokenIn);
        assertEq(tokenOut, expectedTokenOut);
        assertTrue(outputToRouter, "the Kipseli Router pays its caller");

        uint256 amountIn = 1234;
        vm.mockCall(
            KIPSELI_ROUTER,
            abi.encodeWithSelector(IKipseliRouter.swap.selector),
            abi.encode(uint256(1))
        );
        vm.expectCall(
            KIPSELI_ROUTER,
            abi.encodeCall(
                IKipseliRouter.swap,
                (
                    expectedTokenIn,
                    amountIn,
                    expectedTokenOut,
                    1,
                    QUOTE_TIMESTAMP,
                    _verificationData()
                )
            )
        );
        executor.swap(amountIn, data, address(0xBEEF));
        vm.clearMockedCalls();
    }

    function testSelectorMatchesDeployedRouter() public pure {
        assertEq(IKipseliRouter.swap.selector, bytes4(0xecd47daa));
    }

    function testFundsExpectedAddressIsCaller() public view {
        assertEq(executor.fundsExpectedAddress(""), address(this));
    }

    /// @dev Swap data shorter than the header and the shortest verification data reverts by
    ///      name, in getTransferData and in swap.
    function testInvalidDataLength() public {
        bytes[3] memory invalid = [
            bytes(""),
            abi.encodePacked(WETH, USDC, QUOTE_TIMESTAMP),
            abi.encodePacked(WETH, USDC, QUOTE_TIMESTAMP, new bytes(159))
        ];
        for (uint256 i; i < invalid.length; ++i) {
            vm.expectRevert(
                KipseliExecutor.KipseliExecutor__InvalidDataLength.selector
            );
            executor.getTransferData(invalid[i]);
            vm.expectRevert(
                KipseliExecutor.KipseliExecutor__InvalidDataLength.selector
            );
            executor.swap(1, invalid[i], address(this));
        }
        // The shortest accepted length: the header and an empty signature's encoding.
        executor.getTransferData(
            abi.encodePacked(WETH, USDC, QUOTE_TIMESTAMP, new bytes(160))
        );
    }
}

/// @dev Router tests on Base through the deployed Kipseli Router, with quotes Kipseli's API signed
///      for the test TychoRouter (`userAddress` = `tychoRouterAddr`) at block 62,953,546. The swap
///      data is laid out as hosted Fynd's `pamm:kipseli` encoder writes it.
///
///      Kipseli's on-chain price is valid for less than 4 seconds, so the fork is that exact
///      block: at any later block the venue pays nothing and the swap reverts.
///
///      Kipseli whitelisted Tycho's Base router `PROD_ROUTER`, not the router these tests deploy.
///      `setUp` copies the two records that whitelisting wrote onto the test router: its signer
///      in the Verifier and its swap handler in `KIPSELI_HANDLERS`. Without the first the
///      venue pays nothing; without the second the swap reverts inside the venue.
contract TychoRouterForKipseliTest is TychoRouterTestSetup {
    address constant KIPSELI_ROUTER =
        0x71C2Ed90CC288229Be59F26b8B3EEF3C07d7ab99;
    /// @dev Kipseli's Verifier, which maps a whitelisted sender (slot 3) to its signer.
    address constant KIPSELI_VERIFIER =
        0xCa369e97cc161c3c3a7368f9bC55A47F36a0A91E;
    /// @dev The Kipseli contract that maps a whitelisted sender (slot 2) to the contract that
    ///      settles its swaps.
    address constant KIPSELI_HANDLERS =
        0xc2Be7B94d5498b1acEb914F3a1dec4Ce502d235F;
    /// @dev Tycho's Base router, which Kipseli whitelisted.
    address constant PROD_ROUTER = 0xd374c00614f95492D816b5957E3797ce3f0c445C;
    /// @dev Uniswap V3 WETH/USDC 0.05% on Base, the other leg of the split.
    address constant WETH_USDC_USV3 =
        0xd0b53D9277642d899DF5C87A3966A349A798F224;
    /// @dev The quotes, as the API returned them.
    uint256 constant WETH_USDC_QUOTE_TIMESTAMP = 1795781082;
    bytes constant WETH_USDC_VERIFICATION_DATA =
        hex"000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006bc529dc7b81a031828ddce2bc419d01ff268c6600000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000041f59695a3aa387782cc732ab562d045fbb6bc1b76ce12616b65e495cdbf5eea8d33543e1eb602cbe58378aa96845f0868f862fffdecfbca92cc3058a0d58f54b61c00000000000000000000000000000000000000000000000000000000000000";
    uint256 constant USDC_WETH_QUOTE_TIMESTAMP = 1795781083;
    bytes constant USDC_WETH_VERIFICATION_DATA =
        hex"000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006bc529dc7b81a031828ddce2bc419d01ff268c660000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000004185477212d8f27e6181e256611371b63c37f0d2555f68287dbf8eac0cc2bd66f153357a5fbf8523c1e4c81644dee5b9fff252e9c721754392f85eb622d752f97d1b00000000000000000000000000000000000000000000000000000000000000";
    uint256 constant USDC_CBBTC_QUOTE_TIMESTAMP = 1795781083;
    bytes constant USDC_CBBTC_VERIFICATION_DATA =
        hex"000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006bc529dc7b81a031828ddce2bc419d01ff268c66000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000411a5b212f382d41529375e81c2d3d8c58df76a6a72cea32bb4f2f0b0c4c5044755604d33f319bc9351544bec179ebea839a85474937075d863795e3b15d26e2691c00000000000000000000000000000000000000000000000000000000000000";
    address constant WETH = 0x4200000000000000000000000000000000000006;
    address constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant CBBTC = 0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf;
    /// @dev Derived from the name, so deployments the shared setup adds do not move it.
    address constant KIPSELI_EXECUTOR =
        address(uint160(uint256(keccak256("tycho.test.KipseliExecutor"))));

    uint24 constant SIXTY_PERCENT = uint24(uint256(0xffffff) * 60 / 100);
    uint256 constant WETH_IN = 1e16;
    uint256 constant USDC_IN = 35e6;

    KipseliExecutor kipseliExecutor;

    function setUp() public override {
        super.setUp();
        deployCodeTo(
            "KipseliExecutor.sol:KipseliExecutor",
            abi.encode(KIPSELI_ROUTER),
            KIPSELI_EXECUTOR
        );
        kipseliExecutor = KipseliExecutor(KIPSELI_EXECUTOR);
        address[] memory executors = new address[](1);
        executors[0] = KIPSELI_EXECUTOR;
        vm.warp(forkTimestamp - _SETUP_TIME_OFFSET_NEW_EXECUTOR);
        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.warp(forkTimestamp);

        _copyKipseliWhitelist(KIPSELI_VERIFIER, 3, PROD_ROUTER, tychoRouterAddr);
        _copyKipseliWhitelist(
            KIPSELI_VERIFIER,
            3,
            address(uint160(PROD_ROUTER) + 1),
            address(uint160(tychoRouterAddr) + 1)
        );
        _copyKipseliWhitelist(KIPSELI_HANDLERS, 2, PROD_ROUTER, tychoRouterAddr);
    }

    /// @dev Gives `to` the record `from` has in the address mapping at `slot` of `kipseli`.
    function _copyKipseliWhitelist(
        address kipseli,
        uint256 slot,
        address from,
        address to
    ) internal {
        bytes32 record = vm.load(kipseli, keccak256(abi.encode(from, slot)));
        assertTrue(
            record != bytes32(0), "Kipseli has not whitelisted the router"
        );
        vm.store(kipseli, keccak256(abi.encode(to, slot)), record);
    }

    /// @dev The executor's swap data for the WETH -> USDC quote.
    function _wethUsdcSwapData() internal pure returns (bytes memory) {
        return abi.encodePacked(
            WETH, USDC, WETH_USDC_QUOTE_TIMESTAMP, WETH_USDC_VERIFICATION_DATA
        );
    }

    function _usdcWethSwapData() internal pure returns (bytes memory) {
        return abi.encodePacked(
            USDC, WETH, USDC_WETH_QUOTE_TIMESTAMP, USDC_WETH_VERIFICATION_DATA
        );
    }

    function _usdcCbbtcSwapData() internal pure returns (bytes memory) {
        return abi.encodePacked(
            USDC,
            CBBTC,
            USDC_CBBTC_QUOTE_TIMESTAMP,
            USDC_CBBTC_VERIFICATION_DATA
        );
    }

    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        // The block the quotes were signed at.
        return 62_953_546;
    }

    /// @dev Checks BOB received `amountOut` of `tokenOut` for all of his `tokenIn`, and that the
    ///      router holds neither token and no approval of the Kipseli Router.
    function _assertSettled(
        address tokenIn,
        address tokenOut,
        uint256 amountOut,
        uint256 beforeBalance
    ) internal view {
        assertGt(amountOut, 0);
        assertEq(IERC20(tokenOut).balanceOf(BOB) - beforeBalance, amountOut);
        assertEq(IERC20(tokenIn).balanceOf(BOB), 0);
        assertEq(IERC20(tokenIn).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(tokenOut).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(tokenIn).allowance(tychoRouterAddr, KIPSELI_ROUTER), 0);
    }

    /// @dev Swaps BOB's `amountIn` of `tokenIn` through Kipseli alone.
    function _singleSwap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        bytes memory swapData
    ) internal {
        deal(tokenIn, BOB, amountIn);
        uint256 beforeBalance = IERC20(tokenOut).balanceOf(BOB);
        vm.startPrank(BOB);
        IERC20(tokenIn).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            tokenIn,
            tokenOut,
            1,
            1,
            BOB,
            noClientFee(),
            encodeSingleSwap(KIPSELI_EXECUTOR, swapData)
        );
        vm.stopPrank();
        _assertSettled(tokenIn, tokenOut, amountOut, beforeBalance);
    }

    function testSingleSwapWethToUsdc() public {
        _singleSwap(WETH, USDC, WETH_IN, _wethUsdcSwapData());
    }

    function testSingleSwapUsdcToWeth() public {
        _singleSwap(USDC, WETH, USDC_IN, _usdcWethSwapData());
    }

    /// @dev Kipseli as the second hop is funded only by the first hop's output, which the
    ///      Kipseli Router paid to the TychoRouter.
    function testSequentialSwapNonFirstHop() public {
        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(KIPSELI_EXECUTOR, _wethUsdcSwapData());
        swaps[1] = encodeSequentialSwap(KIPSELI_EXECUTOR, _usdcCbbtcSwapData());
        deal(WETH, BOB, WETH_IN);
        uint256 beforeBalance = IERC20(CBBTC).balanceOf(BOB);
        vm.startPrank(BOB);
        IERC20(WETH).approve(tychoRouterAddr, WETH_IN);
        uint256 amountOut = tychoRouter.sequentialSwap(
            WETH_IN, WETH, CBBTC, 1, 1, BOB, noClientFee(), pleEncode(swaps)
        );
        vm.stopPrank();
        _assertSettled(WETH, CBBTC, amountOut, beforeBalance);
        assertEq(IERC20(USDC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC).allowance(tychoRouterAddr, KIPSELI_ROUTER), 0);
    }

    /// @dev Kipseli trades 60% of a split and the Uniswap V3 WETH/USDC pool the rest: the Kipseli
    ///      leg is funded with a fraction of the input.
    function testSplitSwap() public {
        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSplitSwap(
            0, 1, SIXTY_PERCENT, KIPSELI_EXECUTOR, _wethUsdcSwapData()
        );
        swaps[1] = encodeSplitSwap(
            0,
            1,
            0,
            address(usv3Executor),
            encodeUniswapV3Swap(WETH, USDC, WETH_USDC_USV3, true)
        );
        deal(WETH, ALICE, WETH_IN);
        vm.startPrank(ALICE);
        IERC20(WETH).approve(tychoRouterAddr, WETH_IN);
        uint256 amountOut = tychoRouter.splitSwap(
            WETH_IN, WETH, USDC, 1, 1, 2, BOB, noClientFee(), pleEncode(swaps)
        );
        vm.stopPrank();
        assertGt(amountOut, 0);
        assertEq(IERC20(USDC).balanceOf(BOB), amountOut);
        assertEq(IERC20(WETH).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(WETH).allowance(tychoRouterAddr, KIPSELI_ROUTER), 0);
    }

    /// @dev Kipseli settles a signature once: the same quote on a second leg is paid nothing
    ///      and Kipseli keeps that leg's input. The executor's one-wei floor turns that into a
    ///      revert of the whole swap instead.
    function testReusedQuoteReverts() public {
        bytes memory data = _wethUsdcSwapData();
        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSplitSwap(0, 1, SIXTY_PERCENT, KIPSELI_EXECUTOR, data);
        swaps[1] = encodeSplitSwap(0, 1, 0, KIPSELI_EXECUTOR, data);
        deal(WETH, ALICE, WETH_IN);
        vm.startPrank(ALICE);
        IERC20(WETH).approve(tychoRouterAddr, WETH_IN);
        vm.expectRevert(bytes("return too low"));
        tychoRouter.splitSwap(
            WETH_IN, WETH, USDC, 1, 1, 2, BOB, noClientFee(), pleEncode(swaps)
        );
        vm.stopPrank();
        assertEq(IERC20(WETH).balanceOf(ALICE), WETH_IN);
    }

    /// @dev A client fee redirects the output through the router, where the fee stays as the
    ///      receiver's vault credit.
    function testSingleSwapWithClientFee() public {
        bytes memory swapData =
            encodeSingleSwap(KIPSELI_EXECUTOR, _wethUsdcSwapData());
        uint32 clientFeeBps = 1_000_000; // 1%
        uint256 expected = 17_220_171;
        ClientFeeParams memory clientFee = makeClientFeeParams(
            clientFeeBps,
            0,
            WETH_IN,
            WETH,
            USDC,
            expected,
            1,
            BOB,
            swapData,
            tychoRouterAddr,
            CLIENT_FEE_RECEIVER_PK
        );
        deal(WETH, ALICE, WETH_IN);
        vm.startPrank(ALICE);
        IERC20(WETH).approve(tychoRouterAddr, WETH_IN);
        uint256 amountOut = tychoRouter.singleSwap(
            WETH_IN, WETH, USDC, expected, 1, BOB, clientFee, swapData
        );
        vm.stopPrank();
        assertEq(IERC20(USDC).balanceOf(BOB), amountOut);
        uint256 fee =
            tychoRouter.balanceOf(clientFeeReceiver, uint256(uint160(USDC)));
        assertGt(fee, 0);
        assertEq(IERC20(USDC).balanceOf(tychoRouterAddr), fee);
        assertEq(IERC20(WETH).balanceOf(tychoRouterAddr), 0);
    }

    /// @dev The route's minAmountOut binds: a minimum above what Kipseli pays reverts.
    function testMinAmountOutBinds() public {
        bytes memory swapData =
            encodeSingleSwap(KIPSELI_EXECUTOR, _wethUsdcSwapData());
        uint256 snapshot = vm.snapshotState();
        deal(WETH, ALICE, WETH_IN);
        vm.startPrank(ALICE);
        IERC20(WETH).approve(tychoRouterAddr, WETH_IN);
        uint256 amountOut = tychoRouter.singleSwap(
            WETH_IN, WETH, USDC, 1, 1, BOB, noClientFee(), swapData
        );
        vm.stopPrank();
        vm.revertToState(snapshot);

        deal(WETH, ALICE, WETH_IN);
        vm.startPrank(ALICE);
        IERC20(WETH).approve(tychoRouterAddr, WETH_IN);
        vm.expectPartialRevert(TychoRouter__NegativeSlippage.selector);
        tychoRouter.singleSwap(
            WETH_IN,
            WETH,
            USDC,
            amountOut + 1,
            amountOut + 1,
            BOB,
            noClientFee(),
            swapData
        );
        vm.stopPrank();
    }
}
