pragma solidity ^0.8.26;

import "../TychoRouterTestSetup.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {MockPropAMM} from "./PropAMM.t.sol";
import {AerodromeV1TestBase} from "./AerodromeV1.t.sol";
import {IAerodromeV1Pool} from "@interfaces/IAerodromeV1Pool.sol";
import {TransferManager} from "../../src/TransferManager.sol";
import {
    FallbackExecutor,
    FallbackExecutor__AddressZero,
    FallbackExecutor__InvalidDataLength
} from "../../src/executors/FallbackExecutor.sol";
import {
    TychoFallbackRouter,
    TychoFallbackRouter__CallbackTokenMismatch,
    TychoFallbackRouter__InvalidSwapLength,
    TychoFallbackRouter__InvalidCallback,
    TychoFallbackRouter__InvalidUniswapV2Fee,
    TychoFallbackRouter__NotPoolManager,
    TychoFallbackRouter__ProtocolUnavailable,
    TychoFallbackRouter__UnknownProtocol,
    TychoFallbackRouter__NotSelf
} from "../../src/fallback/TychoFallbackRouter.sol";
import {UniswapV2Math__ZeroReserves} from "../../lib/UniswapV2Math.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";

/// @notice Builds the protocol entries `TychoFallbackRouter` decodes.
library FallbackSwaps {
    function swap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address receiver
    ) internal pure returns (TychoFallbackRouter.Swap memory) {
        return TychoFallbackRouter.Swap({
            tokenIn: tokenIn,
            tokenOut: tokenOut,
            amountIn: amountIn,
            receiver: receiver
        });
    }

    function uniswapV2(address pair, uint8 feeBps)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(
            uint8(TychoFallbackRouter.FallbackProtocol.UniswapV2), pair, feeBps
        );
    }

    function uniswapV3(address pool) internal pure returns (bytes memory) {
        return abi.encodePacked(
            uint8(TychoFallbackRouter.FallbackProtocol.UniswapV3), pool
        );
    }

    function uniswapV4(
        uint24 fee,
        int24 tickSpacing,
        address hook,
        bytes memory hookData
    ) internal pure returns (bytes memory) {
        return abi.encodePacked(
            uint8(TychoFallbackRouter.FallbackProtocol.UniswapV4),
            bytes3(fee),
            tickSpacing,
            hook,
            hookData
        );
    }

    function curve(address pool, uint8 poolType, uint8 i, uint8 j)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(
            uint8(TychoFallbackRouter.FallbackProtocol.Curve),
            pool,
            poolType,
            i,
            j
        );
    }

    function fluidV1(address dex, bool zero2one)
        internal
        pure
        returns (bytes memory)
    {
        return abi.encodePacked(
            uint8(TychoFallbackRouter.FallbackProtocol.FluidV1), dex, zero2one
        );
    }

    function aerodromeV1(address pool) internal pure returns (bytes memory) {
        return abi.encodePacked(
            uint8(TychoFallbackRouter.FallbackProtocol.AerodromeV1), pool
        );
    }
}

error RevertingPool__Nope();

/// @notice Fluid's own error, raised with an internal error id.
error FluidDexError(uint256 errorId);

/// @notice A V2-shaped pair with nothing in it, for the zero-reserve guard.
contract EmptyReservePair {
    function getReserves()
        external
        pure
        returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast)
    {
        return (0, 0, 0);
    }
}

/// @notice A V3-shaped "pool" that reports success without paying, so the swap delivers nothing
/// and the route-level `minAmountOut` must be what catches it.
contract SilentPool {
    function swap(
        address, /* recipient */
        bool, /* zeroForOne */
        int256, /* amountSpecified */
        uint160, /* sqrtPriceLimitX96 */
        bytes calldata /* data */
    )
        external
        pure
        returns (int256 amount0, int256 amount1)
    {
        return (0, 0);
    }
}

/// @notice A "pool" that always reverts with its own error, so a test can assert the fallback
/// slot's failure escapes `swap` unchanged.
contract RevertingPool {
    fallback() external {
        revert RevertingPool__Nope();
    }
}

interface IPancakeV3SwapCallback {
    function pancakeV3SwapCallback(int256, int256, bytes calldata) external;
}

/// @notice A V3-shaped pool that asks for its input through Pancake's renamed
/// `pancakeV3SwapCallback` rather than `uniswapV3SwapCallback`. Proves the
/// catch-all `fallback` pays a V3 fork whatever callback name the pool picks.
contract RenamedCallbackPool {
    address immutable tokenOut;
    uint256 immutable amountOut;

    constructor(address tokenOut_, uint256 amountOut_) {
        tokenOut = tokenOut_;
        amountOut = amountOut_;
    }

    function swap(address recipient, bool, int256, uint160, bytes calldata)
        external
        returns (int256, int256)
    {
        // A real V3 pool pays the recipient, then pulls its input in the callback.
        IERC20(tokenOut).transfer(recipient, amountOut);
        IPancakeV3SwapCallback(msg.sender).pancakeV3SwapCallback(0, 0, "");
        return (0, 0);
    }
}

/// @notice Accepts `tokenIn` and reports success without paying anything. Quotes the maximum so
/// the router still tries it.
contract SilentPropAMM {
    function quote(
        address, /* tokenIn */
        address, /* tokenOut */
        uint256 /* amountIn */
    )
        external
        pure
        returns (uint256 amountOut)
    {
        return type(uint256).max;
    }

    function swap(
        address, /* tokenIn */
        address, /* tokenOut */
        uint256, /* amountIn */
        uint256, /* minAmountOut */
        address, /* recipient */
        uint256 /* deadline */
    )
        external
        pure
        returns (uint256 amountOut)
    {
        return 0;
    }
}

error UnquotablePropAMM__NoQuote();

/// @notice Reverts on `quote` but pays on `swap`, so a router that attempts a pAMM it could not
/// quote leaves a balance behind to prove it.
contract UnquotablePropAMM {
    uint256 public constant AMOUNT_OUT = 4 ether;

    function quote(
        address, /* tokenIn */
        address, /* tokenOut */
        uint256 /* amountIn */
    )
        external
        pure
        returns (uint256 amountOut)
    {
        revert UnquotablePropAMM__NoQuote();
    }

    function swap(
        address, /* tokenIn */
        address tokenOut,
        uint256, /* amountIn */
        uint256, /* minAmountOut */
        address recipient,
        uint256 /* deadline */
    ) external returns (uint256 amountOut) {
        IERC20(tokenOut).transfer(recipient, AMOUNT_OUT);
        return AMOUNT_OUT;
    }
}

