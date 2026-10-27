// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {PropAMMExecutor} from "@src/executors/PropAMMExecutor.sol";
import {TychoRouterTestSetup} from "../TychoRouterTestSetup.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {TychoRouter__NegativeSlippage} from "@src/TychoRouterV3.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";
import {
    ElfomofiExecutor,
    IElfomofiRouter,
    ElfomofiExecutor__InvalidData,
    ElfomofiExecutor__InvalidRouter,
    ElfomofiExecutor__QuoteExpired,
    ElfomofiExecutor__InvalidAmount
} from "@src/executors/ElfomofiExecutor.sol";

contract ElfomofiTestToken is ERC20 {
    mapping(address => mapping(address => uint256)) public approvalCalls;
    address public lastTransferFromReceiver;

    constructor() ERC20("Test", "TEST") {}

    function mint(address receiver, uint256 amount) external {
        _mint(receiver, amount);
    }

    function approve(address spender, uint256 amount)
        public
        override
        returns (bool)
    {
        approvalCalls[msg.sender][spender]++;
        return super.approve(spender, amount);
    }

    function transferFrom(address from, address to, uint256 amount)
        public
        override
        returns (bool)
    {
        lastTransferFromReceiver = to;
        return super.transferFrom(from, to, amount);
    }
}

contract ElfomofiTestVault {}

contract MockElfomofiRouter is IElfomofiRouter {
    uint256 public lastPartner;
    uint256 public lastAmount;
    uint256 public lastMinimum;
    uint256 public outputCap = type(uint256).max;

    function setOutputCap(uint256 cap) external {
        outputCap = cap;
    }
    address public lastCaller;
    address public lastReceiver;
    address public immutable vault = address(new ElfomofiTestVault());

    function swapWithContractBalance(
        address from,
        address to,
        uint256 minimum,
        address receiver,
        uint256 partner
    ) external {
        uint256 input = IERC20(from).balanceOf(address(this));
        require(input > 0, "input balance required");
        uint256 amountOut = input * 2 < outputCap ? input * 2 : outputCap;
        require(amountOut >= minimum, "minimum output");
        lastMinimum = minimum;
        lastPartner = partner;
        lastAmount = input;
        lastCaller = msg.sender;
        lastReceiver = receiver;
        IERC20(from).transfer(vault, input);
        IERC20(to).transfer(receiver, amountOut);
    }
}

contract MockElfomofiPushDex {
    uint256 public outputDivisor = 1;

    function setOutputDivisor(uint256 divisor) external {
        outputDivisor = divisor;
    }
    address public immutable vault = address(new ElfomofiTestVault());
    address public lastReceiver;
    uint256 public lastAmount;

    function swap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut,
        address receiver,
        uint256 deadline
    ) external returns (uint256) {
        require(block.timestamp <= deadline, "expired");
        uint256 amountOut = amountIn / outputDivisor;
        require(amountOut >= minAmountOut, "minimum output");
        require(
            IERC20(tokenIn).balanceOf(address(this)) >= amountIn,
            "input not pushed"
        );
        lastReceiver = receiver;
        lastAmount = amountIn;
        IERC20(tokenIn).transfer(vault, amountIn);
        IERC20(tokenOut).transfer(receiver, amountOut);
        return amountOut;
    }
}

