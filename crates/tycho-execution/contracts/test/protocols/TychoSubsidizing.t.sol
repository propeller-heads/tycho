// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {ISubsidyPayer, Subsidy} from "@interfaces/ISubsidyPayer.sol";
import {
    Dispatcher__UnapprovedExecutor,
    Dispatcher__UnsupportedSingleHopCycle
} from "@src/Dispatcher.sol";
import {
    TychoSubsidizingExecutor,
    TychoSubsidizingExecutor__InvalidDataLength,
    TychoSubsidizingExecutor__UnapprovedInnerExecutor
} from "@src/executors/TychoSubsidizingExecutor.sol";
import {UniswapV4Executor} from "@src/executors/UniswapV4Executor.sol";
import {UniswapV4Utils} from "./UniswapV4Utils.sol";
import {SubsidyPayer} from "@src/SubsidyPayer.sol";

/// @dev Pays every subsidy it can afford and records the last call. Stands in
///      for SubsidyPayer, so it checks no signature, nonce or caller.
contract RecordingSubsidyPayer is ISubsidyPayer {
    using SafeERC20 for IERC20;

    address constant ETH = 0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE;

    uint256 public calls;
    Subsidy public lastSubsidy;
    uint256 public lastHopAmountIn;
    address public lastRouterMsgSender;
    address public lastTo;
    bytes public lastSignature;

    function pay(
        Subsidy calldata subsidy,
        uint256 hopAmountIn,
        address routerMsgSender,
        address to,
        bytes calldata signature
    ) external returns (bool paid) {
        calls++;
        lastSubsidy = subsidy;
        lastHopAmountIn = hopAmountIn;
        lastRouterMsgSender = routerMsgSender;
        lastTo = to;
        lastSignature = signature;

        if (subsidy.token == ETH) {
            if (address(this).balance < subsidy.amount) return false;
            (paid,) = to.call{value: subsidy.amount}("");
            return paid;
        }
        if (IERC20(subsidy.token).balanceOf(address(this)) < subsidy.amount) {
            return false;
        }
        IERC20(subsidy.token).safeTransfer(to, subsidy.amount);
        return true;
    }

    receive() external payable {}
}

contract RevertingSubsidyPayer is ISubsidyPayer {
    function pay(Subsidy calldata, uint256, address, address, bytes calldata)
        external
        pure
        returns (bool)
    {
        revert("payer down");
    }
}

/// @dev Calls the router's fallback from inside `pay`, as a contract paid by
///      the payer could, and records the revert data. Pays nothing.
contract CallbackProbingPayer is ISubsidyPayer {
    bytes public callbackRevertData;

    function pay(Subsidy calldata, uint256, address, address, bytes calldata)
        external
        returns (bool)
    {
        (bool success, bytes memory result) = msg.sender.call(hex"deadbeef");
        require(!success, "callback reached an executor");
        callbackRevertData = result;
        return false;
    }
}

abstract contract TychoSubsidizingTestBase is TychoRouterTestSetup {
    uint256 constant DAI_SUBSIDY = 5 ether;
    uint256 constant USDC_SUBSIDY = 5_000000;
    uint256 constant WETH_SUBSIDY = 0.002 ether;
    uint256 constant ETH_SUBSIDY = 0.002 ether;

    /// @dev Approves the executor with its timelock already passed, without
    ///      moving block.timestamp off the fork's.
    function _approvedWrapper(ISubsidyPayer payer_)
        internal
        returns (TychoSubsidizingExecutor executor)
    {
        executor = new TychoSubsidizingExecutor(payer_);
        address[] memory executors = new address[](1);
        executors[0] = address(executor);
        vm.warp(block.timestamp - tychoRouter.DELAY_EXECUTOR_ACTIVATION());
        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.warp(forkTimestamp);
    }

    function _singleSwap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address receiver,
        bytes memory swapData
    ) internal returns (uint256 amountOut) {
        amountOut = _singleSwapExpecting(
            tokenIn, tokenOut, amountIn, receiver, 1, 1, swapData
        );
    }

    /// @dev Output of a single swap sent by ALICE, with the state reverted
    ///      afterwards.
    function _baselineSingleSwap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address receiver,
        bytes memory swapData
    ) internal returns (uint256 amountOut) {
        uint256 snapshot = vm.snapshotState();
        amountOut = _singleSwap(tokenIn, tokenOut, amountIn, receiver, swapData);
        vm.revertToState(snapshot);
    }

    function _singleSwapExpecting(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address receiver,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        bytes memory swapData
    ) internal returns (uint256 amountOut) {
        uint256 value = _fundAlice(tokenIn, amountIn);
        vm.prank(ALICE);
        amountOut = tychoRouter.singleSwap{value: value}(
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
            minAmountOut,
            receiver,
            noClientFee(),
            swapData
        );
    }

    function _sequentialSwap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        bytes[] memory swaps
    ) internal returns (uint256 amountOut) {
        uint256 value = _fundAlice(tokenIn, amountIn);
        vm.prank(ALICE);
        amountOut = tychoRouter.sequentialSwap{value: value}(
            amountIn,
            tokenIn,
            tokenOut,
            1,
            1,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
    }

    function _splitSwap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 nTokens,
        bytes[] memory swaps
    ) internal returns (uint256 amountOut) {
        uint256 value = _fundAlice(tokenIn, amountIn);
        vm.prank(ALICE);
        amountOut = tychoRouter.splitSwap{value: value}(
            amountIn,
            tokenIn,
            tokenOut,
            1,
            1,
            nTokens,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
    }

    /// @return value The ETH to send with the swap.
    function _fundAlice(address token, uint256 amount)
        internal
        returns (uint256 value)
    {
        if (token == ETH_ADDR) {
            vm.deal(ALICE, amount);
            return amount;
        }
        deal(token, ALICE, amount);
        vm.prank(ALICE);
        IERC20(token).approve(tychoRouterAddr, amount);
    }

    function _usv2(address pool, address tokenIn, address tokenOut)
        internal
        pure
        returns (bytes memory)
    {
        return encodeUniswapV2Swap(pool, tokenIn, tokenOut);
    }

    function _usv4UsdcToEth() internal pure returns (bytes memory) {
        UniswapV4Executor.UniswapV4Pool[] memory pools =
            new UniswapV4Executor.UniswapV4Pool[](1);
        pools[0] = UniswapV4Executor.UniswapV4Pool({
            intermediaryToken: 0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE,
            fee: uint24(3000),
            tickSpacing: int24(60),
            hook: address(0),
            hookData: bytes("")
        });
        return UniswapV4Utils.encodeExactInput(
            0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48,
            0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE,
            false,
            false,
            pools
        );
    }
}