/// @notice Deploys a `TychoFallbackRouter` on a fork and holds the assertions every fallback
/// test repeats. Subclasses name the fork block, since the protocols are not all live at the
/// same one.
abstract contract TychoFallbackRouterTestBase is Constants, TestUtils {
    TychoFallbackRouter router;
    MockPropAMM pamm;

    function getForkBlock() internal pure virtual returns (uint256);

    function setUp() public virtual {
        vm.createSelectFork(vm.rpcUrl("mainnet"), getForkBlock());
        router = new TychoFallbackRouter(
            IPoolManager(POOL_MANAGER),
            FLUIDV1_LIQUIDITY,
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER)
        );
        pamm = new MockPropAMM();
    }

    /// `quoteFallback` is self-only, so this calls it as the router.
    function _quoteFallback(
        TychoFallbackRouter.Swap memory swap_,
        bytes memory fallbackSwap
    ) internal returns (uint256 amountOut) {
        vm.prank(address(router));
        return router.quoteFallback(swap_, fallbackSwap);
    }

    /// Requires `swap` to emit `FallbackSwap` for `protocol` and `reason` on the next call.
    function _expectFallbackSwap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        TychoFallbackRouter.FallbackProtocol protocol,
        TychoFallbackRouter.FallbackReason reason
    ) internal {
        vm.expectEmit(address(router));
        emit TychoFallbackRouter.FallbackSwap(
            address(pamm), tokenIn, tokenOut, amountIn, protocol, reason
        );
    }

    /// Holds no funds once a swap is done.
    function _assertRouterDrained(address tokenIn, address tokenOut)
        internal
        view
    {
        assertEq(IERC20(tokenIn).balanceOf(address(router)), 0);
        assertEq(IERC20(tokenOut).balanceOf(address(router)), 0);
    }
}