contract ElfomofiExecutorTest is TychoRouterTestSetup {
    ElfomofiExecutor internal executor;
    MockElfomofiRouter internal venue;
    ElfomofiTestToken internal input;
    ElfomofiTestToken internal output;

    function testConfiguredRuntimeUsesDeploymentArguments() public {
        ElfomofiExecutor configured =
            new ElfomofiExecutor(0xf0f0F0F0FB0d738452EfD03A28e8be14C76d5f73);
        assertEq(
            configured.elfomofiRouter(),
            0xf0f0F0F0FB0d738452EfD03A28e8be14C76d5f73
        );
        assertEq(configured.partnerId(), 299);
        assertEq(
            vm.parseJsonBytes(
                vm.readFile(
                    "../../../protocols/testing/fixtures/Elfomofi.runtime.json"
                ),
                ".runtimeBytecode"
            ),
            address(configured).code
        );
    }

    // Boundary tests run without RPC; only the protocol call is mocked. The actual TychoRouter
    // and TransferManager exercise push funding, delegatecall, receiver accounting and slippage.
    function setUp() public override {
        vm.etch(PERMIT2_ADDRESS, hex"00");
        deployRouter();
        deployFeeCalculator();
        venue = new MockElfomofiRouter();
        executor = new ElfomofiExecutor(address(venue));
        input = new ElfomofiTestToken();
        output = new ElfomofiTestToken();
        address[] memory executors = new address[](1);
        executors[0] = address(executor);
        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.prank(FEE_SETTER);
        tychoRouter.setFeeCalculator(address(feeCalculator));
        vm.warp(
            block.timestamp + tychoRouter.DELAY_EXECUTOR_ACTIVATION()
                + tychoRouter.DELAY_FEE_CALCULATOR_ACTIVATION()
        );
        vm.prank(FEE_SETTER);
        tychoRouter.activateFeeCalculator();
        input.mint(BOB, 100);
        output.mint(address(venue), 1_000);
        vm.prank(BOB);
        input.approve(tychoRouterAddr, 100);
    }

    function testTransferDataUsesFixedRouter() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(
            abi.encodePacked(
                address(input),
                address(output),
                uint64(block.timestamp + 10),
                uint256(100),
                uint256(199)
            )
        );
        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, address(venue));
        assertEq(tokenIn, address(input));
        assertEq(tokenOut, address(output));
        assertFalse(outputToRouter);
        assertEq(executor.fundsExpectedAddress(hex""), address(venue));
    }

    function testPushFundingAndPartnerWithoutConsumingRouterSurplus() public {
        input.mint(tychoRouterAddr, 17);
        assertEq(input.allowance(tychoRouterAddr, address(venue)), 0);
        vm.prank(BOB);
        uint256 amount = tychoRouter.singleSwap(
            100,
            address(input),
            address(output),
            200,
            200,
            BOB,
            noClientFee(),
            encodeSingleSwap(
                address(executor),
                abi.encodePacked(
                    address(input),
                    address(output),
                    uint64(block.timestamp + 10),
                    uint256(100),
                    uint256(199)
                )
            )
        );
        assertEq(amount, 200);
        assertEq(output.balanceOf(BOB), 200);
        assertEq(input.balanceOf(BOB), 0);
        assertEq(input.balanceOf(tychoRouterAddr), 17);
        assertEq(venue.lastAmount(), 100);
        assertEq(venue.lastPartner(), 299);
        assertEq(venue.lastCaller(), tychoRouterAddr);
        assertEq(input.lastTransferFromReceiver(), address(venue));
        assertEq(input.balanceOf(address(venue)), 0);
        assertEq(input.balanceOf(venue.vault()), 100);
        assertEq(input.allowance(tychoRouterAddr, address(venue)), 0);
        assertEq(input.approvalCalls(tychoRouterAddr, address(venue)), 0);
    }

    function testRouterMinimumOutputRevertsAtomically() public {
        bytes memory data = encodeSingleSwap(
            address(executor),
            abi.encodePacked(
                address(input),
                address(output),
                uint64(block.timestamp + 10),
                uint256(100),
                uint256(199)
            )
        );
        vm.prank(BOB);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__NegativeSlippage.selector,
                uint256(200),
                uint256(201)
            )
        );
        tychoRouter.singleSwap(
            100,
            address(input),
            address(output),
            201,
            201,
            BOB,
            noClientFee(),
            data
        );
        assertEq(input.balanceOf(BOB), 100);
        assertEq(output.balanceOf(BOB), 0);
        assertEq(input.balanceOf(address(venue)), 0);
        assertEq(input.balanceOf(venue.vault()), 0);
        assertEq(venue.lastAmount(), 0);
    }

    function testVenueDustIsIncludedInTheBalanceSwap() public {
        input.mint(address(venue), 7);
        input.mint(tychoRouterAddr, 17);
        vm.prank(BOB);
        uint256 amount = tychoRouter.singleSwap(
            100,
            address(input),
            address(output),
            200,
            200,
            BOB,
            noClientFee(),
            encodeSingleSwap(
                address(executor), _elfomofiData(uint64(block.timestamp + 10))
            )
        );
        assertEq(amount, 214);
        assertEq(output.balanceOf(BOB), 214);
        assertEq(venue.lastAmount(), 107);
        assertEq(input.balanceOf(address(venue)), 0);
        assertEq(input.balanceOf(venue.vault()), 107);
        assertEq(input.balanceOf(tychoRouterAddr), 17);
        assertEq(input.approvalCalls(tychoRouterAddr, address(venue)), 0);
    }

    function testSequentialDexElfomofiDexUsesDirectReceivers() public {
        PropAMMExecutor dexExecutor = _registerPushDexExecutor();
        MockElfomofiPushDex beforeDex = new MockElfomofiPushDex();
        MockElfomofiPushDex afterDex = new MockElfomofiPushDex();
        ElfomofiTestToken firstToken = new ElfomofiTestToken();
        ElfomofiTestToken lastToken = new ElfomofiTestToken();
        beforeDex.setOutputDivisor(2);
        firstToken.mint(BOB, 100);
        input.mint(address(beforeDex), 50);
        lastToken.mint(address(afterDex), 100);
        input.mint(tychoRouterAddr, 17);
        output.mint(tychoRouterAddr, 19);
        vm.prank(BOB);
        firstToken.approve(tychoRouterAddr, 100);

        bytes[] memory swaps = new bytes[](3);
        swaps[0] = encodeSequentialSwap(
            address(dexExecutor),
            abi.encodePacked(
                address(beforeDex), address(firstToken), address(input)
            )
        );
        swaps[1] = encodeSequentialSwap(
            address(executor), _elfomofiData(uint64(block.timestamp + 10))
        );
        swaps[2] = encodeSequentialSwap(
            address(dexExecutor),
            abi.encodePacked(
                address(afterDex), address(output), address(lastToken)
            )
        );
        vm.prank(BOB);
        uint256 amount = tychoRouter.sequentialSwap(
            100,
            address(firstToken),
            address(lastToken),
            100,
            100,
            BOB,
            noClientFee(),
            pleEncode(swaps)
        );
        assertEq(amount, 100);
        assertEq(firstToken.lastTransferFromReceiver(), address(beforeDex));
        assertEq(beforeDex.lastReceiver(), address(venue));
        assertEq(venue.lastReceiver(), address(afterDex));
        assertEq(afterDex.lastReceiver(), BOB);
        assertEq(beforeDex.lastAmount(), 100);
        assertEq(venue.lastAmount(), 50);
        assertEq(venue.lastMinimum(), 100);
        assertEq(afterDex.lastAmount(), 100);
        assertEq(lastToken.balanceOf(BOB), 100);
        assertEq(input.balanceOf(tychoRouterAddr), 17);
        assertEq(output.balanceOf(tychoRouterAddr), 19);
        assertEq(input.balanceOf(venue.vault()), 50);
        assertEq(output.balanceOf(afterDex.vault()), 100);
        assertEq(input.approvalCalls(tychoRouterAddr, address(venue)), 0);
        assertEq(output.approvalCalls(tychoRouterAddr, address(afterDex)), 0);
    }

    function testVaultFundingPushesOnlyTheRequestedInput() public {
        vm.prank(BOB);
        tychoRouter.deposit(address(input), 100);
        input.mint(tychoRouterAddr, 17);
        vm.prank(BOB);
        uint256 amount = tychoRouter.singleSwapUsingVault(
            100,
            address(input),
            address(output),
            200,
            200,
            BOB,
            noClientFee(),
            encodeSingleSwap(
                address(executor), _elfomofiData(uint64(block.timestamp + 10))
            )
        );
        assertEq(amount, 200);
        assertEq(output.balanceOf(BOB), 200);
        assertEq(input.balanceOf(tychoRouterAddr), 17);
        assertEq(input.balanceOf(venue.vault()), 100);
        assertEq(
            tychoRouter.balanceOf(BOB, uint256(uint160(address(input)))), 0
        );
        assertEq(input.approvalCalls(tychoRouterAddr, address(venue)), 0);
    }

    function testSplitFundingPushesEachLegWithoutVenueApproval() public {
        PropAMMExecutor dexExecutor = _registerPushDexExecutor();
        MockElfomofiPushDex dex = new MockElfomofiPushDex();
        output.mint(address(dex), 50);
        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSplitSwap(
            0,
            1,
            0x800000,
            address(executor),
            _elfomofiData(uint64(block.timestamp + 10))
        );
        swaps[1] = encodeSplitSwap(
            0,
            1,
            0,
            address(dexExecutor),
            abi.encodePacked(address(dex), address(input), address(output))
        );
        vm.prank(BOB);
        uint256 amount = tychoRouter.splitSwap(
            100,
            address(input),
            address(output),
            150,
            150,
            2,
            BOB,
            noClientFee(),
            pleEncode(swaps)
        );
        assertEq(amount, 150);
        assertEq(output.balanceOf(BOB), 150);
        assertEq(input.balanceOf(BOB), 0);
        assertEq(venue.lastAmount(), 50);
        assertEq(dex.lastAmount(), 50);
        assertEq(input.balanceOf(venue.vault()), 50);
        assertEq(input.balanceOf(dex.vault()), 50);
        assertEq(input.approvalCalls(tychoRouterAddr, address(venue)), 0);
        assertEq(input.approvalCalls(tychoRouterAddr, address(dex)), 0);
    }

    function _registerPushDexExecutor()
        internal
        returns (PropAMMExecutor dexExecutor)
    {
        dexExecutor = new PropAMMExecutor();
        address[] memory executors = new address[](1);
        executors[0] = address(dexExecutor);
        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.warp(block.timestamp + tychoRouter.DELAY_EXECUTOR_ACTIVATION());
    }

    function _elfomofiData(uint64 validUntil)
        internal
        view
        returns (bytes memory)
    {
        return abi.encodePacked(
            address(input),
            address(output),
            validUntil,
            uint256(100),
            uint256(199)
        );
    }

    function testRejectsInvalidLengthNativeAndIdenticalTokens() public {
        vm.expectRevert(ElfomofiExecutor__InvalidData.selector);
        executor.getTransferData(hex"1234");
        vm.expectRevert(ElfomofiExecutor__InvalidData.selector);
        executor.getTransferData(
            abi.encodePacked(
                ETH_ADDRESS,
                address(output),
                uint64(block.timestamp + 10),
                uint256(100),
                uint256(199)
            )
        );
        vm.expectRevert(ElfomofiExecutor__InvalidData.selector);
        executor.getTransferData(
            abi.encodePacked(
                address(input),
                address(input),
                uint64(block.timestamp + 10),
                uint256(100),
                uint256(199)
            )
        );
    }

    function testRejectsInvalidRouter() public {
        vm.expectRevert(ElfomofiExecutor__InvalidRouter.selector);
        new ElfomofiExecutor(address(0));
    }

    function testExpiredQuoteRevertsAtomically() public {
        bytes memory data = encodeSingleSwap(
            address(executor),
            abi.encodePacked(
                address(input),
                address(output),
                uint64(block.timestamp - 1),
                uint256(100),
                uint256(199)
            )
        );
        vm.prank(BOB);
        vm.expectRevert(ElfomofiExecutor__QuoteExpired.selector);
        tychoRouter.singleSwap(
            100,
            address(input),
            address(output),
            200,
            200,
            BOB,
            noClientFee(),
            data
        );
        assertEq(input.balanceOf(BOB), 100);
        assertEq(output.balanceOf(BOB), 0);
        assertEq(input.balanceOf(address(venue)), 0);
        assertEq(input.balanceOf(venue.vault()), 0);
        assertEq(venue.lastAmount(), 0);
    }

    function testQuoteRejectsExactDeadline() public {
        bytes memory data = encodeSingleSwap(
            address(executor),
            abi.encodePacked(
                address(input),
                address(output),
                uint64(block.timestamp),
                uint256(100),
                uint256(199)
            )
        );
        vm.prank(BOB);
        vm.expectRevert(ElfomofiExecutor__QuoteExpired.selector);
        tychoRouter.singleSwap(
            100,
            address(input),
            address(output),
            200,
            200,
            BOB,
            noClientFee(),
            data
        );
        assertEq(input.balanceOf(BOB), 100);
        assertEq(output.balanceOf(BOB), 0);
    }

    function testQuoteAcceptsImmediatelyBeforeDeadline() public {
        bytes memory data = encodeSingleSwap(
            address(executor),
            abi.encodePacked(
                address(input),
                address(output),
                uint64(block.timestamp + 1),
                uint256(100),
                uint256(199)
            )
        );
        vm.prank(BOB);
        uint256 amount = tychoRouter.singleSwap(
            100,
            address(input),
            address(output),
            200,
            200,
            BOB,
            noClientFee(),
            data
        );
        assertEq(amount, 200);
    }

    function testRejectsZeroExactInput() public {
        vm.expectRevert(ElfomofiExecutor__InvalidAmount.selector);
        executor.swap(
            0,
            abi.encodePacked(
                address(input),
                address(output),
                uint64(block.timestamp + 10),
                uint256(100),
                uint256(199)
            ),
            BOB
        );
        assertEq(input.balanceOf(BOB), 100);
        assertEq(output.balanceOf(BOB), 0);
        assertEq(venue.lastAmount(), 0);
    }

    function testMinimumRateScalesWithActualInput() public {
        input.mint(address(venue), 50);
        executor.swap(50, _elfomofiData(uint64(block.timestamp + 10)), BOB);
        assertEq(venue.lastMinimum(), 100);
        assertEq(output.balanceOf(BOB), 100);
    }

    function testCappedOutputBelowLegMinimumRevertsAtomically() public {
        venue.setOutputCap(150);
        bytes memory data = encodeSingleSwap(
            address(executor), _elfomofiData(uint64(block.timestamp + 10))
        );
        vm.prank(BOB);
        vm.expectRevert(
            abi.encodeWithSignature("Error(string)", "minimum output")
        );
        tychoRouter.singleSwap(
            100,
            address(input),
            address(output),
            150,
            150,
            BOB,
            noClientFee(),
            data
        );
        assertEq(input.balanceOf(BOB), 100);
        assertEq(input.balanceOf(venue.vault()), 0);
    }

    function testMinimumRateUsesFullPrecisionUnsignedMath() public {
        input.mint(address(venue), 100);
        executor.swap(
            type(uint256).max,
            abi.encodePacked(
                address(input),
                address(output),
                uint64(block.timestamp + 10),
                type(uint256).max,
                uint256(199)
            ),
            BOB
        );
        assertEq(venue.lastMinimum(), 199);
        assertEq(output.balanceOf(BOB), 200);
    }

    function testRejectsZeroReferenceAmounts() public {
        for (uint256 i = 0; i < 2; i++) {
            vm.expectRevert(ElfomofiExecutor__InvalidData.selector);
            executor.getTransferData(
                abi.encodePacked(
                    address(input),
                    address(output),
                    uint64(block.timestamp + 10),
                    i == 0 ? uint256(0) : uint256(100),
                    i == 0 ? uint256(199) : uint256(0)
                )
            );
        }
    }
}

