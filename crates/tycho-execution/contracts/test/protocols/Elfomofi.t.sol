// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {console} from "forge-std/console.sol";
import {MockERC20} from "forge-std/mocks/MockERC20.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {TychoRouterTestSetup} from "../TychoRouterTestSetup.sol";
import {TestUtils} from "../TestUtils.sol";
import {Constants} from "../Constants.sol";
import {
    ElfomofiExecutor,
    IElfomofiRouter
} from "@src/executors/ElfomofiExecutor.sol";
import {TransferManager} from "@src/TransferManager.sol";
import {ClientFeeParams} from "@src/TychoRouterV3.sol";

/// @dev Records the call it gets and pays a fixed output from its own
/// balance, reverting below the floor like ElFomoFi's router.
contract MockElfomofiRouter is IElfomofiRouter {
    address public fromToken;
    address public toToken;
    uint256 public minAmountOut;
    address public receiver;
    uint256 public partnerId;
    uint256 public amountIn;
    uint256 public payout;

    error BelowFloor(uint256 payout, uint256 floor);

    function setPayout(uint256 payout_) external {
        payout = payout_;
    }

    function swapWithContractBalance(
        address fromToken_,
        address toToken_,
        uint256 minAmountOut_,
        address receiver_,
        uint256 partnerId_
    ) external {
        if (payout < minAmountOut_) {
            revert BelowFloor(payout, minAmountOut_);
        }
        fromToken = fromToken_;
        toToken = toToken_;
        minAmountOut = minAmountOut_;
        receiver = receiver_;
        partnerId = partnerId_;
        amountIn = IERC20(fromToken_).balanceOf(address(this));
        IERC20(toToken_).transfer(receiver_, payout);
    }
}

