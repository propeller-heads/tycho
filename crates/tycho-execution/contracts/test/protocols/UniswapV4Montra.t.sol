// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {UniswapV4Executor} from "@src/executors/UniswapV4Executor.sol";
import {TychoRouter__NegativeSlippage} from "@src/TychoRouterV3.sol";
import {Constants} from "../Constants.sol";
import {TestUtils} from "../TestUtils.sol";
import {TychoRouterTestSetup} from "../TychoRouterTestSetup.sol";
import {UniswapV4ExecutorExposed} from "./UniswapV4.t.sol";

/// @dev Montra launch pools on Elysium, protocol system uniswap_v4_hooks: Uniswap V4 pools whose
///      hook is Montra's LaunchHook, swapped through the Elysium UniswapV4Executor with empty
///      hook data. The hook takes its fee through the deltas beforeSwap and afterSwap return, so
///      the amount the executor takes is already net of it. Expected outputs are V4Quoter
///      (0x379F556c5a748BD7Cc9baF0F7A344094Ac1814f5) quoteExactInputSingle and quoteExactInput
///      results at the fork block.
abstract contract MontraFixtures is TestUtils {
    uint256 internal constant ELYSIUM_FORK_BLOCK = 6024400;
    address internal constant MONTRA_HOOK =
        0x6b1216A40B65c83e9c9e18a562C446697B8cf8EC;

    /// @dev Pool 0x850fd929…: native HYPE (currency0) / launch token.
    address internal constant BUY_POOL_TOKEN =
        0x95925E9C25FE28A046bCd43662b826aFaC06b22D;
    uint256 internal constant BUY_AMOUNT_IN = 1 ether;
    uint256 internal constant BUY_AMOUNT_OUT = 22658136496415342153969658;

    /// @dev Pool 0x35bd0e6a…: native HYPE (currency0) / launch token.
    address internal constant NATIVE_SELL_POOL_TOKEN =
        0x32193Afd14c53f12c45A4c04ECA9F7Ba98e3ec71;
    uint256 internal constant NATIVE_SELL_AMOUNT_IN = 1_000_000 ether;
    uint256 internal constant NATIVE_SELL_AMOUNT_OUT = 5092761962617617745;

    /// @dev Pool 0x33f724fd…: wNVDAx (currency0) / launch token.
    address internal constant WNVDAX =
        0x11265018bF2d5a99fb71EFaE73aD670203C1dF44;
    address internal constant ERC20_SELL_POOL_TOKEN =
        0xec8A718F0bC06C25B92082F7bA595931E0D579Dc;
    uint256 internal constant ERC20_SELL_AMOUNT_IN = 1_000_000 ether;
    uint256 internal constant ERC20_SELL_AMOUNT_OUT = 22834129896015656;
    /// @dev A buy on the same pool, paid in wNVDAx.
    uint256 internal constant ERC20_BUY_AMOUNT_IN = 0.01 ether;
    uint256 internal constant ERC20_BUY_AMOUNT_OUT = 428563952001316776957803;

    /// @dev NATIVE_SELL_AMOUNT_IN sold on pool 0x35bd0e6a…, its HYPE bought into on pool
    ///      0x850fd929…, as one grouped swap.
    uint256 internal constant SELL_THEN_BUY_AMOUNT_OUT =
        105599768801641583489401170;
    /// @dev The client's 1% (1_000_000 fee units) of SELL_THEN_BUY_AMOUNT_OUT.
    uint256 internal constant SELL_THEN_BUY_CLIENT_FEE =
        1055997688016415834894011;

    /// @dev The PoolManager and Angstrom arguments the executor is deployed with on Elysium.
    function _elysiumV4ExecutorArgs()
        internal
        view
        returns (address poolManager, address angstrom)
    {
        address[] memory args = executorDeploymentArgs("elysium", "uniswap_v4");
        return (args[0], args[1]);
    }
}

