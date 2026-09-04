// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {AccessControl} from "@openzeppelin/contracts/access/AccessControl.sol";
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
import {ITychoFallbackRouter} from "@interfaces/ITychoFallbackRouter.sol";

error TychoFallbackRouter__AddressZero();
error TychoFallbackRouter__InvalidCallback();
error TychoFallbackRouter__InvalidSwapLength(uint256 length);
error TychoFallbackRouter__InvalidUniswapV2Fee(uint256 feeBps);
error TychoFallbackRouter__NoOutput();
error TychoFallbackRouter__NotPoolManager();
error TychoFallbackRouter__NotSelf();
error TychoFallbackRouter__UnknownVenue(uint8 venue);
error TychoFallbackRouter__ZeroReserves();

interface ICurveCryptoPool {
    function exchange(uint256 i, uint256 j, uint256 dx, uint256 minDy)
        external
        payable;
}

interface ICurveStablePool {
    function exchange(int128 i, int128 j, uint256 dx, uint256 minDy)
        external
        payable;
}

interface IFluidV1Dex {
    function swapInWithCallback(
        bool swap0to1_,
        uint256 amountIn_,
        uint256 amountOutMin_,
        address to_
    ) external payable returns (uint256 amountOut_);
}

/// @title TychoFallbackRouter
/// @notice Runs a primary venue and, only if it fails, the caller's chosen fallback venue.
/// The primary is always a pAMM; the fallback never is.
/// @dev Exists because an executor cannot fall back: the Dispatcher transfers a leg's input before
/// it delegatecalls `swap()`, so a reverting pAMM has already been paid and a Uniswap V3 retry,
/// which pays in a callback, cannot be funded. Here the tokens stay in this contract.
///
/// Holds no funds and grants no allowances between transactions. Native ETH unsupported.
contract TychoFallbackRouter is
    ITychoFallbackRouter,
    AccessControl,
    ReentrancyGuardTransient
{
    using SafeERC20 for IERC20;

    enum Venue {
        UniswapV2,
        UniswapV3,
        UniswapV4,
        Curve,
        FluidV1
    }

    struct UniswapV4Swap {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        address receiver;
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

    uint160 private constant _MIN_SQRT_RATIO = 4295128739;
    uint160 private constant _MAX_SQRT_RATIO =
        1461446703485210103287273052203988822378723970342;

    IPoolManager public immutable poolManager;
    /// @notice Where `dexCallback` pays a Fluid dex.
    address public immutable fluidLiquidity;

    event Rescued(
        address indexed token, address indexed receiver, uint256 amount
    );

    constructor(
        address admin,
        IPoolManager poolManager_,
        address fluidLiquidity_
    ) {
        if (
            admin == address(0) || address(poolManager_) == address(0)
                || fluidLiquidity_ == address(0)
        ) {
            revert TychoFallbackRouter__AddressZero();
        }
        _grantRole(DEFAULT_ADMIN_ROLE, admin);
        poolManager = poolManager_;
        fluidLiquidity = fluidLiquidity_;
    }

    /// @inheritdoc ITychoFallbackRouter
    function swap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address receiver,
        address pamm,
        bytes calldata fallbackSwap
    ) external nonReentrant returns (uint256 amountOut) {
        // The try/catch is what unwinds the pAMM's transfer. Only the pAMM gets one: the fallback
        // is the caller's chosen venue, so its revert is the swap's revert.
        // slither-disable-next-line reentrancy-events
        try this.executePropAMM(
            tokenIn, tokenOut, amountIn, receiver, pamm
        ) returns (
            uint256 pammAmountOut
        ) {
            return pammAmountOut;
        } catch {}

        return
            _executeFallback(
                tokenIn, tokenOut, amountIn, receiver, fallbackSwap
            );
    }

    /// @notice Runs the pAMM. External only so `swap` can try/catch it.
    function executePropAMM(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address receiver,
        address pamm
    ) external returns (uint256 amountOut) {
        if (msg.sender != address(this)) {
            revert TychoFallbackRouter__NotSelf();
        }
        uint256 balanceBefore = IERC20(tokenOut).balanceOf(receiver);

        // Push-payment, so the transfer comes first.
        IERC20(tokenIn).safeTransfer(pamm, amountIn);
        // slither-disable-next-line unused-return
        IPropAMM(pamm)
            .swap(tokenIn, tokenOut, amountIn, 0, receiver, block.timestamp);

        return _delivered(tokenOut, receiver, balanceBefore);
    }

    /// @dev The fallback is venue-tagged; a pAMM is not among the kinds, so the venue the primary
    /// slot exists to retry can never also be the rescue.
    function _executeFallback(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address receiver,
        bytes calldata encodedSwap
    ) internal returns (uint256 amountOut) {
        if (encodedSwap.length == 0) {
            revert TychoFallbackRouter__InvalidSwapLength(encodedSwap.length);
        }

        uint256 balanceBefore = IERC20(tokenOut).balanceOf(receiver);

        uint8 venue = uint8(encodedSwap[0]);
        bytes calldata venueData = encodedSwap[1:];

        if (venue == uint8(Venue.UniswapV2)) {
            _swapUniswapV2(tokenIn, tokenOut, amountIn, receiver, venueData);
        } else if (venue == uint8(Venue.UniswapV3)) {
            _swapUniswapV3(tokenIn, tokenOut, amountIn, receiver, venueData);
        } else if (venue == uint8(Venue.UniswapV4)) {
            _swapUniswapV4(tokenIn, tokenOut, amountIn, receiver, venueData);
        } else if (venue == uint8(Venue.Curve)) {
            _swapCurve(tokenIn, tokenOut, amountIn, receiver, venueData);
        } else if (venue == uint8(Venue.FluidV1)) {
            _swapFluidV1(tokenIn, amountIn, receiver, venueData);
        } else {
            revert TychoFallbackRouter__UnknownVenue(venue);
        }

        return _delivered(tokenOut, receiver, balanceBefore);
    }

    /// @dev Zero delivered counts as a failure, so a venue that fills with nothing still falls
    /// through to the fallback.
    function _delivered(
        address tokenOut,
        address receiver,
        uint256 balanceBefore
    ) internal view returns (uint256 amountOut) {
        amountOut = IERC20(tokenOut).balanceOf(receiver) - balanceBefore;
        if (amountOut == 0) revert TychoFallbackRouter__NoOutput();
    }

    /// @notice Sends out a balance a Curve exchange rounded into this contract.
    function rescue(address token, address receiver, uint256 amount)
        external
        onlyRole(DEFAULT_ADMIN_ROLE)
    {
        if (receiver == address(0)) {
            revert TychoFallbackRouter__AddressZero();
        }
        emit Rescued(token, receiver, amount);
        IERC20(token).safeTransfer(receiver, amount);
    }

    /// @dev Venue data: `[pair: 20][feeBps: 1]`. The pair prices nothing, so the output amount
    /// comes from the reserves.
    function _swapUniswapV2(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address receiver,
        bytes calldata data
    ) internal {
        if (data.length != 21) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }
        IUniswapV2Pair pair = IUniswapV2Pair(address(bytes20(data[0:20])));
        uint256 feeBps = uint8(data[20]);
        if (feeBps > 30) {
            revert TychoFallbackRouter__InvalidUniswapV2Fee(feeBps);
        }

        bool zeroForOne = tokenIn < tokenOut;
        // slither-disable-next-line unused-return
        (uint112 reserve0, uint112 reserve1,) = pair.getReserves();
        uint256 calculatedAmount = _getAmountOut(
            amountIn,
            zeroForOne ? reserve0 : reserve1,
            zeroForOne ? reserve1 : reserve0,
            feeBps
        );

        IERC20(tokenIn).safeTransfer(address(pair), amountIn);
        if (zeroForOne) {
            pair.swap(0, calculatedAmount, receiver, "");
        } else {
            pair.swap(calculatedAmount, 0, receiver, "");
        }
    }

    /// @dev Venue data: `[pool: 20]`.
    function _swapUniswapV3(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address receiver,
        bytes calldata data
    ) internal {
        if (data.length != 20) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }
        address pool = address(bytes20(data[0:20]));
        bool zeroForOne = tokenIn < tokenOut;

        _setCallbackContext(pool, tokenIn, amountIn);
        // slither-disable-next-line unused-return
        IUniswapV3Pool(pool)
            .swap(
                receiver,
                zeroForOne,
                int256(amountIn),
                zeroForOne ? _MIN_SQRT_RATIO + 1 : _MAX_SQRT_RATIO - 1,
                ""
            );
        _setCallbackContext(address(0), address(0), 0);
    }

    /// @dev Venue data: `[fee: 3][tickSpacing: 3][hook: 20][hookData: rest]`. One pool, never a
    /// path: the currencies come from the sort order of `tokenIn` and `tokenOut`.
    function _swapUniswapV4(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address receiver,
        bytes calldata data
    ) internal {
        if (data.length < 26) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }

        UniswapV4Swap memory v4Swap = UniswapV4Swap({
            tokenIn: tokenIn,
            tokenOut: tokenOut,
            amountIn: amountIn,
            receiver: receiver,
            fee: uint24(bytes3(data[0:3])),
            tickSpacing: int24(uint24(bytes3(data[3:6]))),
            hook: address(bytes20(data[6:26])),
            hookData: data[26:]
        });

        // slither-disable-next-line unused-return
        poolManager.unlock(abi.encode(v4Swap));
    }

    /// @dev Venue data: `[pool: 20][poolType: 1][i: 1][j: 1]`. Curve pays the caller, so this
    /// forwards to `receiver`.
    function _swapCurve(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        address receiver,
        bytes calldata data
    ) internal {
        if (data.length != 23) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }
        address pool = address(bytes20(data[0:20]));
        uint8 poolType = uint8(data[20]);
        uint256 i = uint8(data[21]);
        uint256 j = uint8(data[22]);

        uint256 balanceBefore = IERC20(tokenOut).balanceOf(address(this));

        IERC20(tokenIn).forceApprove(pool, amountIn);
        if (poolType == 1 || poolType == 10) {
            // stable and stable_ng
            ICurveStablePool(pool)
                .exchange(int128(uint128(i)), int128(uint128(j)), amountIn, 0);
        } else {
            // crypto or llamma
            ICurveCryptoPool(pool).exchange(i, j, amountIn, 0);
        }
        IERC20(tokenIn).forceApprove(pool, 0);

        uint256 received =
            IERC20(tokenOut).balanceOf(address(this)) - balanceBefore;
        IERC20(tokenOut).safeTransfer(receiver, received);
    }

    /// @dev Venue data: `[dex: 20][zero2one: 1]`. `zero2one` is the dex's token order, not the
    /// address sort order, so it cannot be derived.
    function _swapFluidV1(
        address tokenIn,
        uint256 amountIn,
        address receiver,
        bytes calldata data
    ) internal {
        if (data.length != 21) {
            revert TychoFallbackRouter__InvalidSwapLength(data.length);
        }
        address dex = address(bytes20(data[0:20]));
        bool zero2one = uint8(data[20]) > 0;

        _setCallbackContext(dex, tokenIn, amountIn);
        // slither-disable-next-line unused-return
        IFluidV1Dex(dex).swapInWithCallback(zero2one, amountIn, 0, receiver);
        _setCallbackContext(address(0), address(0), 0);
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

    /// @notice Pays the Fluid liquidity layer. The dex's arguments are ignored, as above.
    function dexCallback(
        address, /* token_ */
        uint256 /* amount_ */
    )
        external
    {
        (address tokenIn, uint256 amountIn) = _consumeCallbackContext();
        IERC20(tokenIn).safeTransfer(fluidLiquidity, amountIn);
    }

    function unlockCallback(bytes calldata data)
        external
        returns (bytes memory)
    {
        if (msg.sender != address(poolManager)) {
            revert TychoFallbackRouter__NotPoolManager();
        }
        UniswapV4Swap memory v4Swap = abi.decode(data, (UniswapV4Swap));
        bool zeroForOne = v4Swap.tokenIn < v4Swap.tokenOut;

        PoolKey memory key = PoolKey({
            currency0: Currency.wrap(
                zeroForOne ? v4Swap.tokenIn : v4Swap.tokenOut
            ),
            currency1: Currency.wrap(
                zeroForOne ? v4Swap.tokenOut : v4Swap.tokenIn
            ),
            fee: v4Swap.fee,
            tickSpacing: v4Swap.tickSpacing,
            hooks: IHooks(v4Swap.hook)
        });

        poolManager.sync(Currency.wrap(v4Swap.tokenIn));
        IERC20(v4Swap.tokenIn)
            .safeTransfer(address(poolManager), v4Swap.amountIn);
        // slither-disable-next-line unused-return
        poolManager.settle();

        BalanceDelta delta = poolManager.swap(
            key,
            SwapParams(
                zeroForOne,
                -int256(v4Swap.amountIn),
                zeroForOne
                    ? TickMath.MIN_SQRT_PRICE + 1
                    : TickMath.MAX_SQRT_PRICE - 1
            ),
            v4Swap.hookData
        );

        int128 amountOut = zeroForOne ? delta.amount1() : delta.amount0();
        if (amountOut <= 0) revert TychoFallbackRouter__NoOutput();
        poolManager.take(
            Currency.wrap(v4Swap.tokenOut),
            v4Swap.receiver,
            uint256(uint128(amountOut))
        );
        return "";
    }

    function _getAmountOut(
        uint256 amountIn,
        uint112 reserveIn,
        uint112 reserveOut,
        uint256 feeBps
    ) internal pure returns (uint256 amount) {
        if (reserveIn == 0 || reserveOut == 0) {
            revert TychoFallbackRouter__ZeroReserves();
        }
        uint256 amountInWithFee = amountIn * (10000 - feeBps);
        uint256 numerator = amountInWithFee * uint256(reserveOut);
        uint256 denominator = (uint256(reserveIn) * 10000) + amountInWithFee;
        amount = numerator / denominator;
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
        if (msg.sender != source || source == address(0)) {
            revert TychoFallbackRouter__InvalidCallback();
        }
    }
}
