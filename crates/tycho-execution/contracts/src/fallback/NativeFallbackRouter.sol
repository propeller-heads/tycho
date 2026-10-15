// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {TychoFallbackRouter} from "./TychoFallbackRouter.sol";

error NativeFallbackRouter__AddressZero();
error NativeFallbackRouter__InvalidAmountIn();
error NativeFallbackRouter__InvalidDataLength(uint256 length);
error NativeFallbackRouter__InvalidSelector(bytes4 selector);
error NativeFallbackRouter__InvalidTarget(address target);
error NativeFallbackRouter__UnexpectedOverride();

/// @title NativeFallbackRouter
/// @notice A `TychoFallbackRouter` whose primary is a signed Native RFQ-T quote. The quote MUST
/// name this contract as recipient.
/// @dev The price is signed off-chain, so the quote runs first and the fallback runs only when it
/// fails.
contract NativeFallbackRouter is TychoFallbackRouter {
    using SafeERC20 for IERC20;
    using Address for address;

    // `nativeData` is `[signedAmountIn: 32][tradeRFQT calldata]`.
    uint256 private constant _CALLDATA_START = 32;
    bytes4 private constant _TRADE_RFQT_SELECTOR = 0x7083527c;
    // tradeRFQT's selector, then three ABI head words: the quote offset, `actualSellerAmount`
    // and `actualMinOutputAmount`.
    uint256 private constant _ACTUAL_SELLER_AMOUNT_OFFSET = 4 + 32;
    uint256 private constant _ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET = 4 + 2 * 32;
    uint256 private constant _MIN_DATA_LENGTH = _CALLDATA_START + 4 + 3 * 32;

    address public immutable nativeRouterV6;

    constructor(
        IPoolManager poolManager_,
        address fluidLiquidity_,
        IUniswapV3StaticQuoter uniswapV3StaticQuoter_,
        address nativeRouterV6_
    )
        TychoFallbackRouter(
            poolManager_, fluidLiquidity_, uniswapV3StaticQuoter_
        )
    {
        if (nativeRouterV6_ == address(0)) {
            revert NativeFallbackRouter__AddressZero();
        }
        nativeRouterV6 = nativeRouterV6_;
    }

    /// @notice Runs the Native quote and, only if it fails, `fallbackSwap`. The quote trades all
    /// of `swap_.amountIn`; Native reverts when it strays too far from the signed amount, and the
    /// fallback then fills the leg.
    /// @dev The caller MUST transfer `swap_.amountIn` of `swap_.tokenIn` here first.
    function swap(
        Swap calldata swap_,
        address target,
        bytes calldata nativeData,
        bytes calldata fallbackSwap
    ) external {
        _validate(swap_.amountIn, target, nativeData);
        _swap(swap_, target, nativeData, fallbackSwap);
    }

    function _runsPrimaryFirst() internal pure override returns (bool) {
        return true;
    }

    function _swapPrimary(
        Swap calldata swap_,
        address target,
        bytes calldata nativeData
    ) internal override {
        bytes memory payload = nativeData[_CALLDATA_START:];
        // Native trades the signed amount when `actualSellerAmount` is zero, and scales the
        // signed minimum output to any other amount.
        if (swap_.amountIn != _signedAmountIn(nativeData)) {
            _setActualSellerAmount(payload, swap_.amountIn);
        }
        IERC20 tokenIn = IERC20(swap_.tokenIn);
        IERC20 tokenOut = IERC20(swap_.tokenOut);
        uint256 balanceBefore = tokenOut.balanceOf(address(this));

        tokenIn.forceApprove(target, swap_.amountIn);
        // slither-disable-next-line unused-return
        target.functionCall(payload);
        tokenIn.forceApprove(target, 0);

        tokenOut.safeTransfer(
            swap_.receiver, tokenOut.balanceOf(address(this)) - balanceBefore
        );
    }

    /// @dev Rejects data the primary must never run, rather than falling back on it.
    function _validate(
        uint256 amountIn,
        address target,
        bytes calldata nativeData
    ) internal view {
        if (nativeData.length < _MIN_DATA_LENGTH) {
            revert NativeFallbackRouter__InvalidDataLength(nativeData.length);
        }
        if (target != nativeRouterV6) {
            revert NativeFallbackRouter__InvalidTarget(target);
        }
        bytes4 selector =
            bytes4(nativeData[_CALLDATA_START:_CALLDATA_START + 4]);
        if (selector != _TRADE_RFQT_SELECTOR) {
            revert NativeFallbackRouter__InvalidSelector(selector);
        }
        // A zero `actualSellerAmount` means "the signed amount" to Native.
        if (amountIn == 0 || _signedAmountIn(nativeData) == 0) {
            revert NativeFallbackRouter__InvalidAmountIn();
        }
        // The only tradeRFQT arguments the maker does not sign.
        if (
            _calldataWord(nativeData, _ACTUAL_SELLER_AMOUNT_OFFSET) != 0
                || _calldataWord(nativeData, _ACTUAL_MIN_OUTPUT_AMOUNT_OFFSET)
                    != 0
        ) {
            revert NativeFallbackRouter__UnexpectedOverride();
        }
    }

    function _signedAmountIn(bytes calldata nativeData)
        internal
        pure
        returns (uint256)
    {
        return uint256(bytes32(nativeData[0:_CALLDATA_START]));
    }

    /// @dev The word at `offset` into the tradeRFQT calldata.
    function _calldataWord(bytes calldata nativeData, uint256 offset)
        internal
        pure
        returns (uint256)
    {
        uint256 start = _CALLDATA_START + offset;
        return uint256(bytes32(nativeData[start:start + 32]));
    }

    function _setActualSellerAmount(bytes memory payload, uint256 amountIn)
        internal
        pure
    {
        // `_validate` guarantees the word lies within `payload`.
        // slither-disable-next-line assembly
        assembly ("memory-safe") {
            mstore(
                add(add(payload, 0x20), _ACTUAL_SELLER_AMOUNT_OFFSET),
                amountIn
            )
        }
    }
}