contract TychoSubsidizingExecutorTest is TychoSubsidizingTestBase {
    uint256 constant NONCE = 42;
    uint256 constant MIN_AMOUNT_IN = 1;
    uint24 constant SEVENTY_PERCENT = uint24(uint256(0xffffff) * 70 / 100);

    RecordingSubsidyPayer payer;
    TychoSubsidizingExecutor wrapper;
    bytes signature;

    function setUp() public override {
        super.setUp();
        payer = new RecordingSubsidyPayer();
        wrapper = _approvedWrapper(ISubsidyPayer(address(payer)));
        signature = abi.encodePacked(
            bytes32(uint256(1)), bytes32(uint256(2)), uint8(27)
        );

        deal(DAI_ADDR, address(payer), 100 ether);
        deal(USDC_ADDR, address(payer), 100_000000);
        deal(WETH_ADDR, address(payer), 1 ether);
        vm.deal(address(payer), 1 ether);
    }

    function _subsidized(address inner, bytes memory innerData, uint256 amount)
        internal
        view
        returns (bytes memory)
    {
        return abi.encodePacked(
            ALICE,
            amount,
            MIN_AMOUNT_IN,
            NONCE,
            uint48(block.timestamp + 1 hours),
            signature,
            inner,
            innerData
        );
    }

    function testUniswapV2LastHopPaysSubsidyToReceiver() public {
        uint256 amountIn = 1 ether;
        bytes memory innerData = _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);

        uint256 baseline = _baselineSingleSwap(
            WETH_ADDR,
            DAI_ADDR,
            amountIn,
            BOB,
            encodeSingleSwap(address(usv2Executor), innerData)
        );

        uint256 amountOut = _singleSwap(
            WETH_ADDR,
            DAI_ADDR,
            amountIn,
            BOB,
            encodeSingleSwap(
                address(wrapper),
                _subsidized(address(usv2Executor), innerData, DAI_SUBSIDY)
            )
        );

        assertEq(amountOut, baseline + DAI_SUBSIDY);
        assertEq(IERC20(DAI_ADDR).balanceOf(BOB), baseline + DAI_SUBSIDY);
        assertEq(payer.lastTo(), BOB);
    }

    function testWrapperPassesSubsidyFieldsToPayer() public {
        uint256 amountIn = 1 ether;
        bytes memory innerData = _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);

        _singleSwap(
            WETH_ADDR,
            DAI_ADDR,
            amountIn,
            BOB,
            encodeSingleSwap(
                address(wrapper),
                _subsidized(address(usv2Executor), innerData, DAI_SUBSIDY)
            )
        );

        (
            address boundTo,
            address token,
            uint256 amount,
            uint256 minAmountIn,
            bytes32 innerSwapHash,
            uint256 nonce,
            uint256 deadline
        ) = payer.lastSubsidy();
        assertEq(payer.calls(), 1);
        assertEq(boundTo, ALICE);
        assertEq(token, DAI_ADDR);
        assertEq(amount, DAI_SUBSIDY);
        assertEq(minAmountIn, MIN_AMOUNT_IN);
        assertEq(
            innerSwapHash,
            keccak256(abi.encodePacked(address(usv2Executor), innerData))
        );
        assertEq(nonce, NONCE);
        assertEq(deadline, block.timestamp + 1 hours);
        assertEq(payer.lastHopAmountIn(), amountIn);
        assertEq(payer.lastRouterMsgSender(), ALICE);
        assertEq(payer.lastSignature(), signature);
    }

    function testSubsidyAmountAboveUint128() public {
        uint256 amount = uint256(type(uint128).max) + 1;

        _singleSwap(
            WETH_ADDR,
            DAI_ADDR,
            1 ether,
            BOB,
            encodeSingleSwap(
                address(wrapper),
                _subsidized(
                    address(usv2Executor),
                    _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR),
                    amount
                )
            )
        );

        (,, uint256 forwardedAmount,,,,) = payer.lastSubsidy();
        assertEq(forwardedAmount, amount);
    }

    function testOutputToRouterInnerPaysRouterAndForwardsToReceiver() public {
        uint256 amountIn = 1 ether;
        bytes memory wrapData = hex"01";

        uint256 amountOut = _singleSwap(
            ETH_ADDR,
            WETH_ADDR,
            amountIn,
            BOB,
            encodeSingleSwap(
                address(wrapper),
                _subsidized(address(nativeWrapExecutor), wrapData, WETH_SUBSIDY)
            )
        );

        assertEq(payer.lastTo(), tychoRouterAddr);
        assertEq(amountOut, amountIn + WETH_SUBSIDY);
        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), amountIn + WETH_SUBSIDY);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testUniswapV3CallbackReachesInnerExecutor() public {
        uint256 amountIn = 1 ether;
        bytes memory innerData =
            encodeUniswapV3Swap(WETH_ADDR, USDC_ADDR, USDC_WETH_USV3, false);

        uint256 baseline = _baselineSingleSwap(
            WETH_ADDR,
            USDC_ADDR,
            amountIn,
            ALICE,
            encodeSingleSwap(address(usv3Executor), innerData)
        );

        uint256 amountOut = _singleSwap(
            WETH_ADDR,
            USDC_ADDR,
            amountIn,
            ALICE,
            encodeSingleSwap(
                address(wrapper),
                _subsidized(address(usv3Executor), innerData, USDC_SUBSIDY)
            )
        );

        assertEq(amountOut, baseline + USDC_SUBSIDY);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), baseline + USDC_SUBSIDY);
    }

    function testUniswapV4EthSubsidyToReceiverWithoutIntercept() public {
        uint256 amountIn = 3000_000000;
        bytes memory innerData = _usv4UsdcToEth();

        uint256 baseline = _baselineSingleSwap(
            USDC_ADDR,
            ETH_ADDR,
            amountIn,
            BOB,
            encodeSingleSwap(address(usv4Executor), innerData)
        );

        uint256 amountOut = _singleSwap(
            USDC_ADDR,
            ETH_ADDR,
            amountIn,
            BOB,
            encodeSingleSwap(
                address(wrapper),
                _subsidized(address(usv4Executor), innerData, ETH_SUBSIDY)
            )
        );

        assertEq(payer.lastTo(), BOB);
        assertEq(amountOut, baseline + ETH_SUBSIDY);
        assertEq(BOB.balance, baseline + ETH_SUBSIDY);
        assertEq(tychoRouterAddr.balance, 0);
    }

    function testUniswapV4EthSubsidyToRouterWhenIntercepting() public {
        uint256 amountIn = 3000_000000;
        uint32 clientFeeBps = 1_000_000; // 1%
        bytes memory swapData = encodeSingleSwap(
            address(wrapper),
            _subsidized(address(usv4Executor), _usv4UsdcToEth(), ETH_SUBSIDY)
        );

        uint256 total =
            _baselineSingleSwap(USDC_ADDR, ETH_ADDR, amountIn, BOB, swapData);

        ClientFeeParams memory clientFee = makeClientFeeParams(
            clientFeeBps,
            0,
            amountIn,
            USDC_ADDR,
            ETH_ADDR,
            total,
            1,
            BOB,
            swapData,
            tychoRouterAddr,
            CLIENT_FEE_RECEIVER_PK
        );
        _fundAlice(USDC_ADDR, amountIn);
        vm.prank(ALICE);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn, USDC_ADDR, ETH_ADDR, total, 1, BOB, clientFee, swapData
        );

        uint256 fee = total * clientFeeBps / 100_000_000;
        assertEq(payer.lastTo(), tychoRouterAddr);
        assertEq(amountOut, total - fee);
        assertEq(BOB.balance, total - fee);
        assertEq(
            tychoRouter.balanceOf(
                clientFeeReceiver, uint256(uint160(ETH_ADDR))
            ),
            fee
        );
        assertEq(tychoRouterAddr.balance, fee);
    }

    function _splitWethToDai(bool subsidizeLargerLeg)
        internal
        view
        returns (bytes[] memory swaps)
    {
        bytes memory usv2Data = _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);
        bytes memory usv3Data =
            encodeUniswapV3Swap(WETH_ADDR, DAI_ADDR, DAI_WETH_USV3, false);

        swaps = new bytes[](2);
        swaps[0] = encodeSplitSwap(
            0, 1, SEVENTY_PERCENT, address(usv2Executor), usv2Data
        );
        swaps[1] = encodeSplitSwap(0, 1, 0, address(usv3Executor), usv3Data);
        if (subsidizeLargerLeg) {
            swaps[0] = encodeSplitSwap(
                0,
                1,
                SEVENTY_PERCENT,
                address(wrapper),
                _subsidized(address(usv2Executor), usv2Data, DAI_SUBSIDY)
            );
        }
    }

    function _splitWethToDaiSubsidizingSmallerLeg()
        internal
        view
        returns (bytes[] memory swaps)
    {
        swaps = _splitWethToDai(false);
        swaps[1] = encodeSplitSwap(
            0,
            1,
            0,
            address(wrapper),
            _subsidized(
                address(usv3Executor),
                encodeUniswapV3Swap(WETH_ADDR, DAI_ADDR, DAI_WETH_USV3, false),
                DAI_SUBSIDY
            )
        );
    }

    function testSplitSwapLargerLegSubsidized() public {
        uint256 amountIn = 1 ether;

        uint256 snapshot = vm.snapshotState();
        uint256 baseline = _splitSwap(
            WETH_ADDR, DAI_ADDR, amountIn, 2, _splitWethToDai(false)
        );
        vm.revertToState(snapshot);

        uint256 amountOut =
            _splitSwap(WETH_ADDR, DAI_ADDR, amountIn, 2, _splitWethToDai(true));

        assertEq(amountOut, baseline + DAI_SUBSIDY);
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), baseline + DAI_SUBSIDY);
    }

    function testSplitSwapSmallerLegSubsidized() public {
        uint256 amountIn = 1 ether;

        uint256 snapshot = vm.snapshotState();
        uint256 baseline = _splitSwap(
            WETH_ADDR, DAI_ADDR, amountIn, 2, _splitWethToDai(false)
        );
        vm.revertToState(snapshot);

        uint256 amountOut = _splitSwap(
            WETH_ADDR,
            DAI_ADDR,
            amountIn,
            2,
            _splitWethToDaiSubsidizingSmallerLeg()
        );

        assertEq(amountOut, baseline + DAI_SUBSIDY);
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), baseline + DAI_SUBSIDY);
    }

    function _splitThroughIntermediates(bool subsidizeDaiLeg)
        internal
        view
        returns (bytes[] memory swaps)
    {
        //          ->   WBTC  ->
        // WETH                     USDC
        //          ->   DAI   ->   (subsidized)
        bytes memory daiToUsdc = _usv2(DAI_USDC_POOL, DAI_ADDR, USDC_ADDR);
        swaps = new bytes[](4);
        swaps[0] = encodeSplitSwap(
            0,
            1,
            (0xffffff * 60) / 100,
            address(usv2Executor),
            _usv2(WETH_WBTC_POOL, WETH_ADDR, WBTC_ADDR)
        );
        swaps[1] = encodeSplitSwap(
            1,
            3,
            0,
            address(usv2Executor),
            _usv2(USDC_WBTC_POOL, WBTC_ADDR, USDC_ADDR)
        );
        swaps[2] = encodeSplitSwap(
            0,
            2,
            0,
            address(usv2Executor),
            _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );
        swaps[3] = subsidizeDaiLeg
            ? encodeSplitSwap(
                2,
                3,
                0,
                address(wrapper),
                _subsidized(address(usv2Executor), daiToUsdc, USDC_SUBSIDY)
            )
            : encodeSplitSwap(2, 3, 0, address(usv2Executor), daiToUsdc);
    }

    function testSplitSwapSubsidizedLegAfterIntermediate() public {
        uint256 amountIn = 1 ether;

        uint256 snapshot = vm.snapshotState();
        uint256 baseline = _splitSwap(
            WETH_ADDR, USDC_ADDR, amountIn, 4, _splitThroughIntermediates(false)
        );
        vm.revertToState(snapshot);

        uint256 amountOut = _splitSwap(
            WETH_ADDR, USDC_ADDR, amountIn, 4, _splitThroughIntermediates(true)
        );

        assertEq(amountOut, baseline + USDC_SUBSIDY);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), baseline + USDC_SUBSIDY);
    }

    function testSequentialSwapPaysIntoInnerPool() public {
        uint256 amountIn = 1 ether;
        bytes memory daiToUsdc = _usv2(DAI_USDC_POOL, DAI_ADDR, USDC_ADDR);
        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(
            address(usv2Executor),
            _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );
        swaps[1] = encodeSequentialSwap(address(usv2Executor), daiToUsdc);

        uint256 snapshot = vm.snapshotState();
        uint256 baseline =
            _sequentialSwap(WETH_ADDR, USDC_ADDR, amountIn, swaps);
        vm.revertToState(snapshot);

        swaps[1] = encodeSequentialSwap(
            address(wrapper),
            _subsidized(address(usv2Executor), daiToUsdc, USDC_SUBSIDY)
        );
        uint256 amountOut =
            _sequentialSwap(WETH_ADDR, USDC_ADDR, amountIn, swaps);

        assertEq(amountOut, baseline + USDC_SUBSIDY);
        assertEq(IERC20(USDC_ADDR).balanceOf(ALICE), baseline + USDC_SUBSIDY);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testSequentialSwapIntoSubsidizedUniswapV3Hop() public {
        uint256 amountIn = 3000 ether;
        bytes memory wethToUsdc =
            encodeUniswapV3Swap(WETH_ADDR, USDC_ADDR, USDC_WETH_USV3, false);
        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(
            address(usv2Executor),
            _usv2(DAI_WETH_UNIV2_POOL, DAI_ADDR, WETH_ADDR)
        );
        swaps[1] = encodeSequentialSwap(address(usv3Executor), wethToUsdc);

        uint256 snapshot = vm.snapshotState();
        uint256 baseline = _sequentialSwap(DAI_ADDR, USDC_ADDR, amountIn, swaps);
        vm.revertToState(snapshot);

        swaps[1] = encodeSequentialSwap(
            address(wrapper),
            _subsidized(address(usv3Executor), wethToUsdc, USDC_SUBSIDY)
        );
        uint256 amountOut =
            _sequentialSwap(DAI_ADDR, USDC_ADDR, amountIn, swaps);

        assertEq(amountOut, baseline + USDC_SUBSIDY);
        assertEq(IERC20(WETH_ADDR).balanceOf(address(wrapper)), 0);
    }

    function testViewsCalledByRouterForInnerReportingItsCaller() public {
        bytes memory data =
            _subsidized(address(nativeWrapExecutor), hex"01", WETH_SUBSIDY);

        vm.startPrank(tychoRouterAddr);
        address fundsExpected = wrapper.fundsExpectedAddress(data);
        (, address receiver,,,) = wrapper.getTransferData(data);
        vm.stopPrank();

        assertEq(fundsExpected, tychoRouterAddr);
        assertEq(receiver, tychoRouterAddr);
    }

    function testClientFeeAndSlippageCaptureWithSubsidy() public {
        uint256 amountIn = 1 ether;
        uint32 clientFeeBps = 1_000_000; // 1%
        bytes memory innerData = _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);
        bytes memory swapData = encodeSingleSwap(
            address(wrapper),
            _subsidized(address(usv2Executor), innerData, DAI_SUBSIDY)
        );

        uint256 baseline = _baselineSingleSwap(
            WETH_ADDR,
            DAI_ADDR,
            amountIn,
            BOB,
            encodeSingleSwap(address(usv2Executor), innerData)
        );

        vm.prank(FEE_SETTER);
        feeCalculator.setPositiveSlippageEnabled(true);
        uint256 expectedAmountOut = baseline + DAI_SUBSIDY;
        ClientFeeParams memory clientFee = makeClientFeeParams(
            clientFeeBps,
            0,
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            expectedAmountOut,
            1,
            BOB,
            swapData,
            tychoRouterAddr,
            CLIENT_FEE_RECEIVER_PK
        );
        _fundAlice(WETH_ADDR, amountIn);
        vm.prank(ALICE);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            WETH_ADDR,
            DAI_ADDR,
            expectedAmountOut,
            1,
            BOB,
            clientFee,
            swapData
        );

        uint256 fee = expectedAmountOut * clientFeeBps / 100_000_000;
        uint256 daiId = uint256(uint160(DAI_ADDR));
        assertEq(amountOut, expectedAmountOut - fee);
        assertEq(IERC20(DAI_ADDR).balanceOf(BOB), expectedAmountOut - fee);
        assertEq(tychoRouter.balanceOf(clientFeeReceiver, daiId), fee);
        assertEq(tychoRouter.balanceOf(routerFeeReceiver, daiId), 0);
    }

    function testSlippageCaptureQuoteExcludingSubsidy() public {
        uint256 amountIn = 1 ether;
        bytes memory innerData = _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);

        uint256 baseline = _baselineSingleSwap(
            WETH_ADDR,
            DAI_ADDR,
            amountIn,
            BOB,
            encodeSingleSwap(address(usv2Executor), innerData)
        );

        vm.prank(FEE_SETTER);
        feeCalculator.setPositiveSlippageEnabled(true);
        uint256 amountOut = _singleSwapExpecting(
            WETH_ADDR,
            DAI_ADDR,
            amountIn,
            BOB,
            baseline,
            1,
            encodeSingleSwap(
                address(wrapper),
                _subsidized(address(usv2Executor), innerData, DAI_SUBSIDY)
            )
        );

        assertEq(amountOut, baseline);
        assertEq(IERC20(DAI_ADDR).balanceOf(BOB), baseline);
        assertEq(
            tychoRouter.balanceOf(
                routerFeeReceiver, uint256(uint160(DAI_ADDR))
            ),
            DAI_SUBSIDY
        );
    }

    function testUnwrapAfterSubsidizedHop() public {
        uint256 amountIn = 3000 ether;
        bytes memory daiToWeth = _usv2(DAI_WETH_UNIV2_POOL, DAI_ADDR, WETH_ADDR);
        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(address(usv2Executor), daiToWeth);
        swaps[1] = encodeSequentialSwap(address(nativeWrapExecutor), hex"00");

        uint256 snapshot = vm.snapshotState();
        uint256 baseline = _sequentialSwap(DAI_ADDR, ETH_ADDR, amountIn, swaps);
        vm.revertToState(snapshot);

        swaps[0] = encodeSequentialSwap(
            address(wrapper),
            _subsidized(address(usv2Executor), daiToWeth, WETH_SUBSIDY)
        );
        uint256 aliceEthBefore = ALICE.balance;
        uint256 amountOut = _sequentialSwap(DAI_ADDR, ETH_ADDR, amountIn, swaps);

        (, address token,,,,,) = payer.lastSubsidy();
        assertEq(token, WETH_ADDR);
        assertEq(amountOut, baseline + WETH_SUBSIDY);
        assertEq(ALICE.balance - aliceEthBefore, baseline + WETH_SUBSIDY);
    }

    function testWrapAfterSubsidizedEthHop() public {
        uint256 amountIn = 3000_000000;
        bytes memory usdcToEth = _usv4UsdcToEth();
        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(address(usv4Executor), usdcToEth);
        swaps[1] = encodeSequentialSwap(address(nativeWrapExecutor), hex"01");

        uint256 snapshot = vm.snapshotState();
        uint256 baseline =
            _sequentialSwap(USDC_ADDR, WETH_ADDR, amountIn, swaps);
        vm.revertToState(snapshot);

        swaps[0] = encodeSequentialSwap(
            address(wrapper),
            _subsidized(address(usv4Executor), usdcToEth, ETH_SUBSIDY)
        );
        uint256 amountOut =
            _sequentialSwap(USDC_ADDR, WETH_ADDR, amountIn, swaps);

        (, address token,,,,,) = payer.lastSubsidy();
        assertEq(token, ETH_ADDR);
        assertEq(amountOut, baseline + ETH_SUBSIDY);
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), baseline + ETH_SUBSIDY);
    }

    function _splitDaiToEth(bool subsidizeLargestLeg)
        internal
        view
        returns (bytes[] memory swaps)
    {
        //       ->  (USV2, 70%, subsidized)  ->
        // DAI                                    WETH  -> unwrap ->  ETH
        //       ->  (USV3, rest)             ->
        bytes memory usv2Data = _usv2(DAI_WETH_UNIV2_POOL, DAI_ADDR, WETH_ADDR);
        swaps = new bytes[](3);
        swaps[0] = subsidizeLargestLeg
            ? encodeSplitSwap(
                0,
                1,
                SEVENTY_PERCENT,
                address(wrapper),
                _subsidized(address(usv2Executor), usv2Data, WETH_SUBSIDY)
            )
            : encodeSplitSwap(
                0, 1, SEVENTY_PERCENT, address(usv2Executor), usv2Data
            );
        swaps[1] = encodeSplitSwap(
            0,
            1,
            0,
            address(usv3Executor),
            encodeUniswapV3Swap(DAI_ADDR, WETH_ADDR, DAI_WETH_USV3, true)
        );
        swaps[2] =
            encodeSplitSwap(1, 2, 0, address(nativeWrapExecutor), hex"00");
    }

    function testSplitEndingInUnwrapCarriesSubsidy() public {
        uint256 amountIn = 3000 ether;

        uint256 snapshot = vm.snapshotState();
        uint256 baseline =
            _splitSwap(DAI_ADDR, ETH_ADDR, amountIn, 3, _splitDaiToEth(false));
        vm.revertToState(snapshot);

        uint256 aliceEthBefore = ALICE.balance;
        uint256 amountOut =
            _splitSwap(DAI_ADDR, ETH_ADDR, amountIn, 3, _splitDaiToEth(true));

        assertEq(amountOut, baseline + WETH_SUBSIDY);
        assertEq(ALICE.balance - aliceEthBefore, baseline + WETH_SUBSIDY);
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), 0);
    }

    function testVaultCreditIncludesSubsidy() public {
        uint256 amountIn = 1 ether;
        bytes memory innerData = _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);

        uint256 baseline = _baselineSingleSwap(
            WETH_ADDR,
            DAI_ADDR,
            amountIn,
            tychoRouterAddr,
            encodeSingleSwap(address(usv2Executor), innerData)
        );

        _singleSwap(
            WETH_ADDR,
            DAI_ADDR,
            amountIn,
            tychoRouterAddr,
            encodeSingleSwap(
                address(wrapper),
                _subsidized(address(usv2Executor), innerData, DAI_SUBSIDY)
            )
        );

        uint256 vaultBalance =
            tychoRouter.balanceOf(ALICE, uint256(uint160(DAI_ADDR)));
        assertEq(vaultBalance, baseline + DAI_SUBSIDY);
        assertEq(IERC20(DAI_ADDR).balanceOf(tychoRouterAddr), vaultBalance);
    }

    function testSubsidyLiftsOutputAboveMinAmountOut() public {
        uint256 amountIn = 1 ether;
        bytes memory innerData = _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);

        uint256 baseline = _baselineSingleSwap(
            WETH_ADDR,
            DAI_ADDR,
            amountIn,
            BOB,
            encodeSingleSwap(address(usv2Executor), innerData)
        );

        uint256 amountOut = _singleSwapExpecting(
            WETH_ADDR,
            DAI_ADDR,
            amountIn,
            BOB,
            baseline + DAI_SUBSIDY,
            baseline + 1,
            encodeSingleSwap(
                address(wrapper),
                _subsidized(address(usv2Executor), innerData, DAI_SUBSIDY)
            )
        );

        assertEq(amountOut, baseline + DAI_SUBSIDY);
    }

    function testPayerRevert() public {
        TychoSubsidizingExecutor revertingWrapper = _approvedWrapper(
            ISubsidyPayer(address(new RevertingSubsidyPayer()))
        );
        uint256 amountIn = 1 ether;
        bytes memory innerData = _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);

        uint256 baseline = _baselineSingleSwap(
            WETH_ADDR,
            DAI_ADDR,
            amountIn,
            BOB,
            encodeSingleSwap(address(usv2Executor), innerData)
        );

        bytes memory swapData = encodeSingleSwap(
            address(revertingWrapper),
            _subsidized(address(usv2Executor), innerData, DAI_SUBSIDY)
        );
        _fundAlice(WETH_ADDR, amountIn);
        vm.expectEmit(tychoRouterAddr);
        emit TychoSubsidizingExecutor.SubsidyPayerReverted(
            NONCE, abi.encodeWithSignature("Error(string)", "payer down")
        );
        vm.prank(ALICE);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn, WETH_ADDR, DAI_ADDR, 1, 1, BOB, noClientFee(), swapData
        );

        assertEq(amountOut, baseline);
        assertEq(IERC20(DAI_ADDR).balanceOf(BOB), baseline);
    }

    function testRouterCallbackDuringPayment() public {
        CallbackProbingPayer probingPayer = new CallbackProbingPayer();
        TychoSubsidizingExecutor probingWrapper =
            _approvedWrapper(ISubsidyPayer(address(probingPayer)));

        _singleSwap(
            WETH_ADDR,
            DAI_ADDR,
            1 ether,
            BOB,
            encodeSingleSwap(
                address(probingWrapper),
                _subsidized(
                    address(usv2Executor),
                    _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR),
                    DAI_SUBSIDY
                )
            )
        );

        assertEq(
            probingPayer.callbackRevertData(),
            abi.encodeWithSelector(
                Dispatcher__UnapprovedExecutor.selector, address(0)
            )
        );
    }

    function _expectInnerRejected(address inner) internal {
        bytes memory swapData = encodeSingleSwap(
            address(wrapper),
            _subsidized(
                inner,
                _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR),
                DAI_SUBSIDY
            )
        );
        _fundAlice(WETH_ADDR, 1 ether);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoSubsidizingExecutor__UnapprovedInnerExecutor.selector,
                inner
            )
        );
        vm.prank(ALICE);
        tychoRouter.singleSwap(
            1 ether, WETH_ADDR, DAI_ADDR, 1, 1, BOB, noClientFee(), swapData
        );
    }

    function testUnapprovedInnerExecutor() public {
        _expectInnerRejected(address(new UniswapV2Executor(30)));
    }

    function testTimelockedInnerExecutor() public {
        address inner = address(new UniswapV2Executor(30));
        address[] memory executors = new address[](1);
        executors[0] = inner;
        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);

        _expectInnerRejected(inner);
    }

    function testWrapperAsInnerExecutor() public {
        _expectInnerRejected(address(wrapper));
    }

    function testSwapDelegatecalledWithUnapprovedInner() public {
        address inner = address(new UniswapV2Executor(30));
        SwapDelegatingRouter router = new SwapDelegatingRouter();
        bytes memory data = _subsidized(
            inner, _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR), DAI_SUBSIDY
        );

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoSubsidizingExecutor__UnapprovedInnerExecutor.selector,
                inner
            )
        );
        router.delegateSwap(address(wrapper), data);
    }

    function testSingleHopCycle() public {
        bytes memory swapData = encodeSingleSwap(
            address(wrapper),
            _subsidized(
                address(usv2Executor),
                _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, WETH_ADDR),
                WETH_SUBSIDY
            )
        );
        _fundAlice(WETH_ADDR, 1 ether);
        vm.expectRevert(
            abi.encodeWithSelector(
                Dispatcher__UnsupportedSingleHopCycle.selector, WETH_ADDR
            )
        );
        vm.prank(ALICE);
        tychoRouter.singleSwap(
            1 ether, WETH_ADDR, WETH_ADDR, 1, 1, BOB, noClientFee(), swapData
        );
    }

    function testDataShorterThanFixedPart() public {
        bytes memory swapData = encodeSingleSwap(
            address(wrapper), abi.encodePacked(ALICE, DAI_SUBSIDY)
        );
        _fundAlice(WETH_ADDR, 1 ether);
        vm.expectRevert(TychoSubsidizingExecutor__InvalidDataLength.selector);
        vm.prank(ALICE);
        tychoRouter.singleSwap(
            1 ether, WETH_ADDR, DAI_ADDR, 1, 1, BOB, noClientFee(), swapData
        );
    }
}