/// @notice The claim the contract exists for: a reverting pAMM still delivers `tokenOut`,
/// because the input is still here to fund the retry -- which is what an executor cannot do,
/// since the Dispatcher has already paid the pAMM by the time it reverts.
contract TychoFallbackRouterTest is TychoFallbackRouterTestBase {
    /// The USDC/WETH, DAI/USDC and USDE/USDT pools this contract quotes all
    /// hold enough liquidity to fill `USDC_IN` here. Moving the block moves
    /// every expected output with it.
    uint256 constant FORK_BLOCK = 22_689_128;

    uint256 constant USDC_IN = 10_000e6;

    /// Measured at FORK_BLOCK against the pools each test names. An exact
    /// amount is what separates a correct fill from one the protocol still
    /// accepted at the wrong fee, direction or scale.
    uint256 constant V2_WETH_OUT = 3_611_787_219_421_119_156;
    uint256 constant V2_USDC_OUT = 10_994_711_547;
    uint256 constant V3_WETH_OUT = 3_611_998_638_539_827_447;
    uint256 constant V3_USDC_OUT = 11_062_418_692;
    uint256 constant V4_USDT_OUT = 99_970_662;
    uint256 constant V4_USDE_OUT = 100_009_300_940_809_442_564;
    uint256 constant CURVE_USDC_OUT = 999_895_324;
    uint256 constant CURVE_CRYPTO_USDC_OUT = 2_766_051_040;

    /// PancakeSwap V3 renames the V3 callback to `pancakeV3SwapCallback`, so
    /// this exercises the catch-all `fallback` against a real fork's pool. The
    /// 0.05% USDC/WETH pool from the mainnet PancakeV3 factory.
    address constant PANCAKE_USDC_WETH_V3 =
        0x1ac1A8FEaAEa1900C4166dEeed0C11cC10669D36;
    uint256 constant PANCAKE_V3_WETH_OUT = 3_601_880_052_618_891_035;

    /// A MockPropAMM price of 5 WETH per 10 000 USDC: above every pool here, so a pAMM at this
    /// price wins the quote.
    uint256 constant PAMM_PRICE_ABOVE_MARKET = 5e26;

    function getForkBlock() internal pure override returns (uint256) {
        return FORK_BLOCK;
    }

    /// The enum ordinals are the protocol byte the encoder emits (the protocol
    /// table in CLAUDE.md); reordering the enum must fail here, not silently.
    function testProtocolByteIsStable() public pure {
        assertEq(uint8(TychoFallbackRouter.FallbackProtocol.UniswapV2), 0);
        assertEq(uint8(TychoFallbackRouter.FallbackProtocol.UniswapV3), 1);
        assertEq(uint8(TychoFallbackRouter.FallbackProtocol.UniswapV4), 2);
        assertEq(uint8(TychoFallbackRouter.FallbackProtocol.Curve), 3);
        assertEq(uint8(TychoFallbackRouter.FallbackProtocol.FluidV1), 4);
        assertEq(uint8(TychoFallbackRouter.FallbackProtocol.AerodromeV1), 5);
    }

    /// 30 bps is the highest accepted fee; 31 reverts naming the value. The
    /// bound is the only guard between a caller-supplied fee and the pricing
    /// math.
    function testUniswapV2FeeBoundary() public {
        deal(USDC_ADDR, address(router), USDC_IN);

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoFallbackRouter__InvalidUniswapV2Fee.selector, uint256(31)
            )
        );
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV2(USDC_WETH_USV2, 31)
        );

        // The boundary itself is accepted and fills.
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV2(USDC_WETH_USV2, 30)
        );
        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V2_WETH_OUT);
    }

    /// A pair with no reserves cannot price the trade.
    function testUniswapV2ZeroReservesReverts() public {
        EmptyReservePair pair = new EmptyReservePair();
        deal(USDC_ADDR, address(router), USDC_IN);

        vm.expectRevert(UniswapV2Math__ZeroReserves.selector);
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV2(address(pair), 30)
        );
    }

    /// Every protocol pins its payload width: truncated and over-long payloads
    /// revert `InvalidSwapLength` naming the offending length. Uniswap V4 is a
    /// lower bound (variable hookData), so only truncation applies to it.
    function testProtocolDataLengthGuards() public {
        deal(USDC_ADDR, address(router), USDC_IN);

        bytes[] memory entries = new bytes[](6);
        entries[0] = FallbackSwaps.uniswapV2(USDC_WETH_USV2, 30);
        entries[1] = FallbackSwaps.uniswapV3(USDC_WETH_USV3);
        entries[2] = FallbackSwaps.uniswapV4(100, 1, address(0), bytes(""));
        entries[3] = FallbackSwaps.curve(TRIPOOL, 1, 0, 1);
        entries[4] = FallbackSwaps.fluidV1(FLUIDV1_LIQUIDITY, true);
        entries[5] = FallbackSwaps.aerodromeV1(USDC_WETH_USV2);

        TychoFallbackRouter.Swap memory swap_ =
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB);

        for (uint256 i = 0; i < entries.length; i++) {
            // The first byte is the protocol tag, so the guarded width is one
            // less.
            uint256 width = entries[i].length - 1;

            vm.expectRevert(
                abi.encodeWithSelector(
                    TychoFallbackRouter__InvalidSwapLength.selector, width - 1
                )
            );
            router.swap(swap_, address(pamm), _truncate(entries[i]));

            bool isUniswapV4 =
                i == uint256(TychoFallbackRouter.FallbackProtocol.UniswapV4);
            if (!isUniswapV4) {
                vm.expectRevert(
                    abi.encodeWithSelector(
                        TychoFallbackRouter__InvalidSwapLength.selector,
                        width + 1
                    )
                );
                router.swap(
                    swap_, address(pamm), bytes.concat(entries[i], hex"00")
                );
            }
        }
    }

    function _truncate(bytes memory data)
        internal
        pure
        returns (bytes memory out)
    {
        out = new bytes(data.length - 1);
        for (uint256 i = 0; i < out.length; i++) {
            out[i] = data[i];
        }
    }

    /// A live pAMM that quotes above the fallback fills, and the fallback is never touched.
    function testPropAMMFills() public {
        // 5 WETH for the whole 10 000 USDC, above the Uniswap V3 price of roughly 3.6 WETH, so
        // the asserted amount can only have come from the pAMM.
        pamm.setPrice(USDC_ADDR, WETH_ADDR, PAMM_PRICE_ABOVE_MARKET);
        deal(WETH_ADDR, address(pamm), 100 ether);
        deal(USDC_ADDR, address(router), USDC_IN);

        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), 5 ether);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pamm)), USDC_IN);
        _assertRouterDrained(USDC_ADDR, WETH_ADDR);
    }

    /// A live pAMM that quotes below the fallback is skipped: the fallback fills at its own
    /// price and the pAMM is never paid.
    function testFallbackQuotedHigherSkipsPropAMM() public {
        // 1 WETH for 10 000 USDC, below the Uniswap V3 price of roughly 3.6 WETH.
        pamm.setPrice(USDC_ADDR, WETH_ADDR, 1e26);
        deal(WETH_ADDR, address(pamm), 100 ether);
        deal(USDC_ADDR, address(router), USDC_IN);

        _expectFallbackSwap(
            USDC_ADDR,
            WETH_ADDR,
            USDC_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pamm)), 0);
        _assertRouterDrained(USDC_ADDR, WETH_ADDR);
    }

    /// Equal quotes keep the pAMM: only a strictly higher fallback quote displaces it.
    function testEqualQuotesKeepPropAMM() public {
        // MockPropAMM pays amountIn * price / 1e18, so this price quotes exactly V3_WETH_OUT for
        // USDC_IN.
        pamm.setPrice(USDC_ADDR, WETH_ADDR, V3_WETH_OUT * 1e8);
        deal(WETH_ADDR, address(pamm), 100 ether);
        deal(USDC_ADDR, address(router), USDC_IN);

        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pamm)), USDC_IN);
    }

    /// A pAMM that wins the quote but reverts on the swap still falls through, and the event
    /// says so.
    function testPropAMMRevertAfterWinningQuoteFallsThrough() public {
        // Quotes 5 WETH but holds none, so `swap` reverts.
        pamm.setPrice(USDC_ADDR, WETH_ADDR, PAMM_PRICE_ABOVE_MARKET);
        deal(USDC_ADDR, address(router), USDC_IN);

        _expectFallbackSwap(
            USDC_ADDR,
            WETH_ADDR,
            USDC_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.PropAMMFailed
        );
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
        // The transfer to the pAMM reverted with it.
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pamm)), 0);
        _assertRouterDrained(USDC_ADDR, WETH_ADDR);
    }

    /// A pAMM address without code returns nothing to decode, which is a zero quote rather
    /// than a revert of the swap.
    /// A pAMM that cannot quote is skipped outright, so the fallback runs even when it cannot
    /// quote either and its revert is the swap's revert.
    function testUnquotablePropAMMWithUnquotableFallback() public {
        UnquotablePropAMM stalePamm = new UnquotablePropAMM();
        bytes memory fallbackSwap =
            FallbackSwaps.uniswapV3(address(new RevertingPool()));
        deal(WETH_ADDR, address(stalePamm), 100 ether);
        deal(USDC_ADDR, address(router), USDC_IN);

        vm.expectRevert(RevertingPool__Nope.selector);
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(stalePamm),
            fallbackSwap
        );
    }

    function testPropAMMWithoutCodeFallsBack() public {
        address noCode = makeAddr("no code");
        deal(USDC_ADDR, address(router), USDC_IN);

        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            noCode,
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
    }

    /// Every fallback quote is the amount the same fallback then fills, so the comparison
    /// against the pAMM is made on real numbers. Uniswap V4 is simulated, which needs the input
    /// to be here as it is in `swap`.
    function testFallbackQuotesMatchFills() public {
        deal(USDE_ADDR, address(router), 100 ether);

        assertEq(
            _quoteFallback(
                FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
                FallbackSwaps.uniswapV2(USDC_WETH_USV2, 30)
            ),
            V2_WETH_OUT
        );
        assertEq(
            _quoteFallback(
                FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
                FallbackSwaps.uniswapV3(USDC_WETH_USV3)
            ),
            V3_WETH_OUT
        );
        assertEq(
            _quoteFallback(
                FallbackSwaps.swap(USDE_ADDR, USDT_ADDR, 100 ether, BOB),
                FallbackSwaps.uniswapV4(100, 1, address(0), bytes(""))
            ),
            V4_USDT_OUT
        );
        assertEq(
            _quoteFallback(
                FallbackSwaps.swap(DAI_ADDR, USDC_ADDR, 1000e18, BOB),
                FallbackSwaps.curve(TRIPOOL, 1, 0, 1)
            ),
            CURVE_USDC_OUT
        );
        assertEq(
            _quoteFallback(
                FallbackSwaps.swap(WETH_ADDR, USDC_ADDR, 1 ether, BOB),
                FallbackSwaps.curve(TRICRYPTO_POOL, 3, 2, 0)
            ),
            CURVE_CRYPTO_USDC_OUT
        );

        // The simulation rolled back: the input is untouched and nothing was delivered.
        assertEq(IERC20(USDE_ADDR).balanceOf(address(router)), 100 ether);
        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), 0);
        assertEq(IERC20(USDT_ADDR).balanceOf(BOB), 0);
    }

    /// `quoteFallback` reverts with the cause -- the pool's own error, the zero-reserve guard,
    /// a "pool" the static quoter cannot read, the protocol data decoder's error -- and `swap`
    /// counts each as a zero quote.
    function testQuoteFallbackRevertsWithTheCause() public {
        TychoFallbackRouter.Swap memory swap_ =
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB);
        address reverting = address(new RevertingPool());
        address emptyPair = address(new EmptyReservePair());
        address silent = address(new SilentPool());

        vm.expectRevert(RevertingPool__Nope.selector);
        _quoteFallback(swap_, FallbackSwaps.uniswapV3(reverting));

        vm.expectRevert(UniswapV2Math__ZeroReserves.selector);
        _quoteFallback(swap_, FallbackSwaps.uniswapV2(emptyPair, 30));

        vm.expectRevert();
        _quoteFallback(swap_, FallbackSwaps.uniswapV3(silent));

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoFallbackRouter__UnknownProtocol.selector, uint8(9)
            )
        );
        _quoteFallback(swap_, abi.encodePacked(uint8(9), USDC_WETH_USV3));

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoFallbackRouter__InvalidSwapLength.selector, uint256(0)
            )
        );
        _quoteFallback(swap_, bytes(""));

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoFallbackRouter__InvalidSwapLength.selector, uint256(19)
            )
        );
        _quoteFallback(
            swap_, _truncate(FallbackSwaps.uniswapV3(USDC_WETH_USV3))
        );
    }

    /// A fallback that cannot quote is a zero quote, never a revert of the swap: the pAMM
    /// still runs first and fills.
    function testUnquotableFallbackKeepsPropAMMFirst() public {
        pamm.setPrice(USDC_ADDR, WETH_ADDR, PAMM_PRICE_ABOVE_MARKET);
        deal(WETH_ADDR, address(pamm), 100 ether);
        deal(USDC_ADDR, address(router), USDC_IN);

        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV3(address(new RevertingPool()))
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), 5 ether);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pamm)), USDC_IN);
    }

    /// The quote entry points are external only so `swap` can try/catch them.
    function testQuoteEntryPointsRejectExternalCaller() public {
        TychoFallbackRouter.Swap memory swap_ =
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB);

        vm.expectRevert(TychoFallbackRouter__NotSelf.selector);
        router.quoteFallback(swap_, FallbackSwaps.uniswapV3(USDC_WETH_USV3));

        // The caller check comes before the decode, so the protocol data is irrelevant here.
        vm.expectRevert(TychoFallbackRouter__NotSelf.selector);
        router.simulateFallback(swap_, "");
    }

    /// A pAMM that fills emits nothing, so counting `FallbackSwap` counts misses.
    function testPropAMMFillEmitsNoFallbackSwap() public {
        pamm.setPrice(USDC_ADDR, WETH_ADDR, PAMM_PRICE_ABOVE_MARKET);
        deal(WETH_ADDR, address(pamm), 100 ether);
        deal(USDC_ADDR, address(router), USDC_IN);

        vm.recordLogs();
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );

        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; i++) {
            assertTrue(
                logs[i].topics[0] != TychoFallbackRouter.FallbackSwap.selector
            );
        }
    }

    /// The pAMM has no price, so `quote` reverts and counts as quoting zero. Uniswap V3 pays
    /// inside its callback, reachable only because this contract still holds the USDC.
    /// `FallbackSwap` is the pAMM fill-rate signal: it marks the swaps the pAMM did not serve,
    /// names the protocol that filled instead, and says why.
    function testFallsBackToUniswapV3() public {
        deal(USDC_ADDR, address(router), USDC_IN);

        _expectFallbackSwap(
            USDC_ADDR,
            WETH_ADDR,
            USDC_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
        // The transfer to the pAMM reverted with it.
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pamm)), 0);
        _assertRouterDrained(USDC_ADDR, WETH_ADDR);
    }

    /// A real PancakeSwap V3 pool fills through byte 1: its pool calls
    /// `pancakeV3SwapCallback`, which lands on the catch-all `fallback` since it
    /// is not one of the router's named callbacks.
    function testFallsBackToPancakeV3Fork() public {
        deal(USDC_ADDR, address(router), USDC_IN);

        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV3(PANCAKE_USDC_WETH_V3)
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), PANCAKE_V3_WETH_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pamm)), 0);
        _assertRouterDrained(USDC_ADDR, WETH_ADDR);
    }

    /// WETH < USDC is false, so this runs the `!zeroForOne` sqrt limit.
    function testFallsBackToUniswapV3Reverse() public {
        uint256 amountIn = 4 ether;
        deal(WETH_ADDR, address(router), amountIn);

        router.swap(
            FallbackSwaps.swap(WETH_ADDR, USDC_ADDR, amountIn, BOB),
            address(pamm),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );

        assertEq(IERC20(USDC_ADDR).balanceOf(BOB), V3_USDC_OUT);
        _assertRouterDrained(WETH_ADDR, USDC_ADDR);
    }

    /// The fallback starts from the full `amountIn`: the pAMM's transfer reverted with it.
    function testFallsBackToUniswapV2() public {
        deal(USDC_ADDR, address(router), USDC_IN);

        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV2(USDC_WETH_USV2, 30)
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V2_WETH_OUT);
        _assertRouterDrained(USDC_ADDR, WETH_ADDR);
    }

    /// Reverse direction: the `!zeroForOne` reserve pairing and `pair.swap`
    /// argument order.
    function testFallsBackToUniswapV2Reverse() public {
        uint256 amountIn = 4 ether;
        deal(WETH_ADDR, address(router), amountIn);

        router.swap(
            FallbackSwaps.swap(WETH_ADDR, USDC_ADDR, amountIn, BOB),
            address(pamm),
            FallbackSwaps.uniswapV2(USDC_WETH_USV2, 30)
        );

        assertEq(IERC20(USDC_ADDR).balanceOf(BOB), V2_USDC_OUT);
        _assertRouterDrained(WETH_ADDR, USDC_ADDR);
    }

    /// Curve pays the caller, so the swap forwards the output itself.
    function testFallsBackToCurve() public {
        uint256 amountIn = 1000e18;
        deal(DAI_ADDR, address(router), amountIn);

        router.swap(
            FallbackSwaps.swap(DAI_ADDR, USDC_ADDR, amountIn, BOB),
            address(pamm),
            FallbackSwaps.curve(TRIPOOL, 1, 0, 1)
        );

        assertEq(IERC20(USDC_ADDR).balanceOf(BOB), CURVE_USDC_OUT);
        _assertRouterDrained(DAI_ADDR, USDC_ADDR);
    }

    /// A crypto pool takes the `uint256` exchange signature -- the dispatch
    /// branch the stable-pool test never reaches.
    function testFallsBackToCurveCryptoPool() public {
        uint256 amountIn = 1 ether;
        deal(WETH_ADDR, address(router), amountIn);

        router.swap(
            FallbackSwaps.swap(WETH_ADDR, USDC_ADDR, amountIn, BOB),
            address(pamm),
            FallbackSwaps.curve(TRICRYPTO_POOL, 3, 2, 0)
        );

        assertEq(IERC20(USDC_ADDR).balanceOf(BOB), CURVE_CRYPTO_USDC_OUT);
        _assertRouterDrained(WETH_ADDR, USDC_ADDR);
    }

    /// V4 runs inside `unlockCallback`, where this contract syncs, transfers and settles.
    function testFallsBackToUniswapV4() public {
        uint256 amountIn = 100 ether;
        deal(USDE_ADDR, address(router), amountIn);

        router.swap(
            FallbackSwaps.swap(USDE_ADDR, USDT_ADDR, amountIn, BOB),
            address(pamm),
            FallbackSwaps.uniswapV4(100, 1, address(0), bytes(""))
        );

        assertEq(IERC20(USDT_ADDR).balanceOf(BOB), V4_USDT_OUT);
        _assertRouterDrained(USDE_ADDR, USDT_ADDR);
    }

    /// Reverse direction: the `!zeroForOne` currency assignment and sqrt limit
    /// inside `unlockCallback`.
    function testFallsBackToUniswapV4Reverse() public {
        uint256 amountIn = 100e6;
        deal(USDT_ADDR, address(router), amountIn);

        router.swap(
            FallbackSwaps.swap(USDT_ADDR, USDE_ADDR, amountIn, BOB),
            address(pamm),
            FallbackSwaps.uniswapV4(100, 1, address(0), bytes(""))
        );

        assertEq(IERC20(USDE_ADDR).balanceOf(BOB), V4_USDE_OUT);
        _assertRouterDrained(USDT_ADDR, USDE_ADDR);
    }

    /// Zero output counts as a failure and takes back the `tokenIn` already sent.
    function testPropAMMPayingNothingFallsThrough() public {
        SilentPropAMM silent = new SilentPropAMM();
        deal(USDC_ADDR, address(router), USDC_IN);

        vm.expectEmit(address(router));
        emit TychoFallbackRouter.FallbackSwap(
            address(silent),
            USDC_ADDR,
            WETH_ADDR,
            USDC_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.PropAMMFailed
        );
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(silent),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(silent)), 0);
        _assertRouterDrained(USDC_ADDR, WETH_ADDR);
    }

    /// A failing fallback reverts the swap with the fallback's own error -- no try/catch around the
    /// fallback slot, and no third attempt.
    function testFallbackFailureReverts() public {
        RevertingPool pool = new RevertingPool();
        deal(USDC_ADDR, address(router), USDC_IN);

        vm.expectRevert(RevertingPool__Nope.selector);
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV3(address(pool))
        );
    }

    function testUnknownFallbackProtocolReverts() public {
        deal(USDC_ADDR, address(router), USDC_IN);

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoFallbackRouter__UnknownProtocol.selector, uint8(9)
            )
        );
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            abi.encodePacked(uint8(9), USDC_WETH_USV3)
        );
    }

    function testEmptyFallbackReverts() public {
        deal(USDC_ADDR, address(router), USDC_IN);

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoFallbackRouter__InvalidSwapLength.selector, uint256(0)
            )
        );
        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            bytes("")
        );
    }

    /// `executePropAMM` is external only so `swap` can wrap it in try/catch.
    function testExecutePropAMMRejectsExternalCaller() public {
        vm.expectRevert(TychoFallbackRouter__NotSelf.selector);
        router.executePropAMM(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm)
        );
    }

    /// No swap is running, so no protocol may be paid. A V3-family callback --
    /// canonical or a fork's renamed selector -- lands on the catch-all
    /// `fallback` and reverts on the context guard rather than paying out.
    function testCallbackRejectsStranger() public {
        (bool success, bytes memory ret) = address(router)
            .call(
                abi.encodeWithSignature(
                    "pancakeV3SwapCallback(int256,int256,bytes)",
                    int256(1),
                    int256(-1),
                    bytes("")
                )
            );
        assertFalse(success);
        assertEq(bytes4(ret), TychoFallbackRouter__InvalidCallback.selector);
    }

    /// A Uniswap V3 fork that renamed its callback (Pancake's
    /// `pancakeV3SwapCallback`) still fills: the pool chooses the selector and
    /// the catch-all `fallback` answers to it.
    function testFallsBackToRenamedV3Fork() public {
        uint256 amountOut = 3 ether;
        RenamedCallbackPool pool = new RenamedCallbackPool(WETH_ADDR, amountOut);
        deal(WETH_ADDR, address(pool), amountOut);
        deal(USDC_ADDR, address(router), USDC_IN);

        router.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.uniswapV3(address(pool))
        );

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), amountOut);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pool)), USDC_IN);
        _assertRouterDrained(USDC_ADDR, WETH_ADDR);
    }

    function testDexCallbackRejectsStranger() public {
        vm.expectRevert(TychoFallbackRouter__InvalidCallback.selector);
        router.dexCallback(USDC_ADDR, USDC_IN);
    }

    function testUnlockCallbackRejectsStranger() public {
        vm.expectRevert(TychoFallbackRouter__NotPoolManager.selector);
        router.unlockCallback(bytes(""));
    }
}