contract ElfomofiExecutorTest is TestUtils, Constants {
    ElfomofiExecutor internal executor;
    MockElfomofiRouter internal router;
    address internal constant TOKEN_IN = address(0x1111);
    address internal constant TOKEN_OUT = address(0x2222);
    address internal constant NATIVE =
        0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE;

    function setUp() public {
        deployCodeTo("MockERC20.sol:MockERC20", TOKEN_IN);
        MockERC20(TOKEN_IN).initialize("In", "IN", 18);
        deployCodeTo("MockERC20.sol:MockERC20", TOKEN_OUT);
        MockERC20(TOKEN_OUT).initialize("Out", "OUT", 6);
        router = new MockElfomofiRouter();
        executor = new ElfomofiExecutor(address(router));
        deal(TOKEN_OUT, address(router), 1e30);
    }

    function _data(uint256 referenceIn, uint256 referenceMinOut)
        internal
        pure
        returns (bytes memory)
    {
        return
            abi.encodePacked(TOKEN_IN, TOKEN_OUT, referenceIn, referenceMinOut);
    }

    /// @dev Funds the router as the TychoRouter would, then swaps.
    function _swap(uint256 amountIn, bytes memory data) internal {
        deal(TOKEN_IN, address(router), amountIn);
        executor.swap(amountIn, data, BOB);
    }

    function testZeroRouterIsRefused() public {
        vm.expectRevert(ElfomofiExecutor.ElfomofiExecutor__ZeroAddress.selector);
        new ElfomofiExecutor(address(0));
    }

    function testTransferData() public view {
        (
            TransferManager.TransferType kind,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = executor.getTransferData(_data(1, 1));
        assertEq(uint8(kind), uint8(TransferManager.TransferType.Transfer));
        assertEq(receiver, address(router));
        assertEq(tokenIn, TOKEN_IN);
        assertEq(tokenOut, TOKEN_OUT);
        assertFalse(outputToRouter);
        assertEq(executor.fundsExpectedAddress(""), address(router));
    }

    function testInvalidLength() public {
        bytes memory data = _data(1, 1);
        bytes[2] memory invalid = [
            abi.encodePacked(data, uint8(0)),
            abi.encodePacked(TOKEN_IN, TOKEN_OUT, uint256(1))
        ];
        for (uint256 i; i < invalid.length; ++i) {
            vm.expectRevert(
                ElfomofiExecutor.ElfomofiExecutor__InvalidDataLength.selector
            );
            executor.getTransferData(invalid[i]);
        }
    }

    function testInvalidTokens() public {
        bytes[5] memory invalid = [
            abi.encodePacked(TOKEN_IN, TOKEN_IN, uint256(1), uint256(1)),
            abi.encodePacked(address(0), TOKEN_OUT, uint256(1), uint256(1)),
            abi.encodePacked(TOKEN_IN, address(0), uint256(1), uint256(1)),
            abi.encodePacked(NATIVE, TOKEN_OUT, uint256(1), uint256(1)),
            abi.encodePacked(TOKEN_IN, NATIVE, uint256(1), uint256(1))
        ];
        for (uint256 i; i < invalid.length; ++i) {
            vm.expectRevert(
                ElfomofiExecutor.ElfomofiExecutor__InvalidToken.selector
            );
            executor.swap(1, invalid[i], BOB);
        }
    }

    function testZeroAmounts() public {
        vm.expectRevert(ElfomofiExecutor.ElfomofiExecutor__ZeroAmount.selector);
        executor.swap(0, _data(1, 1), BOB);
        vm.expectRevert(ElfomofiExecutor.ElfomofiExecutor__ZeroAmount.selector);
        executor.swap(1, _data(0, 1), BOB);
        vm.expectRevert(ElfomofiExecutor.ElfomofiExecutor__ZeroAmount.selector);
        executor.swap(1, _data(1, 0), BOB);
    }

    /// @dev A third of the reference input keeps a third of the floor, rounded
    /// up: 10 / 3 is 4.
    function testFloorScalesDownRoundingUp() public {
        router.setPayout(4);
        _swap(1, _data(3, 10));
        assertEq(router.minAmountOut(), 4);
    }

    /// @dev An input at or above the reference keeps the absolute floor.
    function testFloorStaysAbsoluteAtOrAboveTheReference() public {
        router.setPayout(10);
        _swap(3, _data(3, 10));
        assertEq(router.minAmountOut(), 10);
        _swap(5, _data(3, 10));
        assertEq(router.minAmountOut(), 10);
    }

    function testForwardsTheSwap() public {
        router.setPayout(7);
        _swap(3, _data(3, 7));
        assertEq(router.fromToken(), TOKEN_IN);
        assertEq(router.toToken(), TOKEN_OUT);
        assertEq(router.receiver(), BOB);
        assertEq(router.partnerId(), executor.PARTNER_ID());
        assertEq(router.amountIn(), 3);
        assertEq(IERC20(TOKEN_OUT).balanceOf(BOB), 7);
    }

    function testOutputBelowTheFloorReverts() public {
        router.setPayout(9);
        deal(TOKEN_IN, address(router), 3);
        vm.expectRevert(
            abi.encodeWithSelector(
                MockElfomofiRouter.BelowFloor.selector, 9, 10
            )
        );
        executor.swap(3, _data(3, 10), BOB);
    }

    /// @dev The Rust encoder's bytes decode to what it priced: one WETH for at
    /// least 2,985 USDC.
    function testDecodesRustEncodedData() public view {
        bytes memory data = loadCallDataFromFile("test_encode_elfomofi");
        (,, address tokenIn, address tokenOut,) = executor.getTransferData(data);
        assertEq(tokenIn, 0x4200000000000000000000000000000000000006);
        assertEq(tokenOut, 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913);
    }
}

/// @dev The helper's view, from the calls this integration was built on.
interface IElfomofiHelper {
    struct CumulativeLevel {
        uint256 amountIn;
        uint256 amountOut;
    }

    struct PairOrderbook {
        address base;
        address quote;
        CumulativeLevel[] askCumulativeLevels;
        CumulativeLevel[] bidCumulativeLevels;
        uint256 balanceBase;
        uint256 balanceQuote;
    }

    function getOrderbook(address base, address quote)
        external
        view
        returns (PairOrderbook memory book, uint256 blockNumber, uint256 ts);
}

/// @dev ElFomoFi's router errors, as its fork traces decode them: an output
/// under the floor, and a vault that cannot pay.
interface IElfomofiErrors {
    error InsufficientAmount(uint256 minAmountOut, uint256 amountOut);
    error ExecutionFailed();
}

/// @dev Swaps through a fresh TychoRouter on a fork, and checks each output
/// against the helper's ladder at the fork block, interpolated as the
/// simulation interpolates it.
abstract contract ElfomofiForkTest is TychoRouterTestSetup {
    address internal constant ELFOMOFI_ROUTER =
        0xf0f0F0F0FB0d738452EfD03A28e8be14C76d5f73;
    /// @dev The Safe holding every pair's inventory.
    address internal constant VAULT =
        0xBb1b19F138dB3925883a96FF7a304277460E0C99;
    /// @dev Derived from the name, so deployments the shared setup adds do not
    /// move it, and the Rust-encoded calldata
    /// (config/test_executor_addresses.json) keeps addressing it.
    address internal constant ELFOMOFI_EXECUTOR =
        address(uint160(uint256(keccak256("tycho.test.ElfomofiExecutor"))));

    ElfomofiExecutor internal elfomofiExecutor;

    function helper() internal pure virtual returns (address);

    function setUp() public virtual override {
        super.setUp();
        deployCodeTo(
            "ElfomofiExecutor.sol:ElfomofiExecutor",
            abi.encode(ELFOMOFI_ROUTER),
            ELFOMOFI_EXECUTOR
        );
        elfomofiExecutor = ElfomofiExecutor(ELFOMOFI_EXECUTOR);
        address[] memory executors = new address[](1);
        executors[0] = ELFOMOFI_EXECUTOR;
        // Registered before the fork's timestamp, so the executor's activation
        // delay has passed when the tests swap at it.
        vm.warp(forkTimestamp - _SETUP_TIME_OFFSET_NEW_EXECUTOR);
        vm.prank(EXECUTOR_SETTER);
        tychoRouter.setExecutors(executors);
        vm.warp(forkTimestamp);
    }

    /// @dev One pair's book, as `getAllOrderbooks` reports it. The feed reads
    /// every book at once, but on Base that call reads stock tokens whose code
    /// is the single byte 0xEF, which forge cannot run.
    function _book(address base, address quote)
        internal
        view
        returns (IElfomofiHelper.PairOrderbook memory book)
    {
        (book,,) = IElfomofiHelper(helper()).getOrderbook(base, quote);
    }

    /// @dev What the simulation quotes for `amountIn` on `levels`.
    function _interpolate(
        IElfomofiHelper.CumulativeLevel[] memory levels,
        uint256 amountIn
    ) internal pure returns (uint256) {
        uint256 startIn;
        uint256 startOut;
        for (uint256 i; i < levels.length; ++i) {
            if (amountIn <= levels[i].amountIn) {
                return startOut + (amountIn - startIn)
                    * (levels[i].amountOut - startOut)
                    / (levels[i].amountIn - startIn);
            }
            startIn = levels[i].amountIn;
            startOut = levels[i].amountOut;
        }
        revert("past the ladder");
    }

    /// @dev Sells `amountIn` through the router with the given floor and
    /// returns what BOB received; checks nothing stays at the TychoRouter.
    function _routerSwap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 floor
    ) internal returns (uint256 received) {
        deal(tokenIn, BOB, amountIn);
        uint256 before = IERC20(tokenOut).balanceOf(BOB);
        vm.startPrank(BOB);
        IERC20(tokenIn).approve(tychoRouterAddr, amountIn);
        bytes memory swap = encodeSingleSwap(
            ELFOMOFI_EXECUTOR,
            abi.encodePacked(tokenIn, tokenOut, amountIn, floor)
        );
        tychoRouter.singleSwap(
            amountIn, tokenIn, tokenOut, floor, floor, BOB, noClientFee(), swap
        );
        vm.stopPrank();
        received = IERC20(tokenOut).balanceOf(BOB) - before;
        assertEq(IERC20(tokenIn).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(tokenOut).balanceOf(tychoRouterAddr), 0);
    }

    /// @dev Both directions of `base`/`quote` fill at least the ladder's
    /// interpolation, for a part of the first level, and at most `maxExcess`
    /// (1e18 = 100%) above it: the simulation never over-quotes.
    function _assertBothDirections(
        address base,
        address quote,
        uint256 maxExcess
    ) internal {
        IElfomofiHelper.PairOrderbook memory book = _book(base, quote);
        uint256 sellBase = book.bidCumulativeLevels[0].amountIn / 3;
        uint256 expectedQuote = _interpolate(book.bidCumulativeLevels, sellBase);
        uint256 receivedQuote =
            _routerSwap(base, quote, sellBase, expectedQuote);
        assertGe(receivedQuote, expectedQuote);
        assertApproxEqRel(receivedQuote, expectedQuote, maxExcess);

        uint256 sellQuote = book.askCumulativeLevels[0].amountIn / 3;
        uint256 expectedBase = _interpolate(book.askCumulativeLevels, sellQuote);
        uint256 receivedBase = _routerSwap(quote, base, sellQuote, expectedBase);
        assertGe(receivedBase, expectedBase);
        assertApproxEqRel(receivedBase, expectedBase, maxExcess);
    }

    /// @dev A floor one unit above the book reverts the whole swap and the
    /// input stays with the user.
    function _assertFloorReverts(address base, address quote) internal {
        IElfomofiHelper.PairOrderbook memory book = _book(base, quote);
        uint256 amountIn = book.bidCumulativeLevels[0].amountIn / 3;
        uint256 floor = _interpolate(book.bidCumulativeLevels, amountIn) + 1;
        deal(base, BOB, amountIn);
        vm.startPrank(BOB);
        IERC20(base).approve(tychoRouterAddr, amountIn);
        bytes memory swap = encodeSingleSwap(
            ELFOMOFI_EXECUTOR, abi.encodePacked(base, quote, amountIn, floor)
        );
        vm.expectRevert(
            abi.encodeWithSelector(
                IElfomofiErrors.InsufficientAmount.selector, floor, floor - 1
            )
        );
        tychoRouter.singleSwap(
            amountIn, base, quote, 1, 1, BOB, noClientFee(), swap
        );
        vm.stopPrank();
        assertEq(IERC20(base).balanceOf(BOB), amountIn);
    }

    /// @dev Gas of the executor's swap alone (the ElFomoFi router call and the
    /// executor around it), the protocol part the simulation reports, with the
    /// input already at the ElFomoFi router as the TychoRouter leaves it.
    function _measureGas(address base, address quote) internal {
        IElfomofiHelper.PairOrderbook memory book = _book(base, quote);
        uint256 amountIn = book.bidCumulativeLevels[0].amountIn / 3;
        deal(
            base,
            ELFOMOFI_ROUTER,
            IERC20(base).balanceOf(ELFOMOFI_ROUTER) + amountIn
        );
        bytes memory data = abi.encodePacked(base, quote, amountIn, uint256(1));
        uint256 start = gasleft();
        elfomofiExecutor.swap(amountIn, data, BOB);
        console.log("ElFomoFi protocol gas", start - gasleft());
    }
}

