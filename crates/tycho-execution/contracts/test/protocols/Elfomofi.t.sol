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
import {ClientFeeParams, TychoRouterV3} from "@src/TychoRouterV3.sol";

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
        assertEq(router.partnerId(), 299);
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

/// @dev Swaps through a TychoRouter on a fork, the shared setup's fresh one or
/// the chain's production one, and checks each output against the helper's
/// ladder at the fork block, interpolated as the simulation interpolates it.
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

    /// @dev Holds EXECUTOR_SETTER_ROLE on the production TychoRouter of each
    /// chain these tests fork, at their fork blocks.
    address internal constant PRODUCTION_EXECUTOR_SETTER =
        0x613112ACD43d95d4bE257Dae562Fb8Ea4d7ED455;

    ElfomofiExecutor internal elfomofiExecutor;

    /// @dev The router the swaps go through.
    TychoRouterV3 internal router;

    function helper() internal pure virtual returns (address);

    /// @dev The chain's TychoRouter, as config/router_addresses.json lists it.
    function productionRouter() internal pure virtual returns (address);

    function setUp() public virtual override {
        super.setUp();
        deployCodeTo(
            "ElfomofiExecutor.sol:ElfomofiExecutor",
            abi.encode(ELFOMOFI_ROUTER),
            ELFOMOFI_EXECUTOR
        );
        elfomofiExecutor = ElfomofiExecutor(ELFOMOFI_EXECUTOR);
        _useRouter(TychoRouterV3(payable(tychoRouterAddr)), EXECUTOR_SETTER);
    }

    /// @dev Registers the executor on `target`, before the fork's timestamp so
    /// its activation delay has passed when the tests swap at it, and swaps
    /// through `target` from then on.
    function _useRouter(TychoRouterV3 target, address executorSetter) internal {
        address[] memory executors = new address[](1);
        executors[0] = ELFOMOFI_EXECUTOR;
        vm.warp(forkTimestamp - _SETUP_TIME_OFFSET_NEW_EXECUTOR);
        vm.prank(executorSetter);
        target.setExecutors(executors);
        vm.warp(forkTimestamp);
        router = target;
    }

    /// @dev The production router is the caller the venue's pricing sees in
    /// production, and the pricing contract takes the caller as an input.
    function _useProductionRouter() internal {
        _useRouter(
            TychoRouterV3(payable(productionRouter())),
            PRODUCTION_EXECUTOR_SETTER
        );
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

    /// @dev Sells `amountIn` through `router` with the given floor and a
    /// signed 1% client fee, the path most routes take, and returns what the
    /// venue paid out of its vault. Checks the venue paid the router, BOB got
    /// that less the fees, the router kept only the fees, and none of the
    /// input stayed with it.
    function _routerSwap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 floor
    ) internal returns (uint256 paid) {
        bytes memory swap = encodeSingleSwap(
            ELFOMOFI_EXECUTOR,
            abi.encodePacked(tokenIn, tokenOut, amountIn, floor)
        );
        uint256 minAmountOut = floor * 98 / 100;
        ClientFeeParams memory feeParams = makeClientFeeParams(
            1_000_000,
            0,
            amountIn,
            tokenIn,
            tokenOut,
            floor,
            minAmountOut,
            BOB,
            swap,
            address(router),
            ALICE_PK
        );
        deal(tokenIn, BOB, amountIn);
        uint256 vaultBefore = IERC20(tokenOut).balanceOf(VAULT);
        uint256 bobBefore = IERC20(tokenOut).balanceOf(BOB);
        uint256 routerInBefore = IERC20(tokenIn).balanceOf(address(router));
        uint256 routerOutBefore = IERC20(tokenOut).balanceOf(address(router));
        vm.startPrank(BOB);
        IERC20(tokenIn).approve(address(router), amountIn);
        uint256 received = router.singleSwap(
            amountIn,
            tokenIn,
            tokenOut,
            floor,
            minAmountOut,
            BOB,
            feeParams,
            swap
        );
        vm.stopPrank();

        paid = vaultBefore - IERC20(tokenOut).balanceOf(VAULT);
        assertEq(IERC20(tokenOut).balanceOf(BOB) - bobBefore, received);
        assertLt(received, paid);
        assertGe(received, minAmountOut);
        assertEq(
            IERC20(tokenOut).balanceOf(address(router)) - routerOutBefore,
            paid - received
        );
        assertEq(IERC20(tokenIn).balanceOf(address(router)), routerInBefore);
    }

    /// @dev Both directions of `base`/`quote` pay at least the ladder's
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
        uint256 paidQuote = _routerSwap(base, quote, sellBase, expectedQuote);
        assertGe(paidQuote, expectedQuote);
        assertApproxEqRel(paidQuote, expectedQuote, maxExcess);

        uint256 sellQuote = book.askCumulativeLevels[0].amountIn / 3;
        uint256 expectedBase = _interpolate(book.askCumulativeLevels, sellQuote);
        uint256 paidBase = _routerSwap(quote, base, sellQuote, expectedBase);
        assertGe(paidBase, expectedBase);
        assertApproxEqRel(paidBase, expectedBase, maxExcess);
    }

    /// @dev A floor one unit above the book reverts the whole swap and the
    /// input stays with the user.
    function _assertFloorReverts(address base, address quote) internal {
        IElfomofiHelper.PairOrderbook memory book = _book(base, quote);
        uint256 amountIn = book.bidCumulativeLevels[0].amountIn / 3;
        uint256 floor = _interpolate(book.bidCumulativeLevels, amountIn) + 1;
        deal(base, BOB, amountIn);
        vm.startPrank(BOB);
        IERC20(base).approve(address(router), amountIn);
        bytes memory swap = encodeSingleSwap(
            ELFOMOFI_EXECUTOR, abi.encodePacked(base, quote, amountIn, floor)
        );
        vm.expectRevert(
            abi.encodeWithSelector(
                IElfomofiErrors.InsufficientAmount.selector, floor, floor - 1
            )
        );
        router.singleSwap(amountIn, base, quote, 1, 1, BOB, noClientFee(), swap);
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
    /// @dev Uniswap V3's WETH/cbBTC pool at 0.05%.
    address internal constant WETH_CBBTC_USV3 =
        0x7AeA2E8A3843516afa07293a10Ac8E49906dabD1;
    uint24 internal constant HALF = 0x7fffff;

    function getChain() public pure override returns (string memory) {
        return "base";
    }

    function productionRouter() internal pure override returns (address) {
        return 0xd374c00614f95492D816b5957E3797ce3f0c445C;
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

    function testProductionRouterBothDirections() public {
        _useProductionRouter();
        _assertBothDirections(WETH, USDC, 0);
    }

    /// @dev WETH split in two: `HALF` of it sold on the WETH book, the rest
    /// swapped to cbBTC on Uniswap V3 and sold on the cbBTC book. Both legs pay
    /// USDC out of the one vault in the same transaction, the cbBTC leg funded
    /// only by the Uniswap leg's output.
    function _splitSwaps(bytes memory wethLeg, bytes memory cbbtcLeg)
        internal
        view
        returns (bytes memory)
    {
        bytes[] memory swaps = new bytes[](3);
        swaps[0] = encodeSplitSwap(0, 2, HALF, ELFOMOFI_EXECUTOR, wethLeg);
        swaps[1] = encodeSplitSwap(
            0,
            1,
            0,
            address(usv3Executor),
            encodeUniswapV3Swap(WETH, CBBTC, WETH_CBBTC_USV3, true)
        );
        swaps[2] = encodeSplitSwap(1, 2, 0, ELFOMOFI_EXECUTOR, cbbtcLeg);
        return pleEncode(swaps);
    }

    function _fundBob(uint256 amountIn) internal {
        deal(WETH, BOB, amountIn);
        vm.prank(BOB);
        IERC20(WETH).approve(tychoRouterAddr, amountIn);
    }

    /// @dev The cbBTC leg's floor is priced for 0.02 cbBTC, more than the
    /// Uniswap leg delivers, so the executor scales it down to what arrives.
    /// The vault pays each book's interpolation for the input it got.
    function testSplitRouteThroughBothBooks() public {
        IElfomofiHelper.PairOrderbook memory weth = _book(WETH, USDC);
        IElfomofiHelper.PairOrderbook memory cbbtc = _book(CBBTC, USDC);
        uint256 amountIn = 1 ether;
        uint256 wethIn = amountIn * HALF / 0xffffff;
        uint256 wethOut = _interpolate(weth.bidCumulativeLevels, wethIn);
        uint256 cbbtcReference = 2e6;
        uint256 cbbtcFloor =
            _interpolate(cbbtc.bidCumulativeLevels, cbbtcReference) * 99 / 100;
        uint256 vaultUsdc = IERC20(USDC).balanceOf(VAULT);
        uint256 vaultCbbtc = IERC20(CBBTC).balanceOf(VAULT);

        bytes memory swaps = _splitSwaps(
            abi.encodePacked(WETH, USDC, wethIn, wethOut),
            abi.encodePacked(CBBTC, USDC, cbbtcReference, cbbtcFloor)
        );
        _fundBob(amountIn);
        vm.prank(BOB);
        uint256 received = tychoRouter.splitSwap(
            amountIn, WETH, USDC, 1, 1, 3, BOB, noClientFee(), swaps
        );

        uint256 cbbtcIn = IERC20(CBBTC).balanceOf(VAULT) - vaultCbbtc;
        assertGt(cbbtcIn, 0);
        assertLt(cbbtcIn, cbbtcReference);
        uint256 paid = vaultUsdc - IERC20(USDC).balanceOf(VAULT);
        assertEq(
            paid, wethOut + _interpolate(cbbtc.bidCumulativeLevels, cbbtcIn)
        );
        assertEq(received, paid);
        assertEq(IERC20(USDC).balanceOf(BOB), received);
        assertEq(IERC20(WETH).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(CBBTC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC).balanceOf(tychoRouterAddr), 0);
    }

    /// @dev Each book counts the vault's whole USDC balance. Split 10 WETH:
    /// each leg alone fits in that balance but the two together do not. The
    /// second leg the router runs then pays only what is left in the vault,
    /// below its floor, and the whole transaction reverts on that floor.
    function testSplitRoutePastTheSharedVaultRevertsOnTheFloor() public {
        IElfomofiHelper.PairOrderbook memory weth = _book(WETH, USDC);
        IElfomofiHelper.PairOrderbook memory cbbtc = _book(CBBTC, USDC);
        uint256 amountIn = 10 ether;
        uint256 wethIn = amountIn * HALF / 0xffffff;
        uint256 cbbtcIn = _usv3CbbtcFor(amountIn - wethIn);
        uint256 wethOut = _interpolate(weth.bidCumulativeLevels, wethIn);
        uint256 cbbtcOut = _interpolate(cbbtc.bidCumulativeLevels, cbbtcIn);
        uint256 vaultUsdc = IERC20(USDC).balanceOf(VAULT);
        assertLt(wethOut, vaultUsdc);
        assertLt(cbbtcOut, vaultUsdc);
        assertGt(wethOut + cbbtcOut, vaultUsdc);

        bytes memory swaps = _splitSwaps(
            abi.encodePacked(WETH, USDC, wethIn, wethOut),
            abi.encodePacked(CBBTC, USDC, cbbtcIn, cbbtcOut * 99 / 100)
        );
        _fundBob(amountIn);
        vm.prank(BOB);
        vm.expectPartialRevert(IElfomofiErrors.InsufficientAmount.selector);
        tychoRouter.splitSwap(
            amountIn, WETH, USDC, 1, 1, 3, BOB, noClientFee(), swaps
        );
    }

    /// @dev The cbBTC that `wethIn` buys on the Uniswap leg, from a swap that
    /// is rolled back.
    function _usv3CbbtcFor(uint256 wethIn) internal returns (uint256 out) {
        uint256 snapshot = vm.snapshotState();
        deal(WETH, BOB, wethIn);
        vm.startPrank(BOB);
        IERC20(WETH).approve(tychoRouterAddr, wethIn);
        out = tychoRouter.singleSwap(
            wethIn,
            WETH,
            CBBTC,
            1,
            1,
            BOB,
            noClientFee(),
            encodeSingleSwap(
                address(usv3Executor),
                encodeUniswapV3Swap(WETH, CBBTC, WETH_CBBTC_USV3, true)
            )
        );
        vm.stopPrank();
        vm.revertToState(snapshot);
    }

    /// @dev The Rust-encoded route sells 0.01 cbBTC on the cbBTC book and
    /// buys WETH with the USDC on the WETH book. The second hop is funded only
    /// by the first one's output, less than the 600 USDC its floor was priced
    /// for: it fills because the executor scales that floor down. The USDC the
    /// vault pays on the first hop comes straight back on the second.
    function testRustEncodedSequentialRoute() public {
        IElfomofiHelper.PairOrderbook memory cbbtc = _book(CBBTC, USDC);
        IElfomofiHelper.PairOrderbook memory weth = _book(WETH, USDC);
        uint256 usdc = _interpolate(cbbtc.bidCumulativeLevels, 1e6);
        uint256 expected = _interpolate(weth.askCumulativeLevels, usdc);
        assertLt(usdc, 600e6);
        assertGt(0.358 ether, expected);
        uint256 vaultUsdc = IERC20(USDC).balanceOf(VAULT);
        uint256 vaultWeth = IERC20(WETH).balanceOf(VAULT);
        uint256 bobWeth = IERC20(WETH).balanceOf(BOB);
        deal(CBBTC, BOB, 1e6);
        vm.startPrank(BOB);
        IERC20(CBBTC).approve(tychoRouterAddr, 1e6);
        (bool success, bytes memory result) = tychoRouterAddr.call(
            loadCallDataFromFile("test_sequential_encoding_strategy_elfomofi")
        );
        vm.stopPrank();

        assertTrue(success, "Rust-encoded ElFomoFi route failed");
        assertEq(abi.decode(result, (uint256)), expected);
        assertEq(IERC20(WETH).balanceOf(BOB) - bobWeth, expected);
        assertEq(IERC20(USDC).balanceOf(VAULT), vaultUsdc);
        assertEq(vaultWeth - IERC20(WETH).balanceOf(VAULT), expected);
        assertEq(IERC20(CBBTC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(WETH).balanceOf(tychoRouterAddr), 0);
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

    function productionRouter() internal pure override returns (address) {
        return 0xEe7eCe626064BDA1c6aFDb8A281afe44Eabd3242;
    }

    function testBothDirections() public {
        // Exact to the wei on BSC.
        _assertBothDirections(ETH, USDT, 0);
    }

    function testProductionRouterBothDirections() public {
        _useProductionRouter();
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

    function productionRouter() internal pure override returns (address) {
        return 0x6F460540B03651d2Ad0ec28f777e4A0480A756a5;
    }

    function testBothDirections() public {
        // Buying NVDA pays about 1.5e-9 more than the interpolation at this
        // block; selling it is exact.
        _assertBothDirections(NVDA, USDG, 1e10);
    }

    function testProductionRouterBothDirections() public {
        _useProductionRouter();
        _assertBothDirections(NVDA, USDG, 1e10);
    }

    function testFloorAboveTheBookReverts() public {
        _assertFloorReverts(NVDA, USDG);
    }

    function testGas() public {
        _measureGas(NVDA, USDG);
    }
}
