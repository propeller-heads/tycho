// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {
    IERC20Metadata
} from "@openzeppelin/contracts/token/ERC20/extensions/IERC20Metadata.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {Constants} from "../Constants.sol";
import {TestUtils} from "../TestUtils.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {IPropAMM} from "@interfaces/IPropAMM.sol";

/// The listing door's base token, the pair's first.
interface IEfficientPropDoor {
    function BASE() external view returns (address);
}

/// @notice Fills an Efficient PropAMM listing door on Ethereum through the
/// PropAMMExecutor deployed there, both directions: the executor pushes the
/// input into the door and calls its `swap` with `block.timestamp` as the
/// deadline, which the door takes. On Ethereum a fill reads the maker's quote
/// only in the block the maker's update landed in (and the next where the
/// listing and the quote carry), and reverts `NoWord` in any other.
///
/// Runs against `EFFICIENT_PROP_ETH_RPC`, an Ethereum node or fork whose
/// latest block carries the update of `EFFICIENT_PROP_ETH_DOOR`'s quote
/// contract, and is skipped without either. A fork test runs in the latest
/// block's number and time, after its transactions, so each fill here is the
/// first of its side behind the update in that block. Each fill pays exactly
/// the door's quote and costs the gas tycho-simulation charges for its class,
/// within a band. The charges are net of the refund a fill earns as the only
/// fill of its transaction, which no call can read: this frame meters the
/// call before refunds and is held to the charge plus that refund
/// (`CUSTODY_FILL_REFUND`, or a routed fill's own in
/// `EFFICIENT_PROP_ETH_SELL_REFUND` and `EFFICIENT_PROP_ETH_BUY_REFUND`).
/// `EFFICIENT_PROP_ETH_BASE_IN` and `EFFICIENT_PROP_ETH_QUOTE_IN`
/// are the amounts sold each way, at or above the listing's least fill;
/// `EFFICIENT_PROP_ETH_PAIR_PLACE` holds the fills to the charge of the
/// listing's place in its pool: 0 its hook pair (the default), 1 past it with
/// the pool's asset (its token 0) as its base, 2 past it with neither. Every
/// listing measured reads a price print. `EFFICIENT_PROP_ETH_SELL_GAS` and
/// `EFFICIENT_PROP_ETH_BUY_GAS`, where set, are the charges the fills are held
/// to in place of the place's: those of a pair tycho-simulation charges
/// apart from its class (`ETHEREUM_PRINT_PAIRS`: wstETH/USDT, 131,620 and
/// 126,449), or of a credit listing, whose caller names its gas
/// (`CreditGas`: the custody figure where the amount fills from the pool's
/// custody, the route's where it borrows or repays), a custody one with
/// `FIRST_BALANCE_WRITE_GAS` (16,964) added where the pool holds none of the
/// token the fill pays into it, as the simulation adds it. A credit
/// listing's route figure is the least over its pool's states, so the state
/// a block holds may spend more: `EFFICIENT_PROP_ETH_SELL_OVER` and
/// `EFFICIENT_PROP_ETH_BUY_OVER` widen the band above the charge for it. With
/// `EFFICIENT_PROP_ETH_NO_UPDATE` true, the latest block carries no update of
/// that quote contract: the door then quotes nothing and every fill reverts
/// `NoWord`.
contract EfficientPropDoorEthereumForkTest is TestUtils, Constants {
    using SafeERC20 for IERC20;

    /// The PropAMMExecutor `executor_addresses.json` lists for Ethereum.
    address constant EXECUTOR = 0xc196f22d69d41A2B1DbF6ef8d28C09bc2CA4E4fa;
    /// `ETHEREUM_SELL_BASE_PRINT_SWAP_GAS` and
    /// `ETHEREUM_BUY_BASE_PRINT_SWAP_GAS` in tycho-simulation's
    /// `efficient_prop/state.rs`: the gas charged on Ethereum for a fill
    /// selling the door's base, and for one buying it, on a listing of its
    /// pool's hook pair reading a print.
    uint256 constant SELL_BASE_PRINT_SWAP_GAS = 105_899;
    uint256 constant BUY_BASE_PRINT_SWAP_GAS = 106_520;
    /// `ETHEREUM_SELL_BASE_PAST_WITH_ASSET_PRINT_SWAP_GAS` and
    /// `ETHEREUM_BUY_BASE_PAST_WITH_ASSET_PRINT_SWAP_GAS` there: the same, on
    /// a listing past its pool's hook pair with the pool's asset as its base.
    /// Measured on a wstETH/USDC listing of the deployed build: run this place
    /// against such a listing's door. A wstETH/USDT listing of the same pool
    /// costs up to about 5,200 more selling base, over the band below.
    uint256 constant SELL_BASE_PAST_WITH_ASSET_PRINT_SWAP_GAS = 126_434;
    uint256 constant BUY_BASE_PAST_WITH_ASSET_PRINT_SWAP_GAS = 125_271;
    /// `ETHEREUM_SELL_BASE_PAST_WITHOUT_ASSET_PRINT_SWAP_GAS` and
    /// `ETHEREUM_BUY_BASE_PAST_WITHOUT_ASSET_PRINT_SWAP_GAS` there: the same,
    /// on a listing past its pool's hook pair with neither token the pool's
    /// asset.
    uint256 constant SELL_BASE_PAST_WITHOUT_ASSET_PRINT_SWAP_GAS = 133_234;
    uint256 constant BUY_BASE_PAST_WITHOUT_ASSET_PRINT_SWAP_GAS = 133_759;
    /// How far a fill's gas may fall under the charge and its refund. Each
    /// charge is the
    /// least first fill seen across runs in real transactions, where
    /// same-size first fills vary by up to about 500 gas from run to run.
    /// This frame meters the same call its own way: its same-size fills have
    /// varied by up to about 750 from run to run, and have come in up to
    /// about 340 under the charge.
    uint256 constant BAND_UNDER = 600;
    /// How far it may rise over the charge: this frame's swing, and other
    /// sizes costing up to about 1,100 more.
    uint256 constant BAND_OVER = 1500;
    /// What a custody fill earns back at its transaction's end as the only
    /// fill there: the router's transfer writes the door's balance of the
    /// input from zero and the fill clears it. Read from traces of real
    /// transactions on the fork, the same on wstETH, WETH, USDC and USDT.
    uint256 constant CUSTODY_FILL_REFUND = 19_900;
    /// `NoWord()`: the front's refusal of a fill in a block without the
    /// quote's update.
    bytes4 constant NO_WORD = bytes4(keccak256("NoWord()"));

    /// A taker no earlier call in a test touched.
    address constant RECEIVER =
        address(uint160(uint256(keccak256("efficient-prop-receiver"))));

    /// The listing door filled, `EFFICIENT_PROP_ETH_DOOR`.
    address door;
    /// The door's base and quote tokens.
    address base;
    address quoteToken;
    /// The amounts sold each way.
    uint256 baseIn;
    uint256 quoteIn;
    /// The door's listing's place in its pool: 0 its hook pair, 1 past it
    /// with the pool's asset, 2 past it without.
    uint256 pairPlace;
    /// Whether the latest block carries no update of the door's quote.
    bool noUpdate;
    /// The charges the fills are held to in place of the place's; zero for
    /// the place's.
    uint256 sellGas;
    uint256 buyGas;
    /// The refund each fill earns, which its charge is net of.
    uint256 sellRefund;
    uint256 buyRefund;
    /// How far each fill may rise over its charge and refund.
    uint256 sellOver;
    uint256 buyOver;

    function setUp() public {
        string memory rpc = vm.envOr("EFFICIENT_PROP_ETH_RPC", string(""));
        door = vm.envOr("EFFICIENT_PROP_ETH_DOOR", address(0));
        if (bytes(rpc).length == 0 || door == address(0)) {
            vm.skip(true);
            return;
        }
        baseIn = vm.envUint("EFFICIENT_PROP_ETH_BASE_IN");
        quoteIn = vm.envUint("EFFICIENT_PROP_ETH_QUOTE_IN");
        pairPlace = vm.envOr("EFFICIENT_PROP_ETH_PAIR_PLACE", uint256(0));
        require(pairPlace <= 2, "EFFICIENT_PROP_ETH_PAIR_PLACE is 0, 1 or 2");
        noUpdate = vm.envOr("EFFICIENT_PROP_ETH_NO_UPDATE", false);
        sellGas = vm.envOr("EFFICIENT_PROP_ETH_SELL_GAS", uint256(0));
        buyGas = vm.envOr("EFFICIENT_PROP_ETH_BUY_GAS", uint256(0));
        sellRefund =
            vm.envOr("EFFICIENT_PROP_ETH_SELL_REFUND", CUSTODY_FILL_REFUND);
        buyRefund =
            vm.envOr("EFFICIENT_PROP_ETH_BUY_REFUND", CUSTODY_FILL_REFUND);
        sellOver = vm.envOr("EFFICIENT_PROP_ETH_SELL_OVER", BAND_OVER);
        buyOver = vm.envOr("EFFICIENT_PROP_ETH_BUY_OVER", BAND_OVER);
        vm.createSelectFork(rpc);
        base = IEfficientPropDoor(door).BASE();
        IPropAMM.TokenPair memory pair = IPropAMM(door).getPairs()[0];
        quoteToken = pair.token0 == base ? pair.token1 : pair.token0;
    }

    function testExecutorIsDeployed() public view {
        assertEq(block.chainid, 1);
        assertGt(EXECUTOR.code.length, 0);
        assertGt(door.code.length, 0);
        // The executor names the door as the address the router funds, and
        // takes the input pushed there.
        (
            uint8 transferType,
            address funded,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = _transferData(abi.encodePacked(door, base, quoteToken));
        // TransferType.Transfer: the router sends its input to the door.
        assertEq(transferType, 0);
        assertEq(funded, door);
        assertEq(tokenIn, base);
        assertEq(tokenOut, quoteToken);
        assertFalse(outputToRouter);
    }

    function testSellBaseMatchesQuote() public {
        _assertSwap(
            base,
            quoteToken,
            baseIn,
            sellGas != 0
                ? sellGas
                : [
                    SELL_BASE_PRINT_SWAP_GAS,
                    SELL_BASE_PAST_WITH_ASSET_PRINT_SWAP_GAS,
                    SELL_BASE_PAST_WITHOUT_ASSET_PRINT_SWAP_GAS
                ][pairPlace],
            sellRefund,
            sellOver
        );
    }

    function testBuyBaseMatchesQuote() public {
        _assertSwap(
            quoteToken,
            base,
            quoteIn,
            buyGas != 0
                ? buyGas
                : [
                    BUY_BASE_PRINT_SWAP_GAS,
                    BUY_BASE_PAST_WITH_ASSET_PRINT_SWAP_GAS,
                    BUY_BASE_PAST_WITHOUT_ASSET_PRINT_SWAP_GAS
                ][pairPlace],
            buyRefund,
            buyOver
        );
    }

    function _transferData(bytes memory data)
        internal
        view
        returns (
            uint8 transferType,
            address funded,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        bytes memory answer = Address.functionStaticCall(
            EXECUTOR, abi.encodeCall(IExecutor.getTransferData, (data))
        );
        (transferType, funded, tokenIn, tokenOut, outputToRouter) =
            abi.decode(answer, (uint8, address, address, address, bool));
    }

    function _assertSwap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 chargedSwapGas,
        uint256 refund,
        uint256 over
    ) internal {
        // The encoder's layout: the door, then the two tokens.
        bytes memory data = abi.encodePacked(door, tokenIn, tokenOut);
        // The router holds the input, and the receiver already holds some of
        // the output, as a returning taker does.
        deal(tokenIn, address(this), amountIn);
        deal(tokenOut, RECEIVER, 10 ** IERC20Metadata(tokenOut).decimals());

        // The fill first, so that it runs on accounts and slots no earlier
        // call in this transaction touched, as a route's first fill does.
        uint256 beforeFill = vm.snapshotState();
        if (noUpdate) {
            try this.hop(data, amountIn) {
                revert("a fill in a block without the update filled");
            } catch (bytes memory reason) {
                assertEq(bytes4(reason), NO_WORD);
            }
            vm.revertToState(beforeFill);
            vm.expectRevert();
            IPropAMM(door).quote(tokenIn, tokenOut, amountIn);
            return;
        }
        (uint256 amountOut, uint256 swapGas) = this.hop(data, amountIn);
        vm.revertToState(beforeFill);
        uint256 quoted = IPropAMM(door).quote(tokenIn, tokenOut, amountIn);

        emit log_named_uint("executor swap gas", swapGas);
        assertGt(quoted, 0);
        assertEq(amountOut, quoted);
        // The charge is what the fill costs its transaction: this frame's
        // gas less the refund the fill earns there.
        assertGe(swapGas + BAND_UNDER, chargedSwapGas + refund);
        assertLe(swapGas, chargedSwapGas + refund + over);
    }

    /// The router's hop around the executor, as `Dispatcher` makes it: the
    /// executor's transfer data, the receiver's output balance, the input sent
    /// to the address the executor names, then a delegatecall of the
    /// executor's `swap`. Only that delegatecall is metered: the gas
    /// tycho-simulation charges for a swap, the router's own transfers being
    /// charged on top of it. A test calls it through `this`, so that it runs
    /// in a fresh frame, as the router's does.
    function hop(bytes calldata data, uint256 amountIn)
        external
        returns (uint256 amountOut, uint256 swapGas)
    {
        (, address funded, address tokenIn, address tokenOut,) =
            _transferData(data);
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