contract ElfomofiBaseTest is ElfomofiForkTest {
    address internal constant WETH = 0x4200000000000000000000000000000000000006;
    address internal constant USDC = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address internal constant CBBTC =
        0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf;

    function getChain() public pure override returns (string memory) {
        return "base";
    }

    /// @dev After the pricing upgrade of 2026-11-20 (59,985,486), with WETH
    /// and cbBTC books quoted both ways and about 10,758 USDC in the vault.
    function getForkBlock() public pure override returns (uint256) {
        return 61817000;
    }

    function helper() internal pure override returns (address) {
        return 0xc1b13606FC7227f2554067aFb3fb12De75C02d81;
    }

    function testBothDirections() public {
        // Exact to the wei on Base.
        _assertBothDirections(WETH, USDC, 0);
        _assertBothDirections(CBBTC, USDC, 0);
    }

    function testFloorAboveTheBookReverts() public {
        _assertFloorReverts(WETH, USDC);
    }

    function testGas() public {
        _measureGas(WETH, USDC);
    }

    /// @dev The WETH and cbBTC bids each reach the vault's whole USDC balance.
    /// Filling one to the end empties the vault's USDC, and the other's sale
    /// then reverts in the same block: the vault cannot pay it.
    function testPairsShareTheVault() public {
        IElfomofiHelper.PairOrderbook memory weth = _book(WETH, USDC);
        IElfomofiHelper.PairOrderbook memory cbbtc = _book(CBBTC, USDC);
        uint256 wethIn =
            weth.bidCumulativeLevels[weth.bidCumulativeLevels.length
                - 1].amountIn;
        uint256 cbbtcIn =
            cbbtc.bidCumulativeLevels[cbbtc.bidCumulativeLevels.length
                - 1].amountIn;
        uint256 wethOut = _interpolate(weth.bidCumulativeLevels, wethIn);
        assertEq(_routerSwap(WETH, USDC, wethIn, wethOut), wethOut);
        assertLt(IERC20(USDC).balanceOf(VAULT), 10);

        uint256 cbbtcOut = _interpolate(cbbtc.bidCumulativeLevels, cbbtcIn);
        deal(CBBTC, BOB, cbbtcIn);
        vm.startPrank(BOB);
        IERC20(CBBTC).approve(tychoRouterAddr, cbbtcIn);
        bytes memory swap = encodeSingleSwap(
            ELFOMOFI_EXECUTOR, abi.encodePacked(CBBTC, USDC, cbbtcIn, cbbtcOut)
        );
        vm.expectRevert(IElfomofiErrors.ExecutionFailed.selector);
        tychoRouter.singleSwap(
            cbbtcIn, CBBTC, USDC, cbbtcOut, cbbtcOut, BOB, noClientFee(), swap
        );
        vm.stopPrank();
    }

    /// @dev With a signed 1% client fee, the venue still pays the whole quote
    /// out of its vault against the executor's floor, and the fees come out of
    /// that output after the swap.
    function testClientFeePath() public {
        IElfomofiHelper.PairOrderbook memory book = _book(WETH, USDC);
        uint256 amountIn = book.bidCumulativeLevels[0].amountIn / 3;
        uint256 quoted = _interpolate(book.bidCumulativeLevels, amountIn);
        uint256 minAmountOut = quoted * 97 / 100;
        bytes memory swap = encodeSingleSwap(
            ELFOMOFI_EXECUTOR, abi.encodePacked(WETH, USDC, amountIn, quoted)
        );
        ClientFeeParams memory feeParams = makeClientFeeParams(
            100,
            0,
            amountIn,
            WETH,
            USDC,
            quoted,
            minAmountOut,
            BOB,
            swap,
            tychoRouterAddr,
            ALICE_PK
        );
        uint256 vaultBefore = IERC20(USDC).balanceOf(VAULT);
        deal(WETH, BOB, amountIn);
        vm.startPrank(BOB);
        IERC20(WETH).approve(tychoRouterAddr, amountIn);
        uint256 received = tychoRouter.singleSwap(
            amountIn, WETH, USDC, quoted, minAmountOut, BOB, feeParams, swap
        );
        vm.stopPrank();

        assertEq(vaultBefore - IERC20(USDC).balanceOf(VAULT), quoted);
        assertEq(IERC20(USDC).balanceOf(BOB), received);
        assertLt(received, quoted);
        assertGe(received, minAmountOut);
    }

    /// @dev The Rust-encoded router call sells 0.1 WETH with a floor the
    /// encoder took from `min_amount_out`.
    function testRustEncodedRouterCall() public {
        deal(WETH, BOB, 1e17);
        vm.startPrank(BOB);
        IERC20(WETH).approve(tychoRouterAddr, 1e17);
        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile("test_single_encoding_strategy_elfomofi")
        );
        vm.stopPrank();
        assertTrue(success, "Rust-encoded ElFomoFi swap failed");
        IElfomofiHelper.PairOrderbook memory book = _book(WETH, USDC);
        assertGe(
            IERC20(USDC).balanceOf(BOB),
            _interpolate(book.bidCumulativeLevels, 1e17) * 99 / 100
        );
        assertEq(IERC20(WETH).balanceOf(tychoRouterAddr), 0);
    }
}