interface IElfomofiHelperTest {
    struct Level {
        uint256 amountIn;
        uint256 amountOut;
    }

    struct Book {
        address base;
        address quote;
        Level[] askCumulativeLevels;
        Level[] bidCumulativeLevels;
        uint256 balanceBase;
        uint256 balanceQuote;
    }
    function getOrderbook(address base, address quote)
        external
        view
        returns (Book memory, uint256, uint256);
}

interface IElfomofiPricingTest {
    function getAmountOut(
        address from,
        address to,
        uint256 amount,
        address executor,
        uint256 partner
    )
        external
        view
        returns (uint256 amountOut, uint256 quoteId, bytes memory data);
}

abstract contract ElfomofiForkTestBase is TychoRouterTestSetup {
    event ElfomoTrade(
        uint256 indexed quoteId,
        uint256 indexed partnerId,
        address executor,
        address receiver,
        address fromToken,
        address toToken,
        uint256 fromAmount,
        uint256 toAmount
    );

    address internal constant VENUE =
        0xf0f0F0F0FB0d738452EfD03A28e8be14C76d5f73;
    address internal constant PRICING =
        0xFFFFffBB2D432B8ACb4c57d556C0C721A431d038;
    ElfomofiExecutor internal executor;

    function _helper() internal pure virtual returns (address);

    function _baseToken() internal pure virtual returns (address);

    function _quoteToken() internal pure virtual returns (address);

    function setUp() public override {
        super.setUp();
        _configureForkSystemContracts();
        // The fork supplies the actual venue, helper, pricing, vault and token state.
        assertGt(VENUE.code.length, 0, "ElFomoFi settlement must be deployed");
        assertGt(PRICING.code.length, 0, "ElFomoFi pricing must be deployed");
        assertGt(_helper().code.length, 0, "ElFomoFi helper must be deployed");
        assertGt(_baseToken().code.length, 0, "Base token must be deployed");
        assertGt(_quoteToken().code.length, 0, "Quote token must be deployed");
        executor = new ElfomofiExecutor(VENUE);
        vm.warp(forkTimestamp - tychoRouter.DELAY_EXECUTOR_ACTIVATION());
        address[] memory executors = new address[](1);
        executors[0] = address(executor);
        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.warp(forkTimestamp);
    }

    function _configureForkSystemContracts() internal virtual {}

    function testHelperBidExecutesThroughTychoRouter() public {
        _testDirection(false);
    }

    function testHelperAskExecutesThroughTychoRouter() public {
        _testDirection(true);
    }

    function _testDirection(bool reverse) internal {
        (
            IElfomofiHelperTest.Book memory book,
            uint256 number,
            uint256 timestamp
        ) = IElfomofiHelperTest(_helper())
            .getOrderbook(_baseToken(), _quoteToken());
        assertEq(number, block.number);
        assertEq(timestamp, block.timestamp);
        assertEq(book.base, _baseToken());
        assertEq(book.quote, _quoteToken());
        assertGt(book.askCumulativeLevels.length, 0, "Ask levels required");
        assertGt(book.bidCumulativeLevels.length, 0, "Bid levels required");
        IElfomofiHelperTest.Level memory level =
            reverse ? book.askCumulativeLevels[0] : book.bidCumulativeLevels[0];
        address sell = reverse ? _quoteToken() : _baseToken();
        address buy = reverse ? _baseToken() : _quoteToken();
        assertGt(level.amountIn, 0);
        assertGt(level.amountOut, 0);
        // Quote with the Tycho Router and partner 299 to match settlement.
        vm.prank(VENUE);
        (uint256 expected,,) = IElfomofiPricingTest(PRICING)
            .getAmountOut(sell, buy, level.amountIn, tychoRouterAddr, 299);
        assertGt(expected, 0);
        // This pinned block has no venue input dust, so the pushed balance is exactly quoted.
        assertEq(IERC20(sell).balanceOf(VENUE), 0);
        assertEq(IERC20(sell).allowance(tychoRouterAddr, VENUE), 0);
        deal(sell, BOB, level.amountIn);
        uint256 beforeOutput = IERC20(buy).balanceOf(BOB);
        uint256 referenceMinimum = (level.amountOut * 9950 + 9999) / 10000;
        bytes memory swapData =
            _swapData(sell, buy, reverse, level.amountIn, referenceMinimum);
        assertEq(
            swapData,
            abi.encodePacked(
                sell,
                buy,
                uint64(block.timestamp + 10),
                level.amountIn,
                referenceMinimum
            )
        );
        bytes memory data = encodeSingleSwap(address(executor), swapData);
        vm.startPrank(BOB);
        IERC20(sell).approve(tychoRouterAddr, level.amountIn);
        // Pricing generates quoteId; check the settlement's partner and complete fill data.
        vm.expectEmit(false, true, false, true, VENUE);
        emit ElfomoTrade(
            0, 299, tychoRouterAddr, BOB, sell, buy, level.amountIn, expected
        );
        uint256 amount = tychoRouter.singleSwap(
            level.amountIn,
            sell,
            buy,
            expected,
            expected,
            BOB,
            noClientFee(),
            data
        );
        vm.stopPrank();
        assertEq(amount, expected);
        assertEq(IERC20(buy).balanceOf(BOB) - beforeOutput, expected);
        assertEq(IERC20(sell).balanceOf(BOB), 0);
        assertEq(IERC20(sell).balanceOf(VENUE), 0);
        assertEq(IERC20(sell).allowance(tychoRouterAddr, VENUE), 0);
    }

    function _swapData(
        address sell,
        address buy,
        bool,
        uint256 referenceIn,
        uint256 referenceMinOut
    ) internal view virtual returns (bytes memory) {
        return abi.encodePacked(
            sell,
            buy,
            uint64(block.timestamp + 10),
            referenceIn,
            referenceMinOut
        );
    }
}