/// @notice The deployment shapes a chain missing a singleton gets, on the mainnet fork so the
/// zeroed slot is the only difference from `TychoFallbackRouterTest`. Split out from it because
/// the extra `new TychoFallbackRouter` sites pushed that contract past a solc assembler limit.
contract TychoFallbackRouterMultichainTest is TychoFallbackRouterTestBase {
    /// `TychoFallbackRouterTest`'s block and USDC/WETH figures, so the two contracts assert the
    /// same numbers.
    uint256 constant FORK_BLOCK = 22_689_128;
    uint256 constant USDC_IN = 10_000e6;
    uint256 constant V3_WETH_OUT = 3_611_998_638_539_827_447;

    function getForkBlock() internal pure override returns (uint256) {
        return FORK_BLOCK;
    }

    /// A chain without Uniswap V4 deploys with a zero PoolManager. The protocol
    /// byte then quotes by reverting with its name, which `swap` counts as zero,
    /// and running it reverts the same way instead of calling `address(0)`.
    function testUniswapV4UnavailableWithoutPoolManager() public {
        TychoFallbackRouter noV4 = new TychoFallbackRouter(
            IPoolManager(address(0)),
            FLUIDV1_LIQUIDITY,
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER)
        );
        TychoFallbackRouter.Swap memory swap_ =
            FallbackSwaps.swap(USDE_ADDR, USDT_ADDR, 100 ether, BOB);
        bytes memory v4 = FallbackSwaps.uniswapV4(100, 1, address(0), bytes(""));
        bytes memory unavailable = abi.encodeWithSelector(
            TychoFallbackRouter__ProtocolUnavailable.selector,
            uint8(TychoFallbackRouter.FallbackProtocol.UniswapV4)
        );
        deal(USDE_ADDR, address(noV4), 100 ether);

        vm.prank(address(noV4));
        vm.expectRevert(unavailable);
        noV4.quoteFallback(swap_, v4);

        vm.expectRevert(unavailable);
        noV4.swap(swap_, address(pamm), v4);
    }

    /// Same for a chain without Fluid.
    function testFluidV1UnavailableWithoutLiquidity() public {
        TychoFallbackRouter noFluid = new TychoFallbackRouter(
            IPoolManager(POOL_MANAGER),
            address(0),
            IUniswapV3StaticQuoter(UNISWAP_V3_STATIC_QUOTER)
        );
        deal(USDC_ADDR, address(noFluid), USDC_IN);

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoFallbackRouter__ProtocolUnavailable.selector,
                uint8(TychoFallbackRouter.FallbackProtocol.FluidV1)
            )
        );
        noFluid.swap(
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB),
            address(pamm),
            FallbackSwaps.fluidV1(FLUIDV1_LIQUIDITY, true)
        );
    }

    /// Without a static quoter a Uniswap V3 fallback is quoted by simulation:
    /// the quote is the fill, the simulation rolls back, and a pAMM quoting
    /// below the pool is displaced exactly as it is on a quoted deployment.
    function testUniswapV3QuotedBySimulationWithoutStaticQuoter() public {
        TychoFallbackRouter simulated = new TychoFallbackRouter(
            IPoolManager(address(0)),
            address(0),
            IUniswapV3StaticQuoter(address(0))
        );
        TychoFallbackRouter.Swap memory swap_ =
            FallbackSwaps.swap(USDC_ADDR, WETH_ADDR, USDC_IN, BOB);
        bytes memory v3 = FallbackSwaps.uniswapV3(USDC_WETH_USV3);
        deal(USDC_ADDR, address(simulated), USDC_IN);

        vm.prank(address(simulated));
        assertEq(simulated.quoteFallback(swap_, v3), V3_WETH_OUT);
        // The simulation rolled back.
        assertEq(IERC20(USDC_ADDR).balanceOf(address(simulated)), USDC_IN);
        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), 0);

        // 1 WETH for 10 000 USDC, below the Uniswap V3 price of roughly 3.6 WETH.
        pamm.setPrice(USDC_ADDR, WETH_ADDR, 1e26);
        deal(WETH_ADDR, address(pamm), 100 ether);

        vm.expectEmit(address(simulated));
        emit TychoFallbackRouter.FallbackSwap(
            address(pamm),
            USDC_ADDR,
            WETH_ADDR,
            USDC_IN,
            TychoFallbackRouter.FallbackProtocol.UniswapV3,
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        simulated.swap(swap_, address(pamm), v3);

        assertEq(IERC20(WETH_ADDR).balanceOf(BOB), V3_WETH_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pamm)), 0);
    }
}