/// @dev An integrator contract that calls the router for whoever sends the
///      transaction.
/// @dev Delegatecalls an executor's swap without calling getTransferData
///      first, and approves no executor.
contract SwapDelegatingRouter {
    mapping(address => uint256) public executorsActivationTimestamp;

    function delegateSwap(address executor, bytes calldata data) external {
        // slither-disable-next-line controlled-delegatecall,low-level-calls
        (bool success, bytes memory result) = executor.delegatecall(
            abi.encodeCall(IExecutor.swap, (1 ether, data, address(this)))
        );
        if (!success) {
            // slither-disable-next-line assembly
            assembly {
                revert(add(result, 0x20), mload(result))
            }
        }
    }
}

contract TestIntegrator {
    function swap(
        TychoRouterV3 router,
        address tokenIn,
        uint256 amountIn,
        address tokenOut,
        address receiver,
        bytes calldata swapData
    ) external returns (uint256) {
        IERC20(tokenIn).approve(address(router), amountIn);
        return router.singleSwap(
            amountIn,
            tokenIn,
            tokenOut,
            1,
            1,
            receiver,
            ClientFeeParams({
                clientFeeBps: 0,
                clientFeeReceiver: address(0),
                maxClientContribution: 0,
                deadline: 0,
                clientSignature: ""
            }),
            swapData
        );
    }
}

