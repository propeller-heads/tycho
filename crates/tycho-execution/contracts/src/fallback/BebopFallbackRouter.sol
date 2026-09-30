// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {TychoFallbackRouter} from "./TychoFallbackRouter.sol";

error BebopFallbackRouter__AddressZero();
error BebopFallbackRouter__InvalidDataLength(uint256 length);
error BebopFallbackRouter__InvalidSelector(bytes4 selector);
error BebopFallbackRouter__InvalidTarget(address target);

/// @title BebopFallbackRouter
/// @notice A `TychoFallbackRouter` whose primary is a signed Bebop order. The order MUST name
/// this contract as taker and receiver.
/// @dev The price is signed off-chain, so the order runs first and the fallback runs only when it
/// fails.
contract BebopFallbackRouter is TychoFallbackRouter {
    using SafeERC20 for IERC20;
    using Address for address;

    uint256 private constant _CALLDATA_START = 33;

    bytes4 private constant _SWAP_SINGLE_SELECTOR = 0x4dcebcba;
    bytes4 private constant _SWAP_AGGREGATE_SELECTOR = 0xa2f74893;
    bytes4 private constant _ROUTER_SWAP_SELECTOR = 0x9586d0e8;

    address public immutable bebopSettlement;
    address public immutable bebopRouter;

    constructor(
        IPoolManager poolManager_,
        address fluidLiquidity_,
        IUniswapV3StaticQuoter uniswapV3StaticQuoter_,
        address bebopSettlement_,
        address bebopRouter_
    )
        TychoFallbackRouter(
            poolManager_, fluidLiquidity_, uniswapV3StaticQuoter_
        )
    {
        if (bebopSettlement_ == address(0) || bebopRouter_ == address(0)) {
            revert BebopFallbackRouter__AddressZero();
        }
        bebopSettlement = bebopSettlement_;
        bebopRouter = bebopRouter_;
    }

    /// @notice Runs the Bebop order and, only if that fails, `fallbackSwap`.
    /// @dev The caller MUST transfer `swap_.amountIn` of `swap_.tokenIn` here first.
    function swap(
        Swap calldata swap_,
        address target,
        bytes calldata bebopData,
        bytes calldata fallbackSwap
    ) external {
        if (bebopData.length < _CALLDATA_START + 4) {
            revert BebopFallbackRouter__InvalidDataLength(bebopData.length);
        }
        _validateCall(
            target, bytes4(bebopData[_CALLDATA_START:_CALLDATA_START + 4])
        );
        _swap(swap_, target, bebopData, fallbackSwap);
    }

    /// @dev An `amountIn` above the signed amount goes to the fallback whole.
    function _quotePrimary(
        Swap calldata swap_,
        address, /* target */
        bytes calldata bebopData
    ) internal pure override returns (uint256 amountOut) {
        return swap_.amountIn > _originalFilledTakerAmount(bebopData)
            ? 0
            : PRIMARY_FIRST;
    }

    function _swapPrimary(
        Swap calldata swap_,
        address target,
        bytes calldata bebopData
    ) internal override {
        bytes memory bebopCalldata = _capFilledTakerAmount(
            bebopData[_CALLDATA_START:],
            swap_.amountIn,
            _originalFilledTakerAmount(bebopData),
            uint8(bebopData[0])
        );
        IERC20 tokenIn = IERC20(swap_.tokenIn);
        IERC20 tokenOut = IERC20(swap_.tokenOut);
        uint256 balanceBefore = tokenOut.balanceOf(address(this));

        tokenIn.forceApprove(target, swap_.amountIn);
        // slither-disable-next-line unused-return
        target.functionCall(bebopCalldata);
        tokenIn.forceApprove(target, 0);

        tokenOut.safeTransfer(
            swap_.receiver, tokenOut.balanceOf(address(this)) - balanceBefore
        );
    }

    function _validateCall(address target, bytes4 selector) internal view {
        if (target == bebopSettlement) {
            if (
                selector != _SWAP_SINGLE_SELECTOR
                    && selector != _SWAP_AGGREGATE_SELECTOR
            ) {
                revert BebopFallbackRouter__InvalidSelector(selector);
            }
        } else if (target == bebopRouter) {
            if (selector != _ROUTER_SWAP_SELECTOR) {
                revert BebopFallbackRouter__InvalidSelector(selector);
            }
        } else {
            revert BebopFallbackRouter__InvalidTarget(target);
        }
    }

    /// @dev Caps the calldata's `filledTakerAmount` at `amountIn`.
    function _capFilledTakerAmount(
        bytes memory bebopCalldata,
        uint256 amountIn,
        uint256 originalFilledTakerAmount,
        uint8 partialFillOffset
    ) internal pure returns (bytes memory) {
        if (amountIn >= originalFilledTakerAmount) {
            return bebopCalldata;
        }
        uint256 filledTakerAmountPos = 4 + uint256(partialFillOffset) * 32;
        // slither-disable-next-line assembly
        assembly {
            mstore(
                add(add(bebopCalldata, 0x20), filledTakerAmountPos),
                amountIn
            )
        }
        return bebopCalldata;
    }

    function _originalFilledTakerAmount(bytes calldata bebopData)
        internal
        pure
        returns (uint256)
    {
        return uint256(bytes32(bebopData[1:_CALLDATA_START]));
    }
}