/// @notice Fluid pulls `tokenIn` through `dexCallback`.
contract TychoFallbackRouterFluidTest is TychoFallbackRouterTestBase {
    address constant FLUID_DEX = 0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b;
    address constant SUSDE_ADDR = 0x9D39A5DE30e57443BfF2A8307A4256c8797A3497;

    /// The sUSDE/USDT dex has no code at this contract's sibling block, so
    /// these tests fork later.
    uint256 constant FORK_BLOCK = 23_748_828;

    /// Measured at FORK_BLOCK against FLUID_DEX.
    uint256 constant FLUID_USDT_OUT = 12_006_909;
    uint256 constant FLUID_SUSDE_OUT = 8_326_872_266_375_000_000;

    function getForkBlock() internal pure override returns (uint256) {
        return FORK_BLOCK;
    }

    /// The router's deterministic deploy address already holds 1 sUSDE at this
    /// block, so zero it to make `_assertRouterDrained` exact.
    function setUp() public override {
        super.setUp();
        deal(SUSDE_ADDR, address(router), 0);
    }

    /// Fluid's dead-address estimate is the amount the dex then fills.
    function testFluidQuoteMatchesFill() public {
        assertEq(
            _quoteFallback(
                FallbackSwaps.swap(SUSDE_ADDR, USDT_ADDR, 10e18, BOB),
                FallbackSwaps.fluidV1(FLUID_DEX, true)
            ),
            FLUID_USDT_OUT
        );
        assertEq(
            _quoteFallback(
                FallbackSwaps.swap(USDT_ADDR, SUSDE_ADDR, 10e6, BOB),
                FallbackSwaps.fluidV1(FLUID_DEX, false)
            ),
            FLUID_SUSDE_OUT
        );
    }

    function testFallsBackToFluidV1() public {
        uint256 amountIn = 10e18;
        deal(SUSDE_ADDR, address(router), amountIn);

        router.swap(
            FallbackSwaps.swap(SUSDE_ADDR, USDT_ADDR, amountIn, BOB),
            address(pamm),
            FallbackSwaps.fluidV1(FLUID_DEX, true)
        );

        assertEq(IERC20(USDT_ADDR).balanceOf(BOB), FLUID_USDT_OUT);
        _assertRouterDrained(SUSDE_ADDR, USDT_ADDR);
    }

    /// `zero2one = false` consistently encoded: the dex requests USDT, which
    /// is the swap's tokenIn, so the swap fills in the reverse direction.
    function testFallsBackToFluidV1Reverse() public {
        uint256 amountIn = 10e6;
        deal(USDT_ADDR, address(router), amountIn);

        router.swap(
            FallbackSwaps.swap(USDT_ADDR, SUSDE_ADDR, amountIn, BOB),
            address(pamm),
            FallbackSwaps.fluidV1(FLUID_DEX, false)
        );

        assertEq(IERC20(SUSDE_ADDR).balanceOf(BOB), FLUID_SUSDE_OUT);
        _assertRouterDrained(USDT_ADDR, SUSDE_ADDR);
    }

    /// `zero2one = true` means the dex pulls sUSDE, but the swap pays USDT, so
    /// `dexCallback` is asked for the wrong token and names the cause. The
    /// amount carries sUSDE's 18 decimals rather than USDT's 6 because the dex
    /// prices `amountIn` against the sUSDE side first: a realistic USDT amount
    /// is dust there and reverts inside the dex, which is the test below.
    function testFluidWrongDirectionNamesCause() public {
        uint256 amountIn = 10e18;
        deal(USDT_ADDR, address(router), amountIn);

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoFallbackRouter__CallbackTokenMismatch.selector,
                SUSDE_ADDR,
                USDT_ADDR
            )
        );
        router.swap(
            FallbackSwaps.swap(USDT_ADDR, SUSDE_ADDR, amountIn, BOB),
            address(pamm),
            FallbackSwaps.fluidV1(FLUID_DEX, true)
        );
    }

    /// The other mis-encoding never reaches `dexCallback`: the dex prices the
    /// amount against its own reserves first, and 10e18 is far past what the
    /// USDT side holds, so Fluid's own error is the swap's error.
    function testFluidWrongDirectionRevertsInsideDex() public {
        uint256 amountIn = 10e18;
        deal(SUSDE_ADDR, address(router), amountIn);

        vm.expectPartialRevert(FluidDexError.selector);
        router.swap(
            FallbackSwaps.swap(SUSDE_ADDR, USDT_ADDR, amountIn, BOB),
            address(pamm),
            FallbackSwaps.fluidV1(FLUID_DEX, false)
        );
    }
}