contract EthRejectingReceiver {
    receive() external payable {
        revert("no ETH");
    }
}

contract TychoSubsidizingWithPayerTest is TychoSubsidizingTestBase {
    uint256 constant NONCE = 7;
    uint256 constant AMOUNT_IN = 1 ether;
    bytes32 constant DOMAIN_TYPEHASH = keccak256(
        "EIP712Domain(string name,string version,uint256 chainId,"
        "address verifyingContract)"
    );

    SubsidyPayer payer;
    TychoSubsidizingExecutor wrapper;
    TestIntegrator integrator;
    address subsidySigner;
    uint256 subsidySignerPk;
    address subsidyPauser = makeAddr("subsidyPauser");
    address budgetManager = makeAddr("budgetManager");
    address relayer = makeAddr("relayer");
    bytes usv2Data;

    function setUp() public override {
        super.setUp();
        (subsidySigner, subsidySignerPk) = makeAddrAndKey("subsidySigner");
        address[] memory pausers = new address[](1);
        pausers[0] = subsidyPauser;
        payer = new SubsidyPayer(
            tychoRouterAddr,
            subsidySigner,
            SubsidyPayer.Roles({
                signerSetter: makeAddr("signerSetter"),
                budgetManager: budgetManager,
                pauserAdmin: makeAddr("pauserAdmin"),
                unpauser: makeAddr("unpauser"),
                pausers: pausers
            })
        );
        wrapper = _approvedWrapper(ISubsidyPayer(address(payer)));
        integrator = new TestIntegrator();
        usv2Data = _usv2(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR);

        deal(DAI_ADDR, address(payer), 100 ether);
        vm.deal(address(payer), 1 ether);
    }

    function _subsidy(
        address boundTo,
        address token,
        uint256 amount,
        address inner,
        bytes memory innerData
    ) internal view returns (Subsidy memory) {
        return Subsidy({
            boundTo: boundTo,
            token: token,
            amount: amount,
            minAmountIn: 1,
            innerSwapHash: keccak256(abi.encodePacked(inner, innerData)),
            nonce: NONCE,
            deadline: block.timestamp + 1 hours
        });
    }

    /// @dev Signs `s` with the subsidy signer's key. Builds the EIP-712 digest
    ///      by hand, so the tests also pin the layout the payer verifies.
    function _sign(Subsidy memory s) internal view returns (bytes memory) {
        bytes32 domainSeparator = keccak256(
            abi.encode(
                DOMAIN_TYPEHASH,
                keccak256("TychoSubsidyPayer"),
                keccak256("1"),
                block.chainid,
                address(payer)
            )
        );
        bytes32 structHash = keccak256(
            abi.encode(
                payer.SUBSIDY_TYPEHASH(),
                s.boundTo,
                s.token,
                s.amount,
                s.minAmountIn,
                s.innerSwapHash,
                s.nonce,
                s.deadline
            )
        );
        (uint8 v, bytes32 r, bytes32 sigS) = vm.sign(
            subsidySignerPk,
            keccak256(abi.encodePacked("\x19\x01", domainSeparator, structHash))
        );
        return abi.encodePacked(r, sigS, v);
    }

    function _encode(
        Subsidy memory s,
        bytes memory signature,
        address inner,
        bytes memory innerData
    ) internal view returns (bytes memory) {
        return encodeSingleSwap(
            address(wrapper),
            abi.encodePacked(
                s.boundTo,
                s.amount,
                s.minAmountIn,
                s.nonce,
                // forge-lint: disable-next-line(unsafe-typecast)
                uint48(s.deadline),
                signature,
                inner,
                innerData
            )
        );
    }

    /// @dev A WETH -> DAI UniswapV2 swap carrying a DAI subsidy bound to
    ///      `boundTo`, signed by the subsidy signer.
    function _signedSwapData(address boundTo)
        internal
        view
        returns (bytes memory)
    {
        Subsidy memory s = _subsidy(
            boundTo, DAI_ADDR, DAI_SUBSIDY, address(usv2Executor), usv2Data
        );
        return _encode(s, _sign(s), address(usv2Executor), usv2Data);
    }

    function _fund(address account) internal {
        deal(WETH_ADDR, account, AMOUNT_IN);
        vm.prank(account);
        IERC20(WETH_ADDR).approve(tychoRouterAddr, AMOUNT_IN);
    }

    /// @dev Sends the swap as `caller`, which is also tx.origin.
    function _swapAs(address caller, bytes memory swapData)
        internal
        returns (uint256)
    {
        vm.prank(caller, caller);
        return tychoRouter.singleSwap(
            AMOUNT_IN,
            WETH_ADDR,
            DAI_ADDR,
            1,
            1,
            caller,
            noClientFee(),
            swapData
        );
    }

    /// @dev Output of a WETH -> DAI `swapData` sent by ALICE, with the state
    ///      reverted afterwards.
    function _baseline(bytes memory swapData) internal returns (uint256) {
        return
            _baselineSingleSwap(WETH_ADDR, DAI_ADDR, AMOUNT_IN, ALICE, swapData);
    }

    function _plainV2() internal view returns (bytes memory) {
        return encodeSingleSwap(address(usv2Executor), usv2Data);
    }

    function _expectSkip(ISubsidyPayer.SkipReason reason) internal {
        vm.expectEmit(address(payer));
        emit ISubsidyPayer.SubsidySkipped(NONCE, reason);
    }

    function testTamperedInnerData() public {
        bytes memory usv3Data =
            encodeUniswapV3Swap(WETH_ADDR, DAI_ADDR, DAI_WETH_USV3, false);
        Subsidy memory s = _subsidy(
            ALICE, DAI_ADDR, DAI_SUBSIDY, address(usv2Executor), usv2Data
        );
        bytes memory swapData =
            _encode(s, _sign(s), address(usv3Executor), usv3Data);
        uint256 baseline =
            _baseline(encodeSingleSwap(address(usv3Executor), usv3Data));

        _fund(ALICE);
        _expectSkip(ISubsidyPayer.SkipReason.InvalidSignature);
        uint256 amountOut = _swapAs(ALICE, swapData);

        assertEq(amountOut, baseline);
        assertFalse(payer.isNonceUsed(NONCE));
    }

    function testExpiredSubsidy() public {
        Subsidy memory s = _subsidy(
            ALICE, DAI_ADDR, DAI_SUBSIDY, address(usv2Executor), usv2Data
        );
        s.deadline = block.timestamp + 10;
        bytes memory swapData =
            _encode(s, _sign(s), address(usv2Executor), usv2Data);
        vm.warp(block.timestamp + 11);
        uint256 baseline = _baseline(_plainV2());

        _fund(ALICE);
        _expectSkip(ISubsidyPayer.SkipReason.Expired);
        uint256 amountOut = _swapAs(ALICE, swapData);

        assertEq(amountOut, baseline);
        assertFalse(payer.isNonceUsed(NONCE));
    }

    function testUnsignedPlaceholder() public {
        Subsidy memory s = _subsidy(
            ALICE, DAI_ADDR, DAI_SUBSIDY, address(usv2Executor), usv2Data
        );
        bytes memory swapData =
            _encode(s, new bytes(65), address(usv2Executor), usv2Data);
        uint256 baseline = _baseline(_plainV2());

        _fund(ALICE);
        _expectSkip(ISubsidyPayer.SkipReason.InvalidSignature);
        uint256 amountOut = _swapAs(ALICE, swapData);

        assertEq(amountOut, baseline);
        assertFalse(payer.isNonceUsed(NONCE));
    }

    function _swapThroughIntegrator(address sender, bytes memory swapData)
        internal
        returns (uint256)
    {
        deal(WETH_ADDR, address(integrator), AMOUNT_IN);
        vm.prank(sender, sender);
        return integrator.swap(
            tychoRouter, WETH_ADDR, AMOUNT_IN, DAI_ADDR, ALICE, swapData
        );
    }

    function testContractCallerBoundToContract() public {
        uint256 baseline = _baseline(_plainV2());

        uint256 amountOut =
            _swapThroughIntegrator(ALICE, _signedSwapData(address(integrator)));

        assertEq(amountOut, baseline + DAI_SUBSIDY);
        assertTrue(payer.isNonceUsed(NONCE));
    }

    function testContractCallerBoundToSendingEoa() public {
        uint256 baseline = _baseline(_plainV2());

        uint256 amountOut =
            _swapThroughIntegrator(ALICE, _signedSwapData(ALICE));

        assertEq(amountOut, baseline + DAI_SUBSIDY);
        assertTrue(payer.isNonceUsed(NONCE));
    }

    function testRelayedCaller() public {
        uint256 baseline = _baseline(_plainV2());
        bytes memory swapData = _signedSwapData(ALICE);

        _expectSkip(ISubsidyPayer.SkipReason.CallerMismatch);
        uint256 amountOut = _swapThroughIntegrator(relayer, swapData);

        assertEq(amountOut, baseline);
        assertFalse(payer.isNonceUsed(NONCE));
    }

    function testFrontRunCopyThenUserSwap() public {
        bytes memory swapData = _signedSwapData(ALICE);

        _fund(BOB);
        _expectSkip(ISubsidyPayer.SkipReason.CallerMismatch);
        _swapAs(BOB, swapData);
        assertFalse(payer.isNonceUsed(NONCE));

        uint256 baseline = _baseline(_plainV2());
        _fund(ALICE);
        uint256 amountOut = _swapAs(ALICE, swapData);

        assertEq(amountOut, baseline + DAI_SUBSIDY);
        assertTrue(payer.isNonceUsed(NONCE));
    }

    function testReplay() public {
        bytes memory swapData = _signedSwapData(ALICE);
        _fund(ALICE);
        _swapAs(ALICE, swapData);

        uint256 baseline = _baseline(_plainV2());
        _fund(ALICE);
        _expectSkip(ISubsidyPayer.SkipReason.NonceUsed);
        uint256 amountOut = _swapAs(ALICE, swapData);

        assertEq(amountOut, baseline);
    }

    function testPausedPayer() public {
        vm.prank(subsidyPauser);
        payer.pause();
        uint256 baseline = _baseline(_plainV2());
        bytes memory swapData = _signedSwapData(ALICE);

        _fund(ALICE);
        _expectSkip(ISubsidyPayer.SkipReason.Paused);
        uint256 amountOut = _swapAs(ALICE, swapData);

        assertEq(amountOut, baseline);
        assertFalse(payer.isNonceUsed(NONCE));
    }

    function testPayerBalanceBelowSubsidy() public {
        uint256 budget = IERC20(DAI_ADDR).balanceOf(address(payer));
        vm.prank(budgetManager);
        payer.withdraw(DAI_ADDR, budgetManager, budget - DAI_SUBSIDY + 1);
        uint256 baseline = _baseline(_plainV2());
        bytes memory swapData = _signedSwapData(ALICE);

        _fund(ALICE);
        _expectSkip(ISubsidyPayer.SkipReason.InsufficientBalance);
        vm.prank(ALICE, ALICE);
        uint256 amountOut = tychoRouter.singleSwap(
            AMOUNT_IN,
            WETH_ADDR,
            DAI_ADDR,
            baseline + DAI_SUBSIDY,
            baseline,
            ALICE,
            noClientFee(),
            swapData
        );

        assertEq(amountOut, baseline);
        assertFalse(payer.isNonceUsed(NONCE));
    }

    /// @dev A USDC -> ETH UniswapV4 swap carrying an ETH subsidy bound to
    ///      ALICE, signed by the subsidy signer.
    function _signedUsdcToEthSwapData() internal view returns (bytes memory) {
        bytes memory usv4Data = _usv4UsdcToEth();
        Subsidy memory s = _subsidy(
            ALICE, ETH_ADDR, ETH_SUBSIDY, address(usv4Executor), usv4Data
        );
        return _encode(s, _sign(s), address(usv4Executor), usv4Data);
    }

    function _usdcToEthBaseline(uint256 amountIn) internal returns (uint256) {
        return _baselineSingleSwap(
            USDC_ADDR,
            ETH_ADDR,
            amountIn,
            BOB,
            encodeSingleSwap(address(usv4Executor), _usv4UsdcToEth())
        );
    }

    function testEthSubsidyIntoRouterWithClientFee() public {
        uint256 amountIn = 3000_000000;
        uint32 clientFeeBps = 1_000_000; // 1%
        bytes memory swapData = _signedUsdcToEthSwapData();
        uint256 expectedAmountOut = _usdcToEthBaseline(amountIn) + ETH_SUBSIDY;
        ClientFeeParams memory clientFee = makeClientFeeParams(
            clientFeeBps,
            0,
            amountIn,
            USDC_ADDR,
            ETH_ADDR,
            expectedAmountOut,
            1,
            BOB,
            swapData,
            tychoRouterAddr,
            CLIENT_FEE_RECEIVER_PK
        );

        _fundAlice(USDC_ADDR, amountIn);
        vm.expectEmit(address(payer));
        emit ISubsidyPayer.SubsidyPaid(
            NONCE, ALICE, tychoRouterAddr, ETH_ADDR, ETH_SUBSIDY
        );
        vm.prank(ALICE, ALICE);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            USDC_ADDR,
            ETH_ADDR,
            expectedAmountOut,
            1,
            BOB,
            clientFee,
            swapData
        );

        uint256 fee = expectedAmountOut * clientFeeBps / 100_000_000;
        assertEq(amountOut, expectedAmountOut - fee);
        assertEq(BOB.balance, expectedAmountOut - fee);
    }

    function testEthSubsidyToEoaReceiver() public {
        uint256 amountIn = 3000_000000;
        bytes memory swapData = _signedUsdcToEthSwapData();
        uint256 baseline = _usdcToEthBaseline(amountIn);

        _fundAlice(USDC_ADDR, amountIn);
        vm.expectEmit(address(payer));
        emit ISubsidyPayer.SubsidyPaid(NONCE, ALICE, BOB, ETH_ADDR, ETH_SUBSIDY);
        vm.prank(ALICE, ALICE);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn, USDC_ADDR, ETH_ADDR, 1, 1, BOB, noClientFee(), swapData
        );

        assertEq(amountOut, baseline + ETH_SUBSIDY);
        assertEq(BOB.balance, baseline + ETH_SUBSIDY);
    }

    function _usdcToEthCall(bytes memory swapData, address receiver)
        internal
        returns (bool success, bytes memory result)
    {
        uint256 amountIn = 3000_000000;
        _fundAlice(USDC_ADDR, amountIn);
        vm.prank(ALICE, ALICE);
        (success, result) = tychoRouterAddr.call(
            abi.encodeCall(
                tychoRouter.singleSwap,
                (
                    amountIn,
                    USDC_ADDR,
                    ETH_ADDR,
                    1,
                    1,
                    receiver,
                    noClientFee(),
                    swapData
                )
            )
        );
    }

    function testEthRejectingReceiver() public {
        address receiver = address(new EthRejectingReceiver());
        bytes memory usv4Data = _usv4UsdcToEth();
        Subsidy memory s = _subsidy(
            ALICE, ETH_ADDR, ETH_SUBSIDY, address(usv4Executor), usv4Data
        );
        bytes memory wrapped =
            _encode(s, _sign(s), address(usv4Executor), usv4Data);

        uint256 snapshot = vm.snapshotState();
        (bool plainSuccess, bytes memory plainResult) = _usdcToEthCall(
            encodeSingleSwap(address(usv4Executor), usv4Data), receiver
        );
        vm.revertToState(snapshot);
        (bool wrappedSuccess, bytes memory wrappedResult) =
            _usdcToEthCall(wrapped, receiver);

        assertFalse(plainSuccess);
        assertFalse(wrappedSuccess);
        assertEq(wrappedResult, plainResult);
        assertFalse(payer.isNonceUsed(NONCE));
    }
}