contract ElfomofiForkTest is ElfomofiForkTestBase {
    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 52_898_481;
    }

    function _helper() internal pure override returns (address) {
        return 0xc1b13606FC7227f2554067aFb3fb12De75C02d81;
    }

    function _baseToken() internal pure override returns (address) {
        return 0x4200000000000000000000000000000000000006;
    }

    function _quoteToken() internal pure override returns (address) {
        return BASE_USDC;
    }

    function _swapData(address, address, bool reverse, uint256, uint256)
        internal
        view
        override
        returns (bytes memory)
    {
        // Base also validates the Rust encoder's shared calldata fixture.
        return loadCallDataFromFile(
            reverse
                ? "test_encode_elfomofi_usdc_weth"
                : "test_encode_elfomofi_weth_usdc"
        );
    }
}

contract ElfomofiBscForkTest is ElfomofiForkTestBase {
    function getChain() public pure override returns (string memory) {
        return "bsc";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 128_939_405;
    }

    function _helper() internal pure override returns (address) {
        return 0x78015E3544d989f0712a0b9986cB05838c3fE06D;
    }

    function _baseToken() internal pure override returns (address) {
        return address(0x7130d2A12B9BCbFAe4f2634d864A1Ee1Ce3Ead9c);
    }

    function _quoteToken() internal pure override returns (address) {
        return 0x55d398326f99059fF775485246999027B3197955;
    }
}

contract ElfomofiRobinhoodForkTest is ElfomofiForkTestBase {
    function _configureForkSystemContracts() internal override {
        // Foundry lacks Arbitrum's native ArbSys precompile. Only its block-number read is
        // supplied here; ElFomoFi's helper, pricing, vault and tokens remain real fork state.
        vm.mockCall(
            address(0x64),
            abi.encodeWithSignature("arbBlockNumber()"),
            abi.encode(block.number)
        );
    }

    function getChain() public pure override returns (string memory) {
        return "robinhood";
    }

    function getForkBlock() public pure override returns (uint256) {
        return 94_288_199;
    }

    function _helper() internal pure override returns (address) {
        return 0x20D24Ee45c9b338Ef7ACf2eEde75183B9E2C9E3c;
    }

    function _baseToken() internal pure override returns (address) {
        return 0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73;
    }

    function _quoteToken() internal pure override returns (address) {
        return 0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168;
    }
}
