// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {
    ReentrancyGuardTransient
} from "@openzeppelin/contracts/utils/ReentrancyGuardTransient.sol";
import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {
    IUniswapV2Pair
} from "@uniswap-v2/contracts/interfaces/IUniswapV2Pair.sol";
import {
    IUniswapV3Pool
} from "@uniswap/v3-core/contracts/interfaces/IUniswapV3Pool.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {SwapParams} from "@uniswap/v4-core/src/types/PoolOperation.sol";
import {Currency} from "@uniswap/v4-core/src/types/Currency.sol";
import {PoolKey} from "@uniswap/v4-core/src/types/PoolKey.sol";
import {BalanceDelta} from "@uniswap/v4-core/src/types/BalanceDelta.sol";
import {TickMath} from "@uniswap/v4-core/src/libraries/TickMath.sol";
import {IHooks} from "@uniswap/v4-core/src/interfaces/IHooks.sol";
import {IPropAMM} from "@interfaces/IPropAMM.sol";
import {
    CryptoPool as ICurveCryptoPool,
    StablePool as ICurveStablePool
} from "../executors/CurveExecutor.sol";
import {IFluidV1Dex} from "../executors/FluidV1Executor.sol";
import {UniswapV2Math} from "../../lib/UniswapV2Math.sol";

error TychoFallbackRouter__AddressZero();
error TychoFallbackRouter__CallbackTokenMismatch(
    address requested, address expected
);
error TychoFallbackRouter__InvalidCallback();
error TychoFallbackRouter__InvalidSwapLength(uint256 length);
error TychoFallbackRouter__InvalidUniswapV2Fee(uint256 feeBps);
error TychoFallbackRouter__NoOutput();
error TychoFallbackRouter__NotPoolManager();
error TychoFallbackRouter__NotSelf();
error TychoFallbackRouter__UnknownVenue(uint8 venue);