contract ElfomofiBscTest is ElfomofiForkTest {
    address internal constant ETH = 0x2170Ed0880ac9A755fd29B2688956BD959F933F8;
    address internal constant USDT = 0x55d398326f99059fF775485246999027B3197955;

    function getChain() public pure override returns (string memory) {
        return "bsc";
    }

    /// @dev After the pricing upgrade of 2026-11-20 (134,716,965), with the
    /// ETH book quoted both ways.
    function getForkBlock() public pure override returns (uint256) {
        return 134869000;
    }

    function helper() internal pure override returns (address) {
        return 0x78015E3544d989f0712a0b9986cB05838c3fE06D;
    }

    function testBothDirections() public {
        // Exact to the wei on BSC.
        _assertBothDirections(ETH, USDT, 0);
    }

    function testFloorAboveTheBookReverts() public {
        _assertFloorReverts(ETH, USDT);
    }

    function testGas() public {
        _measureGas(ETH, USDT);
    }
}

/// @dev Robinhood is an Arbitrum chain: the venue reads ArbSys's block number,
/// which forge does not provide, so it is mocked to the fork's.
contract ElfomofiRobinhoodTest is ElfomofiForkTest {
    address internal constant NVDA = 0xd0601CE157Db5bdC3162BbaC2a2C8aF5320D9EEC;
    address internal constant USDG = 0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168;
    address internal constant ARB_SYS = address(0x64);

    function setUp() public override {
        super.setUp();
        vm.mockCall(
            ARB_SYS,
            abi.encodeWithSignature("arbBlockNumber()"),
            abi.encode(block.number)
        );
    }

    function getChain() public pure override returns (string memory) {
        return "robinhood";
    }

    /// @dev After the pricing upgrade of 2026-11-20 (119,145,799), with the
    /// NVDA book quoted both ways.
    function getForkBlock() public pure override returns (uint256) {
        return 121390000;
    }

    function helper() internal pure override returns (address) {
        return 0x20D24Ee45c9b338Ef7ACf2eEde75183B9E2C9E3c;
    }

    function testBothDirections() public {
        // Buying NVDA pays about 1.5e-9 more than the interpolation at this
        // block; selling it is exact.
        _assertBothDirections(NVDA, USDG, 1e10);
    }

    function testFloorAboveTheBookReverts() public {
        _assertFloorReverts(NVDA, USDG);
    }

    function testGas() public {
        _measureGas(NVDA, USDG);
    }
}
