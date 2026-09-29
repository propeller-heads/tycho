// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {IPoolManager} from "@uniswap/v4-core/src/interfaces/IPoolManager.sol";
import {IUniswapV3StaticQuoter} from "@interfaces/IUniswapV3StaticQuoter.sol";
import {BebopCalldata} from "../../lib/BebopCalldata.sol";
import {TychoFallbackRouter} from "./TychoFallbackRouter.sol";

error BebopFallbackRouter__AddressZero();
error BebopFallbackRouter__InvalidDataLength(uint256 length);
error BebopFallbackRouter__InvalidSelector(bytes4 selector);
error BebopFallbackRouter__InvalidTarget(address target);

/// @title BebopFallbackRouter
/// @notice A `TychoFallbackRouter` whose primary venue is a signed Bebop PMM order, run through
/// the Bebop settlement or router contract.
/// @dev The order's price is signed off-chain, so there is nothing to compare on-chain: the order
/// runs first, and the fallback runs only when the order reverts, delivers nothing, or cannot take
/// the whole `amountIn`. The order MUST name this contract as taker and receiver.
///
/// The settlement pulls `tokenIn` with `transferFrom`, so this contract approves the target for
/// `amountIn` and revokes the approval after the call. The target and selector are checked
/// against the immutable settlement and router, since this contract is permissionless and holds
/// the leg's tokens. Bebop pays this contract, which forwards the output to the receiver.
contract BebopFallbackRouter is TychoFallbackRouter {
    using SafeERC20 for IERC20;
    using Address for address;

    /// @notice `[partialFillOffset: 1][originalFilledTakerAmount: 32]`, then the calldata.
    uint256 private constant _CALLDATA_START = 33;

    /// @notice The Bebop settlement contract.
    address public immutable bebopSettlement;
    /// @notice The Bebop router contract.
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

    /// @notice Runs the Bebop order and, only if that fails, `fallbackSwap`. A failing fallback
    /// reverts the swap; there is no third attempt.
    /// @dev Permissionless: the caller names every parameter, so a balance sitting in this
    /// contract can be taken by anyone and is considered lost. Push-payment: the caller MUST
    /// transfer `swap_.amountIn` of `swap_.tokenIn` here first. Native ETH, fee-on-transfer and
    /// rebasing tokens are not supported.
    /// @param target The Bebop settlement or router contract.
    /// @param bebopData `[partialFillOffset: 1][originalFilledTakerAmount: 32][calldata]`, where
    /// `calldata` is the Bebop API's transaction data for `target`.
    function swap(
        Swap calldata swap_,
        address target,
        bytes calldata bebopData,
        bytes calldata fallbackSwap
    ) external {
        if (bebopData.length < _CALLDATA_START + 4) {
            revert BebopFallbackRouter__InvalidDataLength(bebopData.length);
        }
        bytes4 selector = bytes4(bebopData[_CALLDATA_START:_CALLDATA_START + 4]);
        BebopCalldata.CallCheck check = BebopCalldata.checkCall(
            target, selector, bebopSettlement, bebopRouter
        );
        if (check == BebopCalldata.CallCheck.InvalidTarget) {
            revert BebopFallbackRouter__InvalidTarget(target);
        }
        if (check == BebopCalldata.CallCheck.InvalidSelector) {
            revert BebopFallbackRouter__InvalidSelector(selector);
        }
        _swap(swap_, target, bebopData, fallbackSwap);
    }

    /// @dev The order fills at most `originalFilledTakerAmount`, so a larger `amountIn` goes to
    /// the fallback whole instead of leaving the remainder here.
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
        bytes memory bebopCalldata =
            BebopCalldata.capFilledTakerAmount(
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

    function _originalFilledTakerAmount(bytes calldata bebopData)
        internal
        pure
        returns (uint256)
    {
        return uint256(bytes32(bebopData[1:_CALLDATA_START]));
    }
}