/// @notice Aerodrome V1 lives on Base. Base has no Fluid, so this is also a deployment with
/// that slot zeroed.
contract TychoFallbackRouterAerodromeTest is
    TychoFallbackRouterTestBase,
    AerodromeV1TestBase
{
    /// Uniswap V4's PoolManager on Base, from `executor_deployments.json`.
    address constant BASE_POOL_MANAGER =
        0x498581fF718922c3f8e6A244956aF099B2652b2b;
    /// Eden's Uniswap V3 static quoter on Base, from `deploy-fallback-router.js`.
    address constant BASE_STATIC_QUOTER =
        0x28aF629a9F3ECE3c8D9F0b7cCf6349708CeC8cFb;

    /// The block `AerodromeV1.t.sol` forks at, where both pools hold liquidity.
    uint256 constant FORK_BLOCK = 44_682_102;

    function getForkBlock() internal pure override returns (uint256) {
        return FORK_BLOCK;
    }

    function setUp() public override {
        vm.createSelectFork(vm.rpcUrl("base"), getForkBlock());
        router = new TychoFallbackRouter(
            IPoolManager(BASE_POOL_MANAGER),
            address(0),
            IUniswapV3StaticQuoter(BASE_STATIC_QUOTER)
        );
        pamm = new MockPropAMM();
    }

    /// The pool's own quote is the fill: Solidly pools price their fee and
    /// curve themselves, which is why they are not byte 0.
    function testAerodromeV1QuoteMatchesFill() public {
        uint256 amountIn = 0.01 ether;
        uint256 expectedOut = IAerodromeV1Pool(AERODROME_V1_VOLATILE_POOL)
            .getAmountOut(amountIn, AERODROME_V1_TBTC);
        assertGt(expectedOut, 0);

        assertEq(
            _quoteFallback(
                FallbackSwaps.swap(
                    AERODROME_V1_TBTC, AERODROME_V1_USDBC, amountIn, BOB
                ),
                FallbackSwaps.aerodromeV1(AERODROME_V1_VOLATILE_POOL)
            ),
            expectedOut
        );
    }

    /// tBTC sorts below USDbC, so this is the `zeroForOne` `pair.swap` order.
    function testFallsBackToAerodromeV1() public {
        uint256 amountIn = 0.01 ether;
        uint256 expectedOut = IAerodromeV1Pool(AERODROME_V1_VOLATILE_POOL)
            .getAmountOut(amountIn, AERODROME_V1_TBTC);
        deal(AERODROME_V1_TBTC, address(router), amountIn);

        _expectFallbackSwap(
            AERODROME_V1_TBTC,
            AERODROME_V1_USDBC,
            amountIn,
            TychoFallbackRouter.FallbackProtocol.AerodromeV1,
            TychoFallbackRouter.FallbackReason.FallbackQuotedHigher
        );
        router.swap(
            FallbackSwaps.swap(
                AERODROME_V1_TBTC, AERODROME_V1_USDBC, amountIn, BOB
            ),
            address(pamm),
            FallbackSwaps.aerodromeV1(AERODROME_V1_VOLATILE_POOL)
        );

        assertGt(expectedOut, 0);
        assertEq(IERC20(AERODROME_V1_USDBC).balanceOf(BOB), expectedOut);
        _assertRouterDrained(AERODROME_V1_TBTC, AERODROME_V1_USDBC);
    }

    /// Reverse direction: the `!zeroForOne` `pair.swap` argument order.
    function testFallsBackToAerodromeV1Reverse() public {
        uint256 amountIn = 10e6;
        uint256 expectedOut = IAerodromeV1Pool(AERODROME_V1_VOLATILE_POOL)
            .getAmountOut(amountIn, AERODROME_V1_USDBC);
        deal(AERODROME_V1_USDBC, address(router), amountIn);

        router.swap(
            FallbackSwaps.swap(
                AERODROME_V1_USDBC, AERODROME_V1_TBTC, amountIn, BOB
            ),
            address(pamm),
            FallbackSwaps.aerodromeV1(AERODROME_V1_VOLATILE_POOL)
        );

        assertGt(expectedOut, 0);
        assertEq(IERC20(AERODROME_V1_TBTC).balanceOf(BOB), expectedOut);
        _assertRouterDrained(AERODROME_V1_USDBC, AERODROME_V1_TBTC);
    }

    /// A stable pool prices on Solidly's x³y + xy³ curve, which `getAmountOut`
    /// hides behind the same interface as the volatile pool.
    function testFallsBackToAerodromeV1StablePool() public {
        uint256 amountIn = 10 ether;
        uint256 expectedOut = IAerodromeV1Pool(AERODROME_V1_STABLE_POOL)
            .getAmountOut(amountIn, AERODROME_V1_DOLA);
        deal(AERODROME_V1_DOLA, address(router), amountIn);

        router.swap(
            FallbackSwaps.swap(
                AERODROME_V1_DOLA, AERODROME_V1_USDBC, amountIn, BOB
            ),
            address(pamm),
            FallbackSwaps.aerodromeV1(AERODROME_V1_STABLE_POOL)
        );

        assertGt(expectedOut, 0);
        assertEq(IERC20(AERODROME_V1_USDBC).balanceOf(BOB), expectedOut);
        _assertRouterDrained(AERODROME_V1_DOLA, AERODROME_V1_USDBC);
    }

    /// Base has no Fluid, so its deployment zeroes the slot and byte 4 reverts.
    function testFluidV1UnavailableOnBase() public {
        deal(AERODROME_V1_USDBC, address(router), 10e6);

        vm.expectRevert(
            abi.encodeWithSelector(
                TychoFallbackRouter__ProtocolUnavailable.selector,
                uint8(TychoFallbackRouter.FallbackProtocol.FluidV1)
            )
        );
        router.swap(
            FallbackSwaps.swap(
                AERODROME_V1_USDBC, AERODROME_V1_TBTC, 10e6, BOB
            ),
            address(pamm),
            FallbackSwaps.fluidV1(AERODROME_V1_VOLATILE_POOL, true)
        );
    }
}

