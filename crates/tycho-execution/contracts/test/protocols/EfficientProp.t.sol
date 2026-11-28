// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {Constants} from "../Constants.sol";
import {TestUtils} from "../TestUtils.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {IPropAMM} from "@interfaces/IPropAMM.sol";

/// @notice Fills an Efficient PropAMM listing door on Base through the
/// PropAMMExecutor deployed there, with the calldata `PropAMMSwapEncoder`
/// writes for the door, both directions: each fill pays exactly the door's
/// quote, and costs the gas tycho-simulation charges for it, within a band.
///
/// Runs against `EFFICIENT_PROP_RPC`, a Base node or fork on which
/// `EFFICIENT_PROP_DOOR` is a WETH/USDC listing door quoting a live price, and
/// is skipped without either. A listing's price lives at most 30 s, so the
/// operator's post must be that recent. The door is a listing of its pool's
/// hook pair, the pool's first listing. With `EFFICIENT_PROP_READS_SOURCE`
/// true, it is one whose fill reads a price source, and the fill is held to
/// the gas charged for one; with `EFFICIENT_PROP_GUARDED` true as well, its
/// print comes from a guarded source, and the fill is held to that class's
/// charge, which is for a fill while the lane holds no replaced print: run it
/// at least the lane's hold after the last post. The calldata names a
/// placeholder door, which the test replaces with the one it is given.
///
/// The charge is the least of a side's first fills in a block, measured on
/// the front's counted clock (live Base's): a fill in a block already holding
/// one of its side costs up to a few hundred less, a larger fill walking more
/// depth rungs about 660 a rung more, and a fork whose block count left the
/// chain's clock about 10,500 more for its call to BaseTime, outside the band.
contract EfficientPropDoorBaseForkTest is TestUtils, Constants {
    using SafeERC20 for IERC20;

    /// The PropAMMExecutor `executor_addresses.json` lists for Base.
    address constant EXECUTOR = 0xF30f4c985fCA63913f421F78058031FA6514eF7f;
    /// The door the Rust encoding test writes the calldata for, a placeholder.
    address constant PLACEHOLDER_DOOR =
        address(uint160(uint256(keccak256("efficient-prop-door"))));
    /// `SELL_BASE_SWAP_GAS` and `BUY_BASE_SWAP_GAS` in tycho-simulation's
    /// `efficient_prop/state.rs`: the gas charged for a fill selling WETH, the
    /// door's base, and for one buying it.
    uint256 constant SELL_BASE_SWAP_GAS = 118_157;
    uint256 constant BUY_BASE_SWAP_GAS = 121_988;
    /// `SELL_BASE_PRINT_SWAP_GAS` and `BUY_BASE_PRINT_SWAP_GAS` there: the
    /// same, on a listing whose fill reads a price source.
    uint256 constant SELL_BASE_PRINT_SWAP_GAS = 130_482;
    uint256 constant BUY_BASE_PRINT_SWAP_GAS = 134_794;
    /// `SELL_BASE_GUARDED_SWAP_GAS` and `BUY_BASE_GUARDED_SWAP_GAS` there:
    /// the same, on a listing reading a print from a guarded source.
    uint256 constant SELL_BASE_GUARDED_SWAP_GAS = 136_461;
    uint256 constant BUY_BASE_GUARDED_SWAP_GAS = 140_520;
    /// How far a fill's gas may fall under the charge, and rise over it.
    uint256 constant BAND_UNDER = 600;
    uint256 constant BAND_OVER = 1500;

    /// A taker no earlier call in a test touched.
    address constant RECEIVER =
        address(uint160(uint256(keccak256("efficient-prop-receiver"))));

    /// The listing door filled, `EFFICIENT_PROP_DOOR`.
    address door;
    /// Whether its fill reads a price source, `EFFICIENT_PROP_READS_SOURCE`.
    bool readsSource;
    /// Whether that source is guarded, `EFFICIENT_PROP_GUARDED`.
    bool guarded;

    function setUp() public {
        string memory rpc = vm.envOr("EFFICIENT_PROP_RPC", string(""));
        door = vm.envOr("EFFICIENT_PROP_DOOR", address(0));
        readsSource = vm.envOr("EFFICIENT_PROP_READS_SOURCE", false);
        guarded = vm.envOr("EFFICIENT_PROP_GUARDED", false);
        if (bytes(rpc).length == 0 || door == address(0)) {
            vm.skip(true);
            return;
        }
        vm.createSelectFork(rpc);
    }

    function testExecutorIsDeployed() public view {
        assertGt(EXECUTOR.code.length, 0);
        assertGt(door.code.length, 0);
    }

    function testSwapWethToUsdcMatchesQuote() public {
        _assertSwapMatchesQuote(
            "test_encode_efficient_prop_weth_usdc",
            BASE_WETH,
            BASE_USDC,
            1 ether,
            _charged(
                SELL_BASE_SWAP_GAS,
                SELL_BASE_PRINT_SWAP_GAS,
                SELL_BASE_GUARDED_SWAP_GAS
            )
        );
    }

    function testSwapUsdcToWethMatchesQuote() public {
        _assertSwapMatchesQuote(
            "test_encode_efficient_prop_usdc_weth",
            BASE_USDC,
            BASE_WETH,
            2000e6,
            _charged(
                BUY_BASE_SWAP_GAS,
                BUY_BASE_PRINT_SWAP_GAS,
                BUY_BASE_GUARDED_SWAP_GAS
            )
        );
    }

    /// The charge of the door's class: `word` reading no source, `print` a
    /// source that is not guarded, `guardedPrint` a guarded one.
    function _charged(uint256 word, uint256 print, uint256 guardedPrint)
        internal
        view
        returns (uint256)
    {
        if (!readsSource) return word;
        return guarded ? guardedPrint : print;
    }

    function _assertSwapMatchesQuote(
        string memory encoding,
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 chargedSwapGas
    ) internal {
        // The encoder's layout, the door then the two tokens, for the door
        // given in place of the placeholder.
        assertEq(
            loadCallDataFromFile(encoding),
            abi.encodePacked(PLACEHOLDER_DOOR, tokenIn, tokenOut)
        );
        bytes memory data = abi.encodePacked(door, tokenIn, tokenOut);
        // The router holds the input, and the receiver already holds some of
        // the output, as a returning taker does.
        deal(tokenIn, address(this), amountIn);
        deal(tokenOut, RECEIVER, 1);

        // The fill first, so that it runs on accounts and slots no earlier
        // call in this transaction touched, as a route's first fill does.
        uint256 beforeFill = vm.snapshotState();
        (uint256 amountOut, uint256 swapGas) = this.hop(data, amountIn);
        vm.revertToState(beforeFill);
        uint256 quoted = IPropAMM(door).quote(tokenIn, tokenOut, amountIn);

        emit log_named_uint("executor swap gas", swapGas);
        assertGt(quoted, 0);
        assertEq(amountOut, quoted);
        assertGe(swapGas + BAND_UNDER, chargedSwapGas);
        assertLe(swapGas, chargedSwapGas + BAND_OVER);
    }

    /// The router's hop around the executor, as `Dispatcher` makes it: the
    /// executor's transfer data, the receiver's output balance, the input sent
    /// to the address the executor names, then a delegatecall of the
    /// executor's `swap`. Only that delegatecall is metered: the gas
    /// tycho-simulation charges for a swap, the router's own transfers being
    /// charged on top of it. A test calls it through `this`, so that it runs
    /// in a fresh frame, as the router's does, and not on the memory the test
    /// spent loading the calldata.
    function hop(bytes calldata data, uint256 amountIn)
        external
        returns (uint256 amountOut, uint256 swapGas)
    {
        (, address funded, address tokenIn, address tokenOut,) =
            IExecutor(EXECUTOR).getTransferData(data);
        uint256 balanceBefore = IERC20(tokenOut).balanceOf(RECEIVER);
        IERC20(tokenIn).safeTransfer(funded, amountIn);

        uint256 gasBefore = gasleft();
        // forge-lint: disable-next-item(low-level-calls) Solidity has no high-level delegatecall.
        (bool success, bytes memory result) = EXECUTOR.delegatecall(
            abi.encodeCall(IExecutor.swap, (amountIn, data, RECEIVER))
        );
        swapGas = gasBefore - gasleft();
        Address.verifyCallResult(success, result);
        amountOut = IERC20(tokenOut).balanceOf(RECEIVER) - balanceBefore;
    }
}