/// @dev The executor on its own, fed the swap data the Rust encoder writes.
contract UniswapV4MontraExecutorTest is Constants, MontraFixtures {
    UniswapV4ExecutorExposed internal _executor;

    function setUp() public {
        vm.createSelectFork(vm.rpcUrl("elysium"), ELYSIUM_FORK_BLOCK);
        (address poolManager, address angstrom) = _elysiumV4ExecutorArgs();
        _executor =
            new UniswapV4ExecutorExposed(IPoolManager(poolManager), angstrom);
    }

    /// @dev Decodes the Rust encoder's calldata `name` and checks it is a single Montra pool
    ///      with the dynamic-fee flag, spacing 200 and no hook data.
    function _assertMontraPool(
        string memory name,
        address tokenIn,
        address tokenOut,
        bool zeroForOne,
        address intermediaryToken
    ) internal view {
        (
            address decodedTokenIn,
            address decodedTokenOut,
            bool decodedZeroForOne,
            bool skipUnlock,
            UniswapV4Executor.UniswapV4Pool[] memory pools
        ) = _executor.decodeData(loadCallDataFromFile(name));

        assertEq(decodedTokenIn, tokenIn);
        assertEq(decodedTokenOut, tokenOut);
        assertEq(decodedZeroForOne, zeroForOne);
        assertFalse(skipUnlock);
        assertEq(pools.length, 1);
        assertEq(pools[0].intermediaryToken, intermediaryToken);
        assertEq(pools[0].fee, 0x800000);
        assertEq(pools[0].tickSpacing, 200);
        assertEq(pools[0].hook, MONTRA_HOOK);
        assertEq(pools[0].hookData, bytes(""));
    }

    /// @dev The native marker becomes address(0).
    function testDecodeMontraNativeQuoteBuy() public view {
        _assertMontraPool(
            "test_encode_montra_native_quote_buy",
            address(0),
            BUY_POOL_TOKEN,
            true,
            BUY_POOL_TOKEN
        );
    }

    /// @dev One-for-zero, with the native marker as address(0) on the output side.
    function testDecodeMontraNativeQuoteSell() public view {
        _assertMontraPool(
            "test_encode_montra_native_quote_sell",
            NATIVE_SELL_POOL_TOKEN,
            address(0),
            false,
            address(0)
        );
    }

    /// @dev One-for-zero on the wNVDAx pool.
    function testDecodeMontraErc20QuoteSell() public view {
        _assertMontraPool(
            "test_encode_montra_erc20_quote_sell",
            ERC20_SELL_POOL_TOKEN,
            WNVDAX,
            false,
            WNVDAX
        );
    }

    // A native-input swap is tested through the router only: the exposed executor's callback
    // pays its input with an ERC-20 transfer, which cannot settle native HYPE.

    /// @dev The hook's fee comes out of the HYPE output, and the executor takes the rest to
    ///      the receiver as native HYPE.
    function testSwapMontraNativeQuoteSell() public {
        deal(NATIVE_SELL_POOL_TOKEN, address(_executor), NATIVE_SELL_AMOUNT_IN);
        uint256 aliceHypeBefore = ALICE.balance;

        _executor.swap(
            NATIVE_SELL_AMOUNT_IN,
            loadCallDataFromFile("test_encode_montra_native_quote_sell"),
            ALICE
        );

        assertEq(ALICE.balance - aliceHypeBefore, NATIVE_SELL_AMOUNT_OUT);
        assertEq(
            IERC20(NATIVE_SELL_POOL_TOKEN).balanceOf(address(_executor)), 0
        );
        assertEq(address(_executor).balance, 0);
    }

    function testSwapMontraErc20QuoteSell() public {
        deal(ERC20_SELL_POOL_TOKEN, address(_executor), ERC20_SELL_AMOUNT_IN);
        uint256 aliceQuoteBefore = IERC20(WNVDAX).balanceOf(ALICE);

        _executor.swap(
            ERC20_SELL_AMOUNT_IN,
            loadCallDataFromFile("test_encode_montra_erc20_quote_sell"),
            ALICE
        );

        assertEq(
            IERC20(WNVDAX).balanceOf(ALICE) - aliceQuoteBefore,
            ERC20_SELL_AMOUNT_OUT
        );
        assertEq(IERC20(ERC20_SELL_POOL_TOKEN).balanceOf(address(_executor)), 0);
        assertEq(IERC20(WNVDAX).balanceOf(address(_executor)), 0);
    }
}

