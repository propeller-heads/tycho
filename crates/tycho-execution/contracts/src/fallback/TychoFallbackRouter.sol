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
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {
    ICurveCryptoPool,
    ICurveStablePool,
    CurvePoolType
} from "@interfaces/ICurvePool.sol";
import {IFluidV1Dex, FluidDexSwapResult} from "@interfaces/IFluidV1Dex.sol";
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
/// @notice Not a failure: carries the amount `simulateFallback` measured, so the swap it ran
/// rolls back.
error TychoFallbackRouter__SimulatedAmountOut(uint256 amountOut);
error TychoFallbackRouter__UnknownProtocol(uint8 protocol);

/// @title TychoFallbackRouter
/// @notice Quotes a pAMM against the caller's chosen fallback protocol and runs whichever quotes
/// more `tokenOut`. A pAMM that wins the quote but fails still falls through to the fallback.
/// @dev Exists because an executor cannot fall back: the Dispatcher transfers a swap's input before
/// it delegatecalls `swap()`, so a reverting pAMM has already been paid and a Uniswap V3 retry,
/// which pays in a callback, cannot be funded. Here the tokens stay in this contract.
///
/// Holds no funds between transactions. A balance that does end up here (Curve rounding dust, a
/// mistaken transfer) is claimable by anyone through `swap` and is considered lost, which is also
/// why a Curve approval is left in place rather than revoked. Native ETH, fee-on-transfer and
/// rebasing tokens are unsupported.
contract TychoFallbackRouter is ReentrancyGuardTransient {
    using SafeERC20 for IERC20;

    /// @notice The protocols a fallback may use.
    enum FallbackProtocol {
        UniswapV2,
        UniswapV3,
        UniswapV4,
        Curve,
        FluidV1
    }

    /// @notice Why the fallback ran instead of the pAMM.
    enum FallbackReason {
        // The fallback quoted more `tokenOut` than the pAMM. A pAMM that cannot quote counts as
        // quoting zero, so a stale pAMM is skipped without its swap being attempted.
        FallbackQuotedHigher,
        // The pAMM quoted at least as much as the fallback, then reverted or delivered nothing.
        PropAMMFailed
    }

    /// @notice One swap: what goes in, what comes out, and who receives it.
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
    /// @notice Prices a Uniswap V3 fallback without running it.
    IUniswapV3StaticQuoter public immutable uniswapV3StaticQuoter;

    /// @notice `protocol` filled instead of the pAMM, for `reason`. Absence of this event on a
    /// filled swap means the pAMM served it, which is the pAMM fill rate.
    /// @dev The pAMM's revert reason is deliberately not carried: reading it would copy
    /// caller-controlled returndata of any size into this frame.
    event FallbackSwap(
        address indexed pamm,
        address indexed tokenIn,
        address indexed tokenOut,
        uint256 amountIn,
        FallbackProtocol protocol,
        FallbackReason reason
    );

    constructor(
        IPoolManager poolManager_,
        address fluidLiquidity_,
        IUniswapV3StaticQuoter uniswapV3StaticQuoter_
    ) {
        if (
            address(poolManager_) == address(0) || fluidLiquidity_ == address(0)
                || address(uniswapV3StaticQuoter_) == address(0)
        ) {
            revert TychoFallbackRouter__AddressZero();
        }
        poolManager = poolManager_;
        fluidLiquidity = fluidLiquidity_;
        uniswapV3StaticQuoter = uniswapV3StaticQuoter_;
    }

    /// @notice Quotes `pamm` and `fallbackSwap`, then runs the fallback if it quotes more
    /// `tokenOut`, otherwise `pamm` and, only if that fails, `fallbackSwap`. A failing fallback
    /// reverts the swap; there is no third attempt.
    /// @dev Permissionless: the caller names every parameter, so a balance sitting in this
    /// contract can be taken by anyone and is considered lost. Push-payment: the caller MUST
    /// transfer `swap_.amountIn` of `swap_.tokenIn` here first. Native ETH, fee-on-transfer and
    /// rebasing tokens are not supported.
    /// `fallbackSwap` names one of Uniswap V2, V3 or V4, Curve, or Fluid V1.
    /// A quote that reverts counts as zero, so two failed quotes keep the pAMM-first order and
    /// equal quotes keep the pAMM.
    /// No output is returned: the caller measures its own `swap_.tokenOut` balance diff at
    /// `swap_.receiver`, which is how the Dispatcher verifies every swap.
    function swap(
        Swap calldata swap_,
        address pamm,
        bytes calldata fallbackSwap
    ) external nonReentrant {
        // Low-level so a `pamm` without code, or one returning nothing decodable, quotes zero
        // instead of reverting `swap`. That covers `pamm == address(0)`, so no zero check.
        // slither-disable-next-line low-level-calls,missing-zero-check
        (bool quoted, bytes memory quote) = pamm.call(
            abi.encodeCall(
                IPropAMM.quote, (swap_.tokenIn, swap_.tokenOut, swap_.amountIn)
            )
        );
        uint256 pammAmountOut =
            quoted && quote.length >= 32 ? abi.decode(quote, (uint256)) : 0;

        uint256 fallbackAmountOut = 0;
        try this.quoteFallback(swap_, fallbackSwap) returns (
            uint256 amountOut
        ) {
            fallbackAmountOut = amountOut;
        } catch {}

        FallbackReason reason = FallbackReason.FallbackQuotedHigher;
        if (fallbackAmountOut <= pammAmountOut) {
            try this.executePropAMM(swap_, pamm) {
                return;
            } catch {}
            reason = FallbackReason.PropAMMFailed;
        }

        FallbackProtocol protocol = _executeFallback(swap_, fallbackSwap);
        // Reentrancy cannot happen: `swap` is nonReentrant.
        // slither-disable-next-line reentrancy-events
        emit FallbackSwap(
            pamm,
            swap_.tokenIn,
            swap_.tokenOut,
            swap_.amountIn,
            protocol,
            reason
        );
    }

    /// @notice Runs the pAMM. External only so `swap` can try/catch it.
    function executePropAMM(Swap calldata swap_, address pamm) external {
        _requireSelf();
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

        // Reverts on zero delivered, so a pAMM that fills with nothing still falls through
        // to the fallback.
        if (IERC20(swap_.tokenOut).balanceOf(swap_.receiver) <= balanceBefore) {
            revert TychoFallbackRouter__NoOutput();
        }
    }

    /// @notice Quotes the fallback protocol. External only so `swap` can try/catch it: a
    /// protocol that cannot quote, or malformed protocol data, is a zero quote.
    function quoteFallback(Swap calldata swap_, bytes calldata fallbackSwap)
        external
        returns (uint256 amountOut)
    {
        _requireSelf();
        (FallbackProtocol protocol, bytes calldata protocolData) =
            _decodeFallback(fallbackSwap);

        if (protocol == FallbackProtocol.UniswapV2) {
            (IUniswapV2Pair pair, uint256 feeBps) =
                _decodeUniswapV2(protocolData);
            return _uniswapV2AmountOut(swap_, pair, feeBps);
        }
        if (protocol == FallbackProtocol.UniswapV3) {
            return _quoteUniswapV3(swap_, protocolData);
        }
        if (protocol == FallbackProtocol.Curve) {
            return _quoteCurve(swap_, protocolData);
        }
        if (protocol == FallbackProtocol.FluidV1) {
            return _quoteFluidV1(swap_, protocolData);
        }
        try this.simulateFallback(swap_, fallbackSwap) {
            return 0;
        } catch (bytes memory reason) {
            return _decodeAmountOut(
                reason, TychoFallbackRouter__SimulatedAmountOut.selector
            );
        }
    }

    /// @notice Runs the fallback, then reverts `TychoFallbackRouter__SimulatedAmountOut` with
    /// the amount it delivered so the swap rolls back. External only so `quoteFallback` can
    /// try/catch it.
    function simulateFallback(Swap calldata swap_, bytes calldata fallbackSwap)
        external
    {
        _requireSelf();
        uint256 balanceBefore = IERC20(swap_.tokenOut).balanceOf(swap_.receiver);
        _executeFallback(swap_, fallbackSwap);
        revert TychoFallbackRouter__SimulatedAmountOut(IERC20(swap_.tokenOut)
                    .balanceOf(swap_.receiver) - balanceBefore);
    }

    /// @dev Same arguments as `_swapUniswapV3`, so the quote is the fill.
    function _quoteUniswapV3(Swap calldata swap_, bytes calldata data)
        internal
        view
        returns (uint256 amountOut)
    {
        address pool = _decodeUniswapV3(data);
        bool zeroForOne = swap_.tokenIn < swap_.tokenOut;
        (int256 amount0, int256 amount1) = uniswapV3StaticQuoter.quote(
            pool,
            zeroForOne,
            int256(swap_.amountIn),
            zeroForOne
                ? TickMath.MIN_SQRT_PRICE + 1
                : TickMath.MAX_SQRT_PRICE - 1
        );
        return uint256(-(zeroForOne ? amount1 : amount0));
    }

    function _quoteCurve(Swap calldata swap_, bytes calldata data)
        internal
        view
        returns (uint256 amountOut)
    {
        (address pool, uint8 poolType, uint256 i, uint256 j) =
            _decodeCurve(data);
        if (CurvePoolType.isStable(poolType)) {
            return ICurveStablePool(pool)
                .get_dy(int128(uint128(i)), int128(uint128(j)), swap_.amountIn);
        }
        return ICurveCryptoPool(pool).get_dy(i, j, swap_.amountIn);
    }

    /// @dev Paying `0xdEaD` makes the dex revert `FluidDexSwapResult` with the amount before it
    /// pulls any token.
    function _quoteFluidV1(Swap calldata swap_, bytes calldata data)
        internal
        returns (uint256 amountOut)
    {
        (address dex, bool zero2one) = _decodeFluidV1(data);
        // slither-disable-next-line unused-return
        try IFluidV1Dex(dex)
            .swapIn(zero2one, swap_.amountIn, 0, address(0xdEaD)) {
            return 0;
        } catch (bytes memory reason) {
            return _decodeAmountOut(reason, FluidDexSwapResult.selector);
        }
    }

    /// @dev The `uint256` a `selector(uint256)` revert carries, or zero for any other revert.
    function _decodeAmountOut(bytes memory reason, bytes4 selector)
        internal
        pure
        returns (uint256 amountOut)
    {
        if (reason.length != 36 || bytes4(reason) != selector) {
            return 0;
        }
        // slither-disable-next-line assembly
        assembly {
            amountOut := mload(add(reason, 36))
        }
    }

    /// @dev Runs the tagged protocol, which pays or forwards to `swap_.receiver`. No output
    /// measurement here: the Dispatcher's balance-diff at the receiver is the single source of
    /// truth.
    function _executeFallback(Swap calldata swap_, bytes calldata fallbackSwap)
        internal
        returns (FallbackProtocol protocol)
    {
        bytes calldata protocolData;
        (protocol, protocolData) = _decodeFallback(fallbackSwap);

        if (protocol == FallbackProtocol.UniswapV2) {
            _swapUniswapV2(swap_, protocolData);
        } else if (protocol == FallbackProtocol.UniswapV3) {
            _swapUniswapV3(swap_, protocolData);
        } else if (protocol == FallbackProtocol.UniswapV4) {
            _swapUniswapV4(swap_, protocolData);
        } else if (protocol == FallbackProtocol.Curve) {
            _swapCurve(swap_, protocolData);
        } else {
            _swapFluidV1(swap_, protocolData);
        }
    }

    function _decodeFallback(bytes calldata fallbackSwap)
        internal
        pure
        returns (FallbackProtocol protocol, bytes calldata protocolData)
    {
        if (fallbackSwap.length == 0) {
            revert TychoFallbackRouter__InvalidSwapLength(fallbackSwap.length);
        }

        uint8 protocolByte = uint8(fallbackSwap[0]);
        if (protocolByte > uint8(type(FallbackProtocol).max)) {
            revert TychoFallbackRouter__UnknownProtocol(protocolByte);
        }
        protocol = FallbackProtocol(protocolByte);
        protocolData = fallbackSwap[1:];
    }

    /// @dev Uniswap V2's `swap` takes explicit output amounts, so this computes the output from
    /// the reserves.
    function _swapUniswapV2(Swap calldata swap_, bytes calldata data) internal {
        (IUniswapV2Pair pair, uint256 feeBps) = _decodeUniswapV2(data);
        bool zeroForOne = swap_.tokenIn < swap_.tokenOut;
        uint256 calculatedAmount = _uniswapV2AmountOut(swap_, pair, feeBps);

        IERC20(swap_.tokenIn).safeTransfer(address(pair), swap_.amountIn);
        if (zeroForOne) {
            pair.swap(0, calculatedAmount, swap_.receiver, "");
        } else {
            pair.swap(calculatedAmount, 0, swap_.receiver, "");
        }
    }

    function _uniswapV2AmountOut(
        Swap calldata swap_,
        IUniswapV2Pair pair,
        uint256 feeBps
    ) internal view returns (uint256 amountOut) {
        bool zeroForOne = swap_.tokenIn < swap_.tokenOut;
        // slither-disable-next-line unused-return
        (uint112 reserve0, uint112 reserve1,) = pair.getReserves();
        return UniswapV2Math.getAmountOut(
            swap_.amountIn,
            zeroForOne ? reserve0 : reserve1,
            zeroForOne ? reserve1 : reserve0,
            feeBps
        );
    }

    function _decodeUniswapV2(bytes calldata data)
        internal
        pure
        returns (IUniswapV2Pair pair, uint256 feeBps)
    {
        if (data.length != 21) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }
        pair = IUniswapV2Pair(address(bytes20(data[0:20])));
        feeBps = uint8(data[20]);
        if (feeBps > 30) {
            revert TychoFallbackRouter__InvalidUniswapV2Fee(feeBps);
        }
    }

    function _swapUniswapV3(Swap calldata swap_, bytes calldata data) internal {
        address pool = _decodeUniswapV3(data);
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

    function _decodeUniswapV3(bytes calldata data)
        internal
        pure
        returns (address pool)
    {
        if (data.length != 20) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }
        pool = address(bytes20(data[0:20]));
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
        (address pool, uint8 poolType, uint256 i, uint256 j) =
            _decodeCurve(data);

        uint256 balanceBefore = IERC20(swap_.tokenOut).balanceOf(address(this));

        IERC20(swap_.tokenIn).forceApprove(pool, swap_.amountIn);
        if (CurvePoolType.isStable(poolType)) {
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

    function _decodeCurve(bytes calldata data)
        internal
        pure
        returns (address pool, uint8 poolType, uint256 i, uint256 j)
    {
        if (data.length != 23) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }
        pool = address(bytes20(data[0:20]));
        poolType = uint8(data[20]);
        i = uint8(data[21]);
        j = uint8(data[22]);
    }

    /// @dev `zero2one` is the dex's token order, not the address sort order, so it cannot be
    /// derived.
    function _swapFluidV1(Swap calldata swap_, bytes calldata data) internal {
        (address dex, bool zero2one) = _decodeFluidV1(data);

        _setCallbackContext(dex, swap_.tokenIn, swap_.amountIn);
        // slither-disable-next-line unused-return
        IFluidV1Dex(dex)
            .swapInWithCallback(zero2one, swap_.amountIn, 0, swap_.receiver);
        _clearCallbackContext();
    }

    function _decodeFluidV1(bytes calldata data)
        internal
        pure
        returns (address dex, bool zero2one)
    {
        if (data.length != 21) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }
        dex = address(bytes20(data[0:20]));
        zero2one = uint8(data[20]) > 0;
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

    /// @notice Runs the Uniswap V4 swap inside the PoolManager's unlock: decodes `data`, pays the
    /// encoded `amountIn`, swaps the single pool the encoded protocol data names, and sends the
    /// output to the encoded receiver.
    /// @dev The pool key's currencies come from the sort order of the encoded `tokenIn` and
    /// `tokenOut`, so the protocol data carries no direction.
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
        // there; a zero delta fails the route-level minAmountOut like any other fallback
        // that pays nothing.
        poolManager.take(
            Currency.wrap(swap_.tokenOut),
            swap_.receiver,
            uint256(uint128(amountOut))
        );
        return "";
    }

    function _requireSelf() internal view {
        if (msg.sender != address(this)) {
            revert TychoFallbackRouter__NotSelf();
        }
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

    /// @dev Rejects a `msg.sender` that is not the stored source, and clears the context so one
    /// callback cannot pay twice.
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