/// @title TychoFallbackRouter
/// @notice Runs a pAMM and, only if it fails, the caller's chosen fallback venue.
/// @dev Exists because an executor cannot fall back: the Dispatcher transfers a swap's input before
/// it delegatecalls `swap()`, so a reverting pAMM has already been paid and a Uniswap V3 retry,
/// which pays in a callback, cannot be funded. Here the tokens stay in this contract.
///
/// Holds no funds between transactions. A balance that does end up here (Curve rounding dust, a
/// mistaken transfer) is claimable by anyone through `swap` and is considered lost, which is also
/// why a Curve approval is left in place rather than revoked. Native ETH unsupported.
///
/// Fee-on-transfer and rebasing tokens are unsupported, and a route carrying one must not name
/// this contract. Every other executor is paid by the Dispatcher, which measures what landed at
/// the pool. This contract adds a second transfer, from here to the venue, and nothing measures
/// it: each venue is told `swap_.amountIn` while it receives less, so it fails its own input
/// check. A Uniswap V2 pair fails its K check, a V3 pool reverts `IIA`, and V4 reverts
/// `CurrencyNotSettled` because `settle` credits less than `poolManager.swap` specifies.
contract TychoFallbackRouter is ReentrancyGuardTransient {
    using SafeERC20 for IERC20;

    /// @notice The venue kinds a fallback may use.
    enum Venue {
        UniswapV2,
        UniswapV3,
        UniswapV4,
        Curve,
        FluidV1
    }

    /// @notice One swap swap: what goes in, what comes out, and who receives it.
    struct Swap {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        address receiver;
    }

    /// @notice The `poolManager.unlock` payload, decoded back in `unlockCallback`.
    struct UniswapV4Swap {
        Swap swap;
        uint24 fee;
        int24 tickSpacing;
        address hook;
        bytes hookData;
    }

    // keccak256("TychoFallbackRouter#CALLBACK_SOURCE")
    bytes32 private constant _CALLBACK_SOURCE_SLOT =
        0xf69ae8e0008b818aeb91c2b052698e485056e760fad9d0aa28144b842debe4f7;
    // keccak256("TychoFallbackRouter#CALLBACK_TOKEN")
    bytes32 private constant _CALLBACK_TOKEN_SLOT =
        0xbb428614797396c24d2ae21e3c7c9a28d69673f64cb7ba6433b600b67ed8541b;
    // keccak256("TychoFallbackRouter#CALLBACK_AMOUNT")
    bytes32 private constant _CALLBACK_AMOUNT_SLOT =
        0xde66fd0ca9c728ba44ca7bab17a304d328bf9cf5d5c72b8bf8ea7cd13765e542;

    IPoolManager public immutable poolManager;
    /// @notice Where `dexCallback` pays a Fluid dex.
    address public immutable fluidLiquidity;

    /// @notice The pAMM failed and `venue` filled instead. Absence of this event on a filled swap
    /// means the pAMM served it, which is the pAMM fill rate.
    /// @dev The pAMM's revert reason is deliberately not carried: reading it would copy
    /// caller-controlled returndata of any size into this frame.
    event FallbackSwap(
        address indexed pamm,
        address indexed tokenIn,
        address indexed tokenOut,
        uint256 amountIn,
        Venue venue
    );

    constructor(IPoolManager poolManager_, address fluidLiquidity_) {
        if (
            address(poolManager_) == address(0) || fluidLiquidity_ == address(0)
        ) {
            revert TychoFallbackRouter__AddressZero();
        }
        poolManager = poolManager_;
        fluidLiquidity = fluidLiquidity_;
    }

    /// @notice Runs `pamm` and, only if it fails, `fallbackSwap`. A failing fallback reverts the
    /// swap; there is no third attempt.
    /// @dev Permissionless: the caller names every parameter, so a balance sitting in this
    /// contract can be taken by anyone and is considered lost. Push-payment: the caller MUST
    /// transfer `swap_.amountIn` of `swap_.tokenIn` here first. Native ETH is not supported, and
    /// neither are fee-on-transfer or rebasing tokens -- see the contract doc for why.
    /// `fallbackSwap` names one of Uniswap V2, V3 or V4, Curve, or Fluid V1.
    /// No output is returned: the caller measures its own `swap_.tokenOut` balance diff at
    /// `swap_.receiver`, which is how the Dispatcher verifies every swap.
    function swap(
        Swap calldata swap_,
        address pamm,
        bytes calldata fallbackSwap
    ) external nonReentrant {
        try this.executePropAMM(swap_, pamm) {
            return;
        } catch {}

        Venue venue = _executeFallback(swap_, fallbackSwap);
        // Reentrancy cannot happen: the function is nonReentrant.
        // slither-disable-next-line reentrancy-events
        emit FallbackSwap(
            pamm, swap_.tokenIn, swap_.tokenOut, swap_.amountIn, venue
        );
    }

    /// @notice Runs the pAMM. External only so `swap` can try/catch it.
    function executePropAMM(Swap calldata swap_, address pamm) external {
        if (msg.sender != address(this)) {
            revert TychoFallbackRouter__NotSelf();
        }
        uint256 balanceBefore = IERC20(swap_.tokenOut).balanceOf(swap_.receiver);

        IERC20(swap_.tokenIn).safeTransfer(pamm, swap_.amountIn);
        // slither-disable-next-line unused-return
        IPropAMM(pamm)
            .swap(
                swap_.tokenIn,
                swap_.tokenOut,
                swap_.amountIn,
                0,
                swap_.receiver,
                block.timestamp
            );

        _requireOutput(swap_.tokenOut, swap_.receiver, balanceBefore);
    }

    /// @dev Runs the tagged venue, which pays `swap_.receiver` directly. No output measurement
    /// here: the Dispatcher's balance-diff at the receiver is the single source of truth.
    function _executeFallback(Swap calldata swap_, bytes calldata encodedSwap)
        internal
        returns (Venue venue)
    {
        if (encodedSwap.length == 0) {
            revert TychoFallbackRouter__InvalidSwapLength(encodedSwap.length);
        }

        uint8 venueByte = uint8(encodedSwap[0]);
        if (venueByte > uint8(type(Venue).max)) {
            revert TychoFallbackRouter__UnknownVenue(venueByte);
        }
        venue = Venue(venueByte);
        bytes calldata venueData = encodedSwap[1:];

        if (venue == Venue.UniswapV2) {
            _swapUniswapV2(swap_, venueData);
        } else if (venue == Venue.UniswapV3) {
            _swapUniswapV3(swap_, venueData);
        } else if (venue == Venue.UniswapV4) {
            _swapUniswapV4(swap_, venueData);
        } else if (venue == Venue.Curve) {
            _swapCurve(swap_, venueData);
        } else if (venue == Venue.FluidV1) {
            _swapFluidV1(swap_, venueData);
        } else {
            revert TychoFallbackRouter__UnknownVenue(venueByte);
        }
    }

    /// @dev Reverts on zero delivered, so a pAMM that fills with nothing still falls through to
    /// the fallback.
    function _requireOutput(
        address tokenOut,
        address receiver,
        uint256 balanceBefore
    ) internal view {
        if (IERC20(tokenOut).balanceOf(receiver) <= balanceBefore) {
            revert TychoFallbackRouter__NoOutput();
        }
    }

    /// @dev Uniswap V2's `swap` takes explicit output amounts, so this computes the output from
    /// the reserves.
    function _swapUniswapV2(Swap calldata swap_, bytes calldata data) internal {
        if (data.length != 21) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }
        IUniswapV2Pair pair = IUniswapV2Pair(address(bytes20(data[0:20])));
        uint256 feeBps = uint8(data[20]);
        if (feeBps > 30) {
            revert TychoFallbackRouter__InvalidUniswapV2Fee(feeBps);
        }

        bool zeroForOne = swap_.tokenIn < swap_.tokenOut;
        // slither-disable-next-line unused-return
        (uint112 reserve0, uint112 reserve1,) = pair.getReserves();
        uint256 calculatedAmount = UniswapV2Math.getAmountOut(
            swap_.amountIn,
            zeroForOne ? reserve0 : reserve1,
            zeroForOne ? reserve1 : reserve0,
            feeBps
        );

        IERC20(swap_.tokenIn).safeTransfer(address(pair), swap_.amountIn);
        if (zeroForOne) {
            pair.swap(0, calculatedAmount, swap_.receiver, "");
        } else {
            pair.swap(calculatedAmount, 0, swap_.receiver, "");
        }
    }

    function _swapUniswapV3(Swap calldata swap_, bytes calldata data) internal {
        if (data.length != 20) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }
        address pool = address(bytes20(data[0:20]));
        bool zeroForOne = swap_.tokenIn < swap_.tokenOut;

        _setCallbackContext(pool, swap_.tokenIn, swap_.amountIn);
        // slither-disable-next-line unused-return
        IUniswapV3Pool(pool)
            .swap(
                swap_.receiver,
                zeroForOne,
                int256(swap_.amountIn),
                zeroForOne
                    ? TickMath.MIN_SQRT_PRICE + 1
                    : TickMath.MAX_SQRT_PRICE - 1,
                ""
            );
        _clearCallbackContext();
    }

    /// @dev One pool, never a path: the currencies come from the sort order of `tokenIn` and
    /// `tokenOut`. Any hook the caller names is used -- there is no allowlist, so a hook that
    /// takes a fee or refuses the swap is the caller's problem to price into `minAmountOut`.
    function _swapUniswapV4(Swap calldata swap_, bytes calldata data) internal {
        if (data.length < 26) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }

        UniswapV4Swap memory v4Swap = UniswapV4Swap({
            swap: swap_,
            fee: uint24(bytes3(data[0:3])),
            tickSpacing: int24(uint24(bytes3(data[3:6]))),
            hook: address(bytes20(data[6:26])),
            hookData: data[26:]
        });

        // slither-disable-next-line unused-return
        poolManager.unlock(abi.encode(v4Swap));
    }

    /// @dev Curve pays the caller, so this forwards to `receiver`.
    function _swapCurve(Swap calldata swap_, bytes calldata data) internal {
        if (data.length != 23) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }
        address pool = address(bytes20(data[0:20]));
        uint8 poolType = uint8(data[20]);
        uint256 i = uint8(data[21]);
        uint256 j = uint8(data[22]);

        uint256 balanceBefore = IERC20(swap_.tokenOut).balanceOf(address(this));

        IERC20(swap_.tokenIn).forceApprove(pool, swap_.amountIn);
        if (poolType == 1 || poolType == 10) {
            // stable and stable_ng
            ICurveStablePool(pool)
                .exchange(
                    int128(uint128(i)), int128(uint128(j)), swap_.amountIn, 0
                );
        } else {
            // crypto or llamma
            ICurveCryptoPool(pool).exchange(i, j, swap_.amountIn, 0);
        }

        uint256 received =
            IERC20(swap_.tokenOut).balanceOf(address(this)) - balanceBefore;
        IERC20(swap_.tokenOut).safeTransfer(swap_.receiver, received);
    }

    /// @dev `zero2one` is the dex's token order, not the address sort order, so it cannot be
    /// derived.
    function _swapFluidV1(Swap calldata swap_, bytes calldata data) internal {
        if (data.length != 21) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }
        address dex = address(bytes20(data[0:20]));
        bool zero2one = uint8(data[20]) > 0;

        _setCallbackContext(dex, swap_.tokenIn, swap_.amountIn);
        // slither-disable-next-line unused-return
        IFluidV1Dex(dex)
            .swapInWithCallback(zero2one, swap_.amountIn, 0, swap_.receiver);
        _clearCallbackContext();
    }

    /// @notice Pays a Uniswap V3 pool.
    /// @dev The pool's deltas are ignored; token and amount come from the callback context.
    function uniswapV3SwapCallback(
        int256, /* amount0Delta */
        int256, /* amount1Delta */
        bytes calldata /* data */
    )
        external
    {
        (address tokenIn, uint256 amountIn) = _consumeCallbackContext();
        IERC20(tokenIn).safeTransfer(msg.sender, amountIn);
    }

    /// @notice Pays the Fluid liquidity layer. The requested token must match the callback
    /// context -- a mismatch means the encoded `zero2one` contradicts the swap -- but the paid
    /// amount comes from the context, never from the dex.
    function dexCallback(
        address token_,
        uint256 /* amount_ */
    )
        external
    {
        (address tokenIn, uint256 amountIn) = _consumeCallbackContext();
        if (token_ != tokenIn) {
            revert TychoFallbackRouter__CallbackTokenMismatch(token_, tokenIn);
        }
        IERC20(tokenIn).safeTransfer(fluidLiquidity, amountIn);
    }

    /// @notice Runs the Uniswap V4 swap inside the PoolManager's unlock: pays `swap_.amountIn`, swaps
    /// the single pool named by the venue data, and sends the output to `swap_.receiver`.
    /// @dev The pool key's currencies come from the sort order of `swap_.tokenIn` and `swap_.tokenOut`,
    /// so the venue data carries no direction.
    function unlockCallback(bytes calldata data)
        external
        returns (bytes memory)
    {
        if (msg.sender != address(poolManager)) {
            revert TychoFallbackRouter__NotPoolManager();
        }
        UniswapV4Swap memory v4Swap = abi.decode(data, (UniswapV4Swap));
        Swap memory swap_ = v4Swap.swap;
        bool zeroForOne = swap_.tokenIn < swap_.tokenOut;

        PoolKey memory key = PoolKey({
            currency0: Currency.wrap(
                zeroForOne ? swap_.tokenIn : swap_.tokenOut
            ),
            currency1: Currency.wrap(
                zeroForOne ? swap_.tokenOut : swap_.tokenIn
            ),
            fee: v4Swap.fee,
            tickSpacing: v4Swap.tickSpacing,
            hooks: IHooks(v4Swap.hook)
        });

        poolManager.sync(Currency.wrap(swap_.tokenIn));
        IERC20(swap_.tokenIn).safeTransfer(address(poolManager), swap_.amountIn);
        // slither-disable-next-line unused-return
        poolManager.settle();

        BalanceDelta delta = poolManager.swap(
            key,
            SwapParams(
                zeroForOne,
                -int256(swap_.amountIn),
                zeroForOne
                    ? TickMath.MIN_SQRT_PRICE + 1
                    : TickMath.MAX_SQRT_PRICE - 1
            ),
            v4Swap.hookData
        );

        int128 amountOut = zeroForOne ? delta.amount1() : delta.amount0();
        // A negative delta (hostile hook) wraps to an amount `take` cannot pay, so it reverts
        // there; a zero delta fails the route-level minAmountOut like any other empty venue.
        poolManager.take(
            Currency.wrap(swap_.tokenOut),
            swap_.receiver,
            uint256(uint128(amountOut))
        );
        return "";
    }

    function _clearCallbackContext() internal {
        _setCallbackContext(address(0), address(0), 0);
    }

    function _setCallbackContext(address source, address token, uint256 amount)
        internal
    {
        // slither-disable-next-line assembly
        assembly {
            tstore(_CALLBACK_SOURCE_SLOT, source)
            tstore(_CALLBACK_TOKEN_SLOT, token)
            tstore(_CALLBACK_AMOUNT_SLOT, amount)
        }
    }

    /// @dev Clears the context, so one callback cannot pay twice.
    function _consumeCallbackContext()
        internal
        returns (address token, uint256 amount)
    {
        address source;
        // slither-disable-next-line assembly
        assembly {
            source := tload(_CALLBACK_SOURCE_SLOT)
            token := tload(_CALLBACK_TOKEN_SLOT)
            amount := tload(_CALLBACK_AMOUNT_SLOT)
            tstore(_CALLBACK_SOURCE_SLOT, 0)
            tstore(_CALLBACK_TOKEN_SLOT, 0)
            tstore(_CALLBACK_AMOUNT_SLOT, 0)
        }
        // An unset context has source == address(0), which no real sender matches.
        if (msg.sender != source) {
            revert TychoFallbackRouter__InvalidCallback();
        }
    }
}