/// @dev TychoRouter swaps through the Elysium executor, driven by calldata from
///      tests/protocol_integration_tests.rs.
contract TychoRouterForUniswapV4MontraTest is
    TychoRouterTestSetup,
    MontraFixtures
{
    function getChain() public pure override returns (string memory) {
        return "elysium";
    }

    function getForkBlock() public pure override returns (uint256) {
        return ELYSIUM_FORK_BLOCK;
    }

    function setUp() public override {
        super.setUp();
        // The shared setup builds usv4Executor on the Ethereum PoolManager, which has no code
        // on Elysium, so the Montra swaps go through an executor on Elysium's PoolManager.
        (address poolManager, address angstrom) = _elysiumV4ExecutorArgs();
        deployExecutorAt(
            "UniswapV4Executor.sol:UniswapV4Executor",
            abi.encode(poolManager, angstrom),
            testExecutorAddress("elysium", "uniswap_v4")
        );
    }

    function testSingleMontraIntegrationNativeQuoteBuy() public {
        deal(ALICE, BUY_AMOUNT_IN);

        vm.prank(ALICE);
        (bool success,) = tychoRouterAddr.call{value: BUY_AMOUNT_IN}(
            loadCallDataFromFile(
                "test_single_encoding_strategy_montra_native_quote_buy"
            )
        );

        assertTrue(success, "Call Failed");
        assertEq(IERC20(BUY_POOL_TOKEN).balanceOf(ALICE), BUY_AMOUNT_OUT);
        assertEq(ALICE.balance, 0);
        assertEq(IERC20(BUY_POOL_TOKEN).balanceOf(tychoRouterAddr), 0);
        assertEq(tychoRouterAddr.balance, 0);
    }

    function testSingleMontraIntegrationNativeQuoteSell() public {
        deal(NATIVE_SELL_POOL_TOKEN, ALICE, NATIVE_SELL_AMOUNT_IN);
        uint256 aliceHypeBefore = ALICE.balance;

        vm.startPrank(ALICE);
        IERC20(NATIVE_SELL_POOL_TOKEN)
            .approve(PERMIT2_ADDRESS, NATIVE_SELL_AMOUNT_IN);
        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile(
                "test_single_encoding_strategy_montra_native_quote_sell"
            )
        );
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(ALICE.balance - aliceHypeBefore, NATIVE_SELL_AMOUNT_OUT);
        assertEq(IERC20(NATIVE_SELL_POOL_TOKEN).balanceOf(ALICE), 0);
        assertEq(IERC20(NATIVE_SELL_POOL_TOKEN).balanceOf(tychoRouterAddr), 0);
        assertEq(tychoRouterAddr.balance, 0);
    }

    function testSingleMontraIntegrationErc20QuoteSell() public {
        deal(ERC20_SELL_POOL_TOKEN, ALICE, ERC20_SELL_AMOUNT_IN);
        uint256 aliceQuoteBefore = IERC20(WNVDAX).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(ERC20_SELL_POOL_TOKEN)
            .approve(PERMIT2_ADDRESS, ERC20_SELL_AMOUNT_IN);
        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile(
                "test_single_encoding_strategy_montra_erc20_quote_sell"
            )
        );
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(
            IERC20(WNVDAX).balanceOf(ALICE) - aliceQuoteBefore,
            ERC20_SELL_AMOUNT_OUT
        );
        assertEq(IERC20(ERC20_SELL_POOL_TOKEN).balanceOf(ALICE), 0);
        assertEq(IERC20(ERC20_SELL_POOL_TOKEN).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(WNVDAX).balanceOf(tychoRouterAddr), 0);
    }

    /// @dev The hook takes its fee out of the wNVDAx input.
    function testSingleMontraIntegrationErc20QuoteBuy() public {
        deal(WNVDAX, ALICE, ERC20_BUY_AMOUNT_IN);

        vm.startPrank(ALICE);
        IERC20(WNVDAX).approve(PERMIT2_ADDRESS, ERC20_BUY_AMOUNT_IN);
        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile(
                "test_single_encoding_strategy_montra_erc20_quote_buy"
            )
        );
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(
            IERC20(ERC20_SELL_POOL_TOKEN).balanceOf(ALICE), ERC20_BUY_AMOUNT_OUT
        );
        assertEq(IERC20(WNVDAX).balanceOf(ALICE), 0);
        assertEq(IERC20(WNVDAX).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(ERC20_SELL_POOL_TOKEN).balanceOf(tychoRouterAddr), 0);
    }

    /// @dev A Montra sell then a Montra buy as one grouped swap, so the buy is a later hop of the
    ///      group. The client takes 1% of the output into the router's vault, and Alice's
    ///      minimum output is exactly what is left, so the slippage check binds.
    function testGroupedMontraIntegrationSellThenBuyWithClientFee() public {
        deal(NATIVE_SELL_POOL_TOKEN, ALICE, NATIVE_SELL_AMOUNT_IN);

        vm.startPrank(ALICE);
        IERC20(NATIVE_SELL_POOL_TOKEN)
            .approve(PERMIT2_ADDRESS, NATIVE_SELL_AMOUNT_IN);
        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile(
                "test_single_encoding_strategy_montra_grouped_sell_then_buy_with_client_fee"
            )
        );
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(
            IERC20(BUY_POOL_TOKEN).balanceOf(ALICE),
            SELL_THEN_BUY_AMOUNT_OUT - SELL_THEN_BUY_CLIENT_FEE
        );
        assertEq(
            tychoRouter.balanceOf(
                clientFeeReceiver, uint256(uint160(BUY_POOL_TOKEN))
            ),
            SELL_THEN_BUY_CLIENT_FEE
        );
        assertEq(IERC20(NATIVE_SELL_POOL_TOKEN).balanceOf(ALICE), 0);
        assertEq(IERC20(NATIVE_SELL_POOL_TOKEN).balanceOf(tychoRouterAddr), 0);
        assertEq(tychoRouterAddr.balance, 0);
    }

    /// @dev The same trade asking for one unit more than it delivers after the client fee.
    function testGroupedMontraIntegrationSellThenBuyRefusesOneUnitMore()
        public
    {
        deal(NATIVE_SELL_POOL_TOKEN, ALICE, NATIVE_SELL_AMOUNT_IN);
        uint256 delivered = SELL_THEN_BUY_AMOUNT_OUT - SELL_THEN_BUY_CLIENT_FEE;

        vm.startPrank(ALICE);
        IERC20(NATIVE_SELL_POOL_TOKEN)
            .approve(PERMIT2_ADDRESS, NATIVE_SELL_AMOUNT_IN);
        vm.expectRevert(
            abi.encodeWithSelector(
                TychoRouter__NegativeSlippage.selector, delivered, delivered + 1
            )
        );
        (bool success,) = tychoRouterAddr.call(
            loadCallDataFromFile(
                "test_single_encoding_strategy_montra_grouped_sell_then_buy_min_out_one_above"
            )
        );
        vm.stopPrank();
        // With an expected revert, the low-level call reports success once the revert matched.
        assertTrue(success, "Call did not revert as expected");
    }
}