/// @notice The same claim through the whole TychoRouter: the swap's input lands at the fallback
/// router, not at a pool, which is what makes the retry fundable.
contract FallbackExecutorTest is TychoRouterTestSetup {
    MockPropAMM pamm;

    /// Measured at getForkBlock() against the pools each test names, so the
    /// value check is independent of the router's minAmountOut check.
    uint256 constant SINGLE_WETH_OUT = 3_611_998_638_539_827_447;
    uint256 constant SEQUENTIAL_DAI_OUT = 9_916_791_090_861_983_461_371;
    uint256 constant SEQUENTIAL_FALLBACK_SECOND_DAI_OUT =
        9_916_211_621_040_833_220_196;
    uint256 constant FEE_WETH_OUT = 3_575_878_652_154_429_173;
    uint256 constant SPLIT_WETH_OUT = 3_612_457_039_884_311_273;
    uint256 constant SUSHI_WETH_OUT = 3_587_564_182_454_912_624;

    function getForkBlock() public pure override returns (uint256) {
        return 22689128;
    }

    function setUp() public override {
        super.setUp();
        pamm = new MockPropAMM();
    }

    function testSingleSwapFromRustCalldata() public {
        uint256 amountIn = 10_000e6;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_fallback_usdc_weth"
        );

        deal(USDC_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), SINGLE_WETH_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(fallbackRouter)), 0);
    }

    /// The `sushiswap_v2` fork name, encoded in Rust, fills through the Uniswap V2 fallback path
    /// against a real SushiSwap USDC/WETH pair.
    function testSushiswapV2AliasFromRustCalldata() public {
        uint256 amountIn = 10_000e6;
        bytes memory callData = loadCallDataFromFile(
            "test_single_encoding_strategy_fallback_sushiswap_v2_alias"
        );

        deal(USDC_ADDR, ALICE, amountIn);
        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        (bool success,) = tychoRouterAddr.call(callData);
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), SUSHI_WETH_OUT);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(fallbackRouter)), 0);
    }

    function testGetTransferData() public view {
        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = fallbackExecutor.getTransferData(_swapData());

        assertEq(
            uint8(transferType), uint8(TransferManager.TransferType.Transfer)
        );
        assertEq(receiver, address(fallbackRouter));
        assertEq(tokenIn, USDC_ADDR);
        assertEq(tokenOut, WETH_ADDR);
        assertFalse(outputToRouter);
    }

    function testFundsExpectedAddress() public view {
        assertEq(
            fallbackExecutor.fundsExpectedAddress(_swapData()),
            address(fallbackRouter)
        );
    }

    function testInvalidDataLength() public {
        vm.expectRevert(
            abi.encodeWithSelector(
                FallbackExecutor__InvalidDataLength.selector, 40
            )
        );
        fallbackExecutor.getTransferData(abi.encodePacked(USDC_ADDR, WETH_ADDR));
    }

    function testConstructorRejectsZeroAddress() public {
        vm.expectRevert(FallbackExecutor__AddressZero.selector);
        new FallbackExecutor(address(0));
    }

    /// The whole swap: a dead pAMM still settles, at the Uniswap V3 price.
    function testSingleSwap() public {
        uint256 amountIn = 10_000e6;
        deal(USDC_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            USDC_ADDR,
            WETH_ADDR,
            1 ether,
            1 ether,
            ALICE,
            noClientFee(),
            encodeSingleSwap(address(fallbackExecutor), _swapData())
        );
        vm.stopPrank();

        assertEq(amountOut, SINGLE_WETH_OUT);
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), amountOut);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(fallbackRouter)), 0);
    }

    /// The whole swap when the pAMM quotes above the pool: the pAMM fills, measured by the
    /// Dispatcher's balance diff like any other leg.
    function testSingleSwapPropAMMFills() public {
        pamm.setPrice(USDC_ADDR, WETH_ADDR, 5e26);
        deal(WETH_ADDR, address(pamm), 100 ether);
        uint256 amountIn = 10_000e6;
        deal(USDC_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            USDC_ADDR,
            WETH_ADDR,
            5 ether,
            5 ether,
            ALICE,
            noClientFee(),
            encodeSingleSwap(address(fallbackExecutor), _swapData())
        );
        vm.stopPrank();

        assertEq(amountOut, 5 ether);
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), 5 ether);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(pamm)), amountIn);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(fallbackRouter)), 0);
    }

    /// The TychoRouter's `minAmountOut` is the swap's only price check.
    function testSingleSwapMinAmountOutBinds() public {
        uint256 amountIn = 10_000e6;
        deal(USDC_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        vm.expectPartialRevert(TychoRouter__NegativeSlippage.selector);
        tychoRouter.singleSwap(
            amountIn,
            USDC_ADDR,
            WETH_ADDR,
            1000 ether,
            1000 ether,
            ALICE,
            noClientFee(),
            encodeSingleSwap(address(fallbackExecutor), _swapData())
        );
        vm.stopPrank();
    }

    /// The fallback swap funds the next hop's pool directly.
    function testSequentialSwap() public {
        uint256 amountIn = 10_000e6;
        deal(USDC_ADDR, ALICE, amountIn);

        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(address(fallbackExecutor), _swapData());
        swaps[1] = encodeSequentialSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(DAI_WETH_UNIV2_POOL, WETH_ADDR, DAI_ADDR)
        );

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.sequentialSwap(
            amountIn,
            USDC_ADDR,
            DAI_ADDR,
            1000e18,
            1000e18,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
        vm.stopPrank();

        assertEq(amountOut, SEQUENTIAL_DAI_OUT);
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), amountOut);
        assertEq(IERC20(WETH_ADDR).balanceOf(address(fallbackRouter)), 0);
    }

    /// The fallback as the second hop, which `TransferManager._transfer` funds
    /// with no transfer of its own: the leg relies entirely on hop one having
    /// paid `fundsExpectedAddress()`.
    function testSequentialSwapFallbackSecond() public {
        uint256 amountIn = 10_000e6;
        deal(USDC_ADDR, ALICE, amountIn);

        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSequentialSwap(
            address(usv2Executor),
            encodeUniswapV2Swap(USDC_WETH_USV2, USDC_ADDR, WETH_ADDR)
        );
        swaps[1] = encodeSequentialSwap(
            address(fallbackExecutor),
            abi.encodePacked(
                WETH_ADDR,
                DAI_ADDR,
                address(pamm),
                FallbackSwaps.uniswapV2(DAI_WETH_UNIV2_POOL, 30)
            )
        );

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.sequentialSwap(
            amountIn,
            USDC_ADDR,
            DAI_ADDR,
            1000e18,
            1000e18,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
        vm.stopPrank();

        assertEq(amountOut, SEQUENTIAL_FALLBACK_SECOND_DAI_OUT);
        assertEq(IERC20(DAI_ADDR).balanceOf(ALICE), amountOut);
        assertEq(IERC20(WETH_ADDR).balanceOf(address(fallbackRouter)), 0);
        assertEq(IERC20(DAI_ADDR).balanceOf(address(fallbackRouter)), 0);
    }

    /// With fees active the swap's receiver is redirected to the router itself,
    /// the configuration every fee-charging production swap runs in.
    function testSingleSwapWithRouterFee() public {
        vm.startPrank(FEE_SETTER);
        feeCalculator.setRouterFeeReceiver(routerFeeReceiver);
        feeCalculator.setRouterFeeOnOutput(1_000_000); // 1%
        vm.stopPrank();

        uint256 amountIn = 10_000e6;
        deal(USDC_ADDR, ALICE, amountIn);

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.singleSwap(
            amountIn,
            USDC_ADDR,
            WETH_ADDR,
            1 ether,
            1 ether,
            ALICE,
            noClientFee(),
            encodeSingleSwap(address(fallbackExecutor), _swapData())
        );
        vm.stopPrank();

        assertEq(amountOut, FEE_WETH_OUT);
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), amountOut);
        // fee == gross / 100, where gross == amountOut + fee.
        uint256 fee = tychoRouter.balanceOf(
            routerFeeReceiver, uint256(uint160(WETH_ADDR))
        );
        assertEq(fee, (amountOut + fee) / 100);
        // The fee stays in the router as the vault balance's backing.
        assertEq(IERC20(WETH_ADDR).balanceOf(tychoRouterAddr), fee);
        assertEq(IERC20(WETH_ADDR).balanceOf(address(fallbackRouter)), 0);
    }

    /// A split swap sends a fraction of the input, the one place a
    /// TransferType.Transfer executor is funded with less than the router's
    /// whole balance. 60% goes through the fallback swap, the rest through
    /// Uniswap V2 directly.
    function testSplitSwap() public {
        uint256 amountIn = 10_000e6;
        deal(USDC_ADDR, ALICE, amountIn);

        bytes[] memory swaps = new bytes[](2);
        swaps[0] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            (0xffffff * 60) / 100, // 60%
            address(fallbackExecutor),
            _swapData()
        );
        swaps[1] = encodeSplitSwap(
            uint8(0),
            uint8(1),
            uint24(0), // remainder
            address(usv2Executor),
            encodeUniswapV2Swap(USDC_WETH_USV2, USDC_ADDR, WETH_ADDR)
        );

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        uint256 amountOut = tychoRouter.splitSwap(
            amountIn,
            USDC_ADDR,
            WETH_ADDR,
            1 ether,
            1 ether,
            2,
            ALICE,
            noClientFee(),
            pleEncode(swaps)
        );
        vm.stopPrank();

        assertEq(amountOut, SPLIT_WETH_OUT);
        assertEq(IERC20(WETH_ADDR).balanceOf(ALICE), amountOut);
        assertEq(IERC20(USDC_ADDR).balanceOf(tychoRouterAddr), 0);
        assertEq(IERC20(USDC_ADDR).balanceOf(address(fallbackRouter)), 0);
    }

    /// A fallback protocol that reports success but pays nothing is caught by the
    /// route-level minAmountOut -- the backstop that replaces any in-slot
    /// output check in the fallback slot.
    function testZeroOutputFallbackFailsRouteMinAmountOut() public {
        SilentPool pool = new SilentPool();
        uint256 amountIn = 10_000e6;
        deal(USDC_ADDR, ALICE, amountIn);

        bytes memory swapData = abi.encodePacked(
            USDC_ADDR,
            WETH_ADDR,
            address(pamm),
            FallbackSwaps.uniswapV3(address(pool))
        );

        vm.startPrank(ALICE);
        IERC20(USDC_ADDR).approve(tychoRouterAddr, amountIn);
        vm.expectPartialRevert(TychoRouter__NegativeSlippage.selector);
        tychoRouter.singleSwap(
            amountIn,
            USDC_ADDR,
            WETH_ADDR,
            1 ether,
            1 ether,
            ALICE,
            noClientFee(),
            encodeSingleSwap(address(fallbackExecutor), swapData)
        );
        vm.stopPrank();
    }

    /// A pAMM with no price, then a Uniswap V3 retry.
    function _swapData() internal view returns (bytes memory) {
        return abi.encodePacked(
            USDC_ADDR,
            WETH_ADDR,
            address(pamm),
            FallbackSwaps.uniswapV3(USDC_WETH_USV3)
        );
    }
}
