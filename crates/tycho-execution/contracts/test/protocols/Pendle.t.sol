// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/PendleExecutor.sol";
import {Constants} from "../Constants.sol";

contract PendleExecutorExposed is PendleExecutor {
    constructor(address _router) PendleExecutor(_router) {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (
            PendleEdge edge,
            address tokenIn,
            address tokenOut,
            address target
        )
    {
        return _decodeData(data);
    }
}

/// Exercises the six edges of the wstETH market expiring 30 Dec 2027 against a mainnet fork.
contract PendleExecutorTest is Constants, TestUtils {
    using SafeERC20 for IERC20;

    PendleExecutorExposed pendleExposed;

    // The per-edge gas the simulation quotes with, one constant per direction: the exact-SY-in
    // directions run the router's approximation loop on chain and cost 27-40% more than their
    // exact-PT and exact-YT counterparts. Each edge is measured in its own test so it runs against a cold
    // fork, which is the state a real swap meets.
    //
    // The measurement covers the executor's `swap` call only. The router's `approve` is added
    // separately, per leg, by `pendle_leg_needs_approval` in `gas_estimator.rs`, so it must not
    // be here.
    //
    // The values are read from the simulation's source rather than copied, so a change there is
    // checked against the chain here.
    string constant SIMULATION_STATE =
        "../../tycho-simulation/src/evm/protocol/pendle/state.rs";
    uint256 gasSyForPt;
    uint256 gasPtForSy;
    uint256 gasSyForYt;
    uint256 gasYtForSy;
    // The wstETH SY converts wstETH `one_to_one`, and stETH and WETH at the `index_rate`, which
    // reads through to the wrapped protocol and wraps or unwraps on the way.
    uint256 gasWrapOneToOne;
    uint256 gasWrapIndexRate;

    IERC20 sy = IERC20(PENDLE_WSTETH_SY);
    IERC20 pt = IERC20(PENDLE_WSTETH_PT);
    IERC20 yt = IERC20(PENDLE_WSTETH_YT);
    IERC20 wstETH = IERC20(WSTETH_ADDR);

    function setUp() public {
        uint256 forkBlock = 25800000;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        pendleExposed = new PendleExecutorExposed(PENDLE_ROUTER_V4);

        string memory source = vm.readFile(SIMULATION_STATE);
        gasSyForPt = _simulationGas(source, "GAS_SY_FOR_PT");
        gasPtForSy = _simulationGas(source, "GAS_PT_FOR_SY");
        gasSyForYt = _simulationGas(source, "GAS_SY_FOR_YT");
        gasYtForSy = _simulationGas(source, "GAS_YT_FOR_SY");
        gasWrapOneToOne = _simulationGas(source, "GAS_WRAP_ONE_TO_ONE");
        gasWrapIndexRate = _simulationGas(source, "GAS_WRAP_INDEX_RATE");
    }

    /// The value of `const <name>: u64 = <value>;` in the simulation's source, which must
    /// declare it exactly once.
    function _simulationGas(string memory source, string memory name)
        internal
        pure
        returns (uint256)
    {
        string[] memory parts =
            vm.split(source, string.concat("const ", name, ": u64 = "));
        require(
            parts.length == 2,
            string.concat(name, " is not declared once in ", SIMULATION_STATE)
        );
        string memory literal = vm.split(parts[1], ";")[0];
        return vm.parseUint(vm.replace(literal, "_", ""));
    }

    function _data(
        PendleEdge edge,
        address tokenIn,
        address tokenOut,
        address target
    ) internal pure returns (bytes memory) {
        return abi.encodePacked(uint8(edge), tokenIn, tokenOut, target);
    }

    /// Funds the executor and gives the contract that will pull the tokens an allowance, which
    /// is what TransferManager does for a `ProtocolWillDebit` executor before delegatecalling.
    function _fund(address token, uint256 amount, address spender) internal {
        deal(token, address(pendleExposed), amount);
        vm.prank(address(pendleExposed));
        IERC20(token).forceApprove(spender, amount);
    }

    function testDecodeParams() public view {
        bytes memory params = _data(
            PendleEdge.SyForPt,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_PT,
            PENDLE_WSTETH_MARKET
        );

        (PendleEdge edge, address tokenIn, address tokenOut, address target) =
            pendleExposed.decodeParams(params);

        assertEq(uint8(edge), uint8(PendleEdge.SyForPt));
        assertEq(tokenIn, PENDLE_WSTETH_SY);
        assertEq(tokenOut, PENDLE_WSTETH_PT);
        assertEq(target, PENDLE_WSTETH_MARKET);
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory params = abi.encodePacked(
            uint8(0),
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_PT,
            PENDLE_WSTETH_MARKET,
            uint8(0)
        );

        vm.expectRevert(PendleExecutor__InvalidDataLength.selector);
        pendleExposed.decodeParams(params);
    }

    function testDecodeParamsInvalidEdge() public {
        bytes memory params = abi.encodePacked(
            uint8(6), PENDLE_WSTETH_SY, PENDLE_WSTETH_PT, PENDLE_WSTETH_MARKET
        );

        vm.expectRevert(PendleExecutor__InvalidEdge.selector);
        pendleExposed.decodeParams(params);
    }

    function testGetTransferDataMarketEdge() public view {
        bytes memory params = _data(
            PendleEdge.PtForSy,
            PENDLE_WSTETH_PT,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_MARKET
        );

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = pendleExposed.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        // The market edges are pulled by Router V4, not by the market itself.
        assertEq(receiver, PENDLE_ROUTER_V4);
        assertEq(tokenIn, PENDLE_WSTETH_PT);
        assertEq(tokenOut, PENDLE_WSTETH_SY);
        assertEq(outputToRouter, false);
    }

    function testGetTransferDataSyEdge() public view {
        bytes memory params = _data(
            PendleEdge.TokenForSy,
            WSTETH_ADDR,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_SY
        );

        (, address receiver, address tokenIn, address tokenOut,) =
            pendleExposed.getTransferData(params);

        assertEq(receiver, PENDLE_WSTETH_SY);
        assertEq(tokenIn, WSTETH_ADDR);
        assertEq(tokenOut, PENDLE_WSTETH_SY);
    }

    /// `SY.redeem` burns the caller's shares without `transferFrom`, so a redemption names the
    /// caller, which the Dispatcher does not approve, rather than the SY.
    function testGetTransferDataRedeemNeedsNoApproval() public {
        bytes memory params = _data(
            PendleEdge.SyForToken,
            PENDLE_WSTETH_SY,
            WSTETH_ADDR,
            PENDLE_WSTETH_SY
        );

        vm.prank(ALICE);
        (, address receiver,,,) = pendleExposed.getTransferData(params);

        assertEq(receiver, ALICE);
    }

    function testSwapSyForTokenWithoutAllowance() public {
        deal(PENDLE_WSTETH_SY, address(pendleExposed), 1e18);
        uint256 balanceBefore = wstETH.balanceOf(BOB);

        pendleExposed.swap(
            1e18,
            _data(
                PendleEdge.SyForToken,
                PENDLE_WSTETH_SY,
                WSTETH_ADDR,
                PENDLE_WSTETH_SY
            ),
            BOB
        );

        assertGt(wstETH.balanceOf(BOB), balanceBefore);
        assertEq(sy.balanceOf(address(pendleExposed)), 0);
    }

    function testSwapSyForPt() public {
        uint256 balanceBefore = pt.balanceOf(BOB);

        uint256 gasUsed = _measure(
            PendleEdge.SyForPt,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_PT,
            PENDLE_WSTETH_MARKET,
            PENDLE_ROUTER_V4
        );

        assertGt(pt.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, gasSyForPt);
    }

    function testSwapPtForSy() public {
        uint256 balanceBefore = sy.balanceOf(BOB);

        uint256 gasUsed = _measure(
            PendleEdge.PtForSy,
            PENDLE_WSTETH_PT,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_MARKET,
            PENDLE_ROUTER_V4
        );

        assertGt(sy.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, gasPtForSy);
    }

    function testSwapSyForYt() public {
        uint256 balanceBefore = yt.balanceOf(BOB);

        uint256 gasUsed = _measure(
            PendleEdge.SyForYt,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_YT,
            PENDLE_WSTETH_MARKET,
            PENDLE_ROUTER_V4
        );

        assertGt(yt.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, gasSyForYt);
    }

    function testSwapYtForSy() public {
        uint256 balanceBefore = sy.balanceOf(BOB);

        uint256 gasUsed = _measure(
            PendleEdge.YtForSy,
            PENDLE_WSTETH_YT,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_MARKET,
            PENDLE_ROUTER_V4
        );

        assertGt(sy.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, gasYtForSy);
    }

    function testSwapTokenForSy() public {
        uint256 balanceBefore = sy.balanceOf(BOB);

        uint256 gasUsed = _measure(
            PendleEdge.TokenForSy,
            WSTETH_ADDR,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_SY
        );

        assertGt(sy.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, gasWrapOneToOne);
    }

    function testSwapSyForToken() public {
        uint256 balanceBefore = wstETH.balanceOf(BOB);

        uint256 gasUsed = _measure(
            PendleEdge.SyForToken,
            PENDLE_WSTETH_SY,
            WSTETH_ADDR,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_SY
        );

        assertGt(wstETH.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, gasWrapOneToOne);
    }

    /// stETH is share-based, so `deal` cannot set a balance; it is minted by staking instead.
    function _fundSteth(uint256 amount) internal returns (uint256 funded) {
        vm.deal(address(this), amount + 1 ether);
        (bool ok,) = STETH_ADDR.call{value: amount + 1 ether}(
            abi.encodeWithSignature("submit(address)", address(0))
        );
        require(ok, "stETH submit failed");
        IERC20(STETH_ADDR).safeTransfer(address(pendleExposed), amount);
        // A stETH transfer rounds down to whole shares.
        funded = IERC20(STETH_ADDR).balanceOf(address(pendleExposed));
        vm.prank(address(pendleExposed));
        IERC20(STETH_ADDR).forceApprove(PENDLE_WSTETH_SY, funded);
    }

    function testSwapStethForSyAtTheIndexRate() public {
        uint256 amountIn = _fundSteth(1e18);
        uint256 balanceBefore = sy.balanceOf(BOB);
        bytes memory data = _data(
            PendleEdge.TokenForSy,
            STETH_ADDR,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_SY
        );

        uint256 before = gasleft();
        pendleExposed.swap(amountIn, data, BOB);
        uint256 gasUsed = before - gasleft();
        console.log("pendle edge gas steth->sy", gasUsed);

        assertGt(sy.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, gasWrapIndexRate);
    }

    function testSwapSyForStethAtTheIndexRate() public {
        deal(PENDLE_WSTETH_SY, address(pendleExposed), 1e18);
        uint256 balanceBefore = IERC20(STETH_ADDR).balanceOf(BOB);
        bytes memory data = _data(
            PendleEdge.SyForToken,
            PENDLE_WSTETH_SY,
            STETH_ADDR,
            PENDLE_WSTETH_SY
        );

        uint256 before = gasleft();
        pendleExposed.swap(1e18, data, BOB);
        uint256 gasUsed = before - gasleft();
        console.log("pendle edge gas sy->steth", gasUsed);

        assertGt(IERC20(STETH_ADDR).balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, gasWrapIndexRate);
    }

    /// WETH is the SY's other `index_rate` entry token, and its dearest: the SY unwraps it, stakes
    /// the ETH with Lido and wraps the stETH, where a stETH deposit only does the last step.
    function testSwapWethForSyAtTheIndexRate() public {
        uint256 balanceBefore = sy.balanceOf(BOB);

        uint256 gasUsed = _measure(
            PendleEdge.TokenForSy,
            WETH_ADDR,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_SY
        );

        assertGt(sy.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, gasWrapIndexRate);
    }

    /// Funds the executor, runs one edge, and returns the gas the `swap` call itself consumed.
    function _measure(
        PendleEdge edge,
        address tokenIn,
        address tokenOut,
        address target,
        address spender
    ) internal returns (uint256 gasUsed) {
        uint256 amountIn = 1e18;
        _fund(tokenIn, amountIn, spender);
        bytes memory data = _data(edge, tokenIn, tokenOut, target);

        uint256 before = gasleft();
        pendleExposed.swap(amountIn, data, BOB);
        gasUsed = before - gasleft();
        console.log("pendle edge gas", uint8(edge), gasUsed);
    }

    function testSwapTokenForSyRejectsMismatchedTarget() public {
        // The SY edges call the token in the calldata; a target that is not the SY being
        // minted has to fail rather than call an arbitrary contract.
        vm.expectRevert(PendleExecutor__InvalidTarget.selector);
        pendleExposed.swap(
            1e18,
            _data(
                PendleEdge.TokenForSy,
                WSTETH_ADDR,
                PENDLE_WSTETH_SY,
                PENDLE_WSTETH_MARKET
            ),
            BOB
        );
    }

    /// The exit direction carries the same check, against `tokenIn` rather than `tokenOut`.
    function testSwapSyForTokenRejectsMismatchedTarget() public {
        vm.expectRevert(PendleExecutor__InvalidTarget.selector);
        pendleExposed.swap(
            1e18,
            _data(
                PendleEdge.SyForToken,
                PENDLE_WSTETH_SY,
                WSTETH_ADDR,
                PENDLE_WSTETH_MARKET
            ),
            BOB
        );
    }

    /// Calldata the encoder would never produce: native ETH, by the router's sentinel or the zero
    /// address an SY may list, fails at the decode with a typed error.
    function testDecodeParamsRejectsNativeEth() public {
        address[2] memory natives =
            [address(0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE), address(0)];
        for (uint256 i = 0; i < natives.length; i++) {
            vm.expectRevert(PendleExecutor__NativeEthUnsupported.selector);
            pendleExposed.decodeParams(
                _data(
                    PendleEdge.TokenForSy,
                    natives[i],
                    PENDLE_WSTETH_SY,
                    PENDLE_WSTETH_SY
                )
            );
            vm.expectRevert(PendleExecutor__NativeEthUnsupported.selector);
            pendleExposed.decodeParams(
                _data(
                    PendleEdge.SyForToken,
                    PENDLE_WSTETH_SY,
                    natives[i],
                    PENDLE_WSTETH_SY
                )
            );
        }
    }

    /// A market edge whose declared pair is not the market's fails before any token moves, in
    /// either direction of the mismatch.
    function testMarketEdgeRejectsAPairTheMarketDoesNotTrade() public {
        _fund(PENDLE_WSTETH_SY, 1e18, PENDLE_ROUTER_V4);

        vm.expectRevert(PendleExecutor__InvalidTarget.selector);
        pendleExposed.swap(
            1e18,
            _data(
                PendleEdge.SyForPt,
                PENDLE_WSTETH_SY,
                PENDLE_WSTETH_YT,
                PENDLE_WSTETH_MARKET
            ),
            BOB
        );

        vm.expectRevert(PendleExecutor__InvalidTarget.selector);
        pendleExposed.swap(
            1e18,
            _data(
                PendleEdge.SyForYt,
                WSTETH_ADDR,
                PENDLE_WSTETH_YT,
                PENDLE_WSTETH_MARKET
            ),
            BOB
        );
    }

    string constant EXECUTED_SWAPS =
        "../../tycho-simulation/src/evm/protocol/pendle/tests/fixtures/executed_swaps.json";

    /// The edge and tokens an edge name from the fixture, such as `sy_for_pt`, stands for.
    function _fixtureEdge(string memory name)
        internal
        view
        returns (PendleEdge edge, address tokenIn, address tokenOut)
    {
        bytes32 key = keccak256(bytes(name));
        address syAddr = PENDLE_WSTETH_SY;
        if (key == keccak256("sy_for_pt")) {
            return (PendleEdge.SyForPt, syAddr, PENDLE_WSTETH_PT);
        }
        if (key == keccak256("pt_for_sy")) {
            return (PendleEdge.PtForSy, PENDLE_WSTETH_PT, syAddr);
        }
        if (key == keccak256("sy_for_yt")) {
            return (PendleEdge.SyForYt, syAddr, PENDLE_WSTETH_YT);
        }
        if (key == keccak256("yt_for_sy")) {
            return (PendleEdge.YtForSy, PENDLE_WSTETH_YT, syAddr);
        }
        revert(string.concat("unknown edge ", name));
    }

    /// Runs one market edge from the unmodified fork state and returns what the receiver got.
    function _executeFromForkState(
        PendleEdge edge,
        address tokenIn,
        address tokenOut,
        uint256 amountIn
    ) internal returns (uint256 amountOut) {
        uint256 snapshot = vm.snapshotState();
        _fund(tokenIn, amountIn, PENDLE_ROUTER_V4);
        uint256 before = IERC20(tokenOut).balanceOf(BOB);
        pendleExposed.swap(
            amountIn, _data(edge, tokenIn, tokenOut, PENDLE_WSTETH_MARKET), BOB
        );
        amountOut = IERC20(tokenOut).balanceOf(BOB) - before;
        vm.revertToState(snapshot);
    }

    /// Every swap the simulation's parity test quotes delivers, here, exactly the amount it
    /// quotes. Both read the same fixture, so a drift on either side fails its own test.
    function testMarketEdgesDeliverWhatTheSimulationQuotes() public {
        string memory json = _executedSwaps();
        uint256 count;
        for (uint256 i = 0;; i++) {
            string memory row = string.concat(".executed[", vm.toString(i), "]");
            if (!vm.keyExistsJson(json, row)) break;
            (PendleEdge edge, address tokenIn, address tokenOut) = _fixtureEdge(
                vm.parseJsonString(json, string.concat(row, ".edge"))
            );
            uint256 amountIn = vm.parseUint(
                vm.parseJsonString(json, string.concat(row, ".amount_in"))
            );
            uint256 expected = vm.parseUint(
                vm.parseJsonString(json, string.concat(row, ".amount_out"))
            );

            assertEq(
                _executeFromForkState(edge, tokenIn, tokenOut, amountIn),
                expected,
                row
            );
            count++;
        }
        assertEq(count, 39);
    }

    /// The fixture's one rejection is a 60 SY → PT swap past the router's search bound: Router
    /// V4's on-chain approximation reverts with its own reason, and the simulation refuses the same
    /// request as `InvalidInput`. Any other revert, such as a wrong pair or a failed approval,
    /// fails the test.
    function testMarketEdgesRejectWhatTheSimulationRejects() public {
        string memory json = _executedSwaps();
        uint256 count;
        for (uint256 i = 0;; i++) {
            string memory row = string.concat(".rejected[", vm.toString(i), "]");
            if (!vm.keyExistsJson(json, row)) break;
            (PendleEdge edge, address tokenIn, address tokenOut) = _fixtureEdge(
                vm.parseJsonString(json, string.concat(row, ".edge"))
            );
            uint256 amountIn = vm.parseUint(
                vm.parseJsonString(json, string.concat(row, ".amount_in"))
            );

            _fund(tokenIn, amountIn, PENDLE_ROUTER_V4);
            vm.expectRevert(bytes("Slippage: search range overflow"));
            pendleExposed.swap(
                amountIn,
                _data(edge, tokenIn, tokenOut, PENDLE_WSTETH_MARKET),
                BOB
            );
            count++;
        }
        assertEq(count, 1);
    }

    /// The fixture, checked to describe the fork and market these tests run against. A fixture
    /// regenerated elsewhere would otherwise fail as an amount mismatch on every row.
    function _executedSwaps() internal view returns (string memory json) {
        json = vm.readFile(EXECUTED_SWAPS);
        assertEq(
            vm.parseJsonUint(json, ".block"), block.number, "fixture block"
        );
        assertEq(
            vm.parseJsonAddress(json, ".market"),
            PENDLE_WSTETH_MARKET,
            "fixture market"
        );
    }

    /// The router is immutable, so a zero address would leave every market edge calling nothing
    /// with no way to correct it.
    function testConstructorRejectsZeroRouter() public {
        vm.expectRevert(PendleExecutor__InvalidRouter.selector);
        new PendleExecutor(address(0));
    }
}

/// Runs the calldata the Rust encoder produced through the deployed router, for every edge.
contract TychoRouterForPendleTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 25800000;
    }

    /// Sends `amountIn` of `tokenIn` through the router with the calldata generated by the
    /// Rust test of the same name, and returns what ALICE received. `spender` is the contract
    /// the edge pulls `tokenIn` through: Router V4 for a market edge, the SY for an SY edge.
    function _runEdge(
        string memory testName,
        address tokenIn,
        address tokenOut,
        address spender,
        uint256 amountIn
    ) internal returns (uint256 received) {
        deal(tokenIn, ALICE, amountIn);
        uint256 balanceBefore = IERC20(tokenOut).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(tokenIn).approve(tychoRouterAddr, amountIn);
        (bool success,) = tychoRouterAddr.call(loadCallDataFromFile(testName));
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        received = IERC20(tokenOut).balanceOf(ALICE) - balanceBefore;

        // The Dispatcher approves each leg's exact amount and revokes what is left, so the
        // router keeps no allowance to the edge's spender.
        assertEq(
            IERC20(tokenIn).allowance(tychoRouterAddr, spender),
            0,
            "allowance left to the spender"
        );
    }

    /// Sends `amountIn` of `tokenIn` along the route the Rust test of the same name encodes, and
    /// returns what ALICE received of `tokenOut`.
    function _runRoute(
        string memory testName,
        address tokenIn,
        address tokenOut,
        uint256 amountIn
    ) internal returns (uint256 received) {
        deal(tokenIn, ALICE, amountIn);
        uint256 balanceBefore = IERC20(tokenOut).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(tokenIn).approve(tychoRouterAddr, amountIn);
        (bool success,) = tychoRouterAddr.call(loadCallDataFromFile(testName));
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        received = IERC20(tokenOut).balanceOf(ALICE) - balanceBefore;
    }

    /// A deposit feeding a market edge: wstETH into the SY, then that SY into PT. Each hop
    /// receives what the single-edge tests deliver, and no allowance is left to the SY or to
    /// Router V4.
    function testRouterTokenSyPt() public {
        uint256 received = _runRoute(
            "test_sequential_encoding_strategy_pendle_token_sy_pt",
            WSTETH_ADDR,
            PENDLE_WSTETH_PT,
            1e18
        );
        assertEq(received, 1276552648088633757);
        assertEq(
            IERC20(WSTETH_ADDR).allowance(tychoRouterAddr, PENDLE_WSTETH_SY), 0
        );
        assertEq(
            IERC20(PENDLE_WSTETH_SY)
                .allowance(tychoRouterAddr, PENDLE_ROUTER_V4),
            0
        );
        assertEq(IERC20(PENDLE_WSTETH_SY).balanceOf(tychoRouterAddr), 0);
    }

    /// A redemption as a later hop: PT into the SY, then the SY the router now holds out to
    /// wstETH. The redemption burns the router's own shares, so nothing approves the SY.
    function testRouterPtSyToken() public {
        uint256 received = _runRoute(
            "test_sequential_encoding_strategy_pendle_pt_sy_token",
            PENDLE_WSTETH_PT,
            WSTETH_ADDR,
            1e18
        );
        assertEq(received, 781959205515422198);
        assertEq(
            IERC20(PENDLE_WSTETH_PT)
                .allowance(tychoRouterAddr, PENDLE_ROUTER_V4),
            0
        );
        assertEq(
            IERC20(PENDLE_WSTETH_SY)
                .allowance(tychoRouterAddr, PENDLE_WSTETH_SY),
            0
        );
        assertEq(IERC20(PENDLE_WSTETH_SY).balanceOf(tychoRouterAddr), 0);
    }

    function testRouterSyForPt() public {
        uint256 received = _runEdge(
            "test_single_encoding_strategy_pendle_sy_for_pt",
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_PT,
            PENDLE_ROUTER_V4,
            1e18
        );
        assertEq(received, 1276552648088633757);
    }

    function testRouterPtForSy() public {
        uint256 received = _runEdge(
            "test_single_encoding_strategy_pendle_pt_for_sy",
            PENDLE_WSTETH_PT,
            PENDLE_WSTETH_SY,
            PENDLE_ROUTER_V4,
            1e18
        );
        assertEq(received, 781959205515422198);
    }

    function testRouterSyForYt() public {
        uint256 received = _runEdge(
            "test_single_encoding_strategy_pendle_sy_for_yt",
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_YT,
            PENDLE_ROUTER_V4,
            1e18
        );
        assertEq(received, 36536075805370490884);
    }

    function testRouterYtForSy() public {
        uint256 received = _runEdge(
            "test_single_encoding_strategy_pendle_yt_for_sy",
            PENDLE_WSTETH_YT,
            PENDLE_WSTETH_SY,
            PENDLE_ROUTER_V4,
            1e18
        );
        assertEq(received, 21770141148458769);
    }

    function testRouterTokenForSy() public {
        uint256 received = _runEdge(
            "test_single_encoding_strategy_pendle_token_for_sy",
            WSTETH_ADDR,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_SY,
            1e18
        );
        assertEq(received, 1000000000000000000);
    }

    /// A redemption burns the router's own shares, so the SY is never approved: no `Approval`
    /// with the router as owner appears in the swap's logs.
    function testRouterSyForToken() public {
        vm.recordLogs();
        uint256 received = _runEdge(
            "test_single_encoding_strategy_pendle_sy_for_token",
            PENDLE_WSTETH_SY,
            WSTETH_ADDR,
            PENDLE_WSTETH_SY,
            1e18
        );
        assertEq(received, 1000000000000000000);

        Vm.Log[] memory logs = vm.getRecordedLogs();
        bytes32 approval = keccak256("Approval(address,address,uint256)");
        for (uint256 i = 0; i < logs.length; i++) {
            bool routerApproves = logs[i].emitter == PENDLE_WSTETH_SY
                && logs[i].topics.length == 3 && logs[i].topics[0] == approval
                && address(uint160(uint256(logs[i].topics[1])))
                    == tychoRouterAddr;
            assertFalse(routerApproves, "the router approved the SY");
        }
    }
}
