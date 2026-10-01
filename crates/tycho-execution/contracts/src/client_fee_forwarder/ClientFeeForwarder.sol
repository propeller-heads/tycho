// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {IERC1271} from "@openzeppelin/contracts/interfaces/IERC1271.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {
    ReentrancyGuardTransient
} from "@openzeppelin/contracts/utils/ReentrancyGuardTransient.sol";
import {TychoRouterV3, ClientFeeParams} from "../TychoRouterV3.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

error ClientFeeForwarder__AddressZero();
error ClientFeeForwarder__InvalidReceiver(address receiver);
error ClientFeeForwarder__UnexpectedSender(address sender);

/// @title ClientFeeForwarder
/// @notice Charges a client fee for clients without a signing EOA and sends
///         it to the caller's chosen fee wallet in the same transaction.
/// @dev See "Client fee forwarder" in crates/tycho-execution/CLAUDE.md.
contract ClientFeeForwarder is IERC1271, ReentrancyGuardTransient {
    using SafeERC20 for IERC20;

    TychoRouterV3 public immutable router;

    bool private transient _swapping;

    event ClientFeeForwarded(
        address indexed token, uint256 amount, address indexed feeWallet
    );

    constructor(address router_) {
        if (router_ == address(0)) revert ClientFeeForwarder__AddressZero();
        router = TychoRouterV3(payable(router_));
    }

    /// @notice For native ETH input, send `amountIn` as `msg.value`.
    // _afterSwap writes after the router call; safe because of nonReentrant
    // slither-disable-next-line reentrancy-benign
    function singleSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        address feeWallet,
        uint32 clientFeeBps,
        bytes calldata swapData
    ) external payable nonReentrant returns (uint256 amountOut) {
        _beforeSwap(tokenIn, amountIn, receiver, feeWallet);
        amountOut = router.singleSwap{value: msg.value}(
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
            minAmountOut,
            receiver,
            _clientFeeParams(clientFeeBps),
            swapData
        );
        _afterSwap(tokenOut, feeWallet);
    }

    // _afterSwap writes after the router call; safe because of nonReentrant
    // slither-disable-next-line reentrancy-benign
    function sequentialSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        address feeWallet,
        uint32 clientFeeBps,
        bytes calldata swaps
    ) external payable nonReentrant returns (uint256 amountOut) {
        _beforeSwap(tokenIn, amountIn, receiver, feeWallet);
        amountOut = router.sequentialSwap{value: msg.value}(
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
            minAmountOut,
            receiver,
            _clientFeeParams(clientFeeBps),
            swaps
        );
        _afterSwap(tokenOut, feeWallet);
    }

    // _afterSwap writes after the router call; safe because of nonReentrant
    // slither-disable-next-line reentrancy-benign
    function splitSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        uint256 nTokens,
        address receiver,
        address feeWallet,
        uint32 clientFeeBps,
        bytes calldata swaps
    ) external payable nonReentrant returns (uint256 amountOut) {
        _beforeSwap(tokenIn, amountIn, receiver, feeWallet);
        amountOut = router.splitSwap{value: msg.value}(
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
            minAmountOut,
            nTokens,
            receiver,
            _clientFeeParams(clientFeeBps),
            swaps
        );
        _afterSwap(tokenOut, feeWallet);
    }

    /// @notice Valid only during this contract's own router call.
    function isValidSignature(bytes32, bytes calldata)
        external
        view
        returns (bytes4)
    {
        if (msg.sender == address(router) && _swapping) {
            return IERC1271.isValidSignature.selector;
        }
        return 0xffffffff;
    }

    /// @dev Receives native ETH fees from `router.withdraw`.
    receive() external payable {
        if (msg.sender != address(router)) {
            revert ClientFeeForwarder__UnexpectedSender(msg.sender);
        }
    }

    function _beforeSwap(
        address tokenIn,
        uint256 amountIn,
        address receiver,
        address feeWallet
    ) private {
        if (feeWallet == address(0)) {
            revert ClientFeeForwarder__AddressZero();
        }
        // The router would credit the output to this contract's vault
        if (receiver == address(router) || receiver == address(this)) {
            revert ClientFeeForwarder__InvalidReceiver(receiver);
        }
        if (tokenIn != ETH_ADDRESS) {
            IERC20(tokenIn)
                .safeTransferFrom(msg.sender, address(this), amountIn);
            IERC20(tokenIn).forceApprove(address(router), amountIn);
        }
        _swapping = true;
    }

    function _afterSwap(address tokenOut, address feeWallet) private {
        _swapping = false;
        uint256 fee =
            router.balanceOf(address(this), uint256(uint160(tokenOut)));
        if (fee == 0) {
            return;
        }
        router.withdraw(tokenOut, fee);
        emit ClientFeeForwarded(tokenOut, fee, feeWallet);
        if (tokenOut == ETH_ADDRESS) {
            Address.sendValue(payable(feeWallet), fee);
        } else {
            IERC20(tokenOut).safeTransfer(feeWallet, fee);
        }
    }

    function _clientFeeParams(uint32 clientFeeBps)
        private
        view
        returns (ClientFeeParams memory)
    {
        return ClientFeeParams({
            clientFeeBps: clientFeeBps,
            clientFeeReceiver: address(this),
            maxClientContribution: 0,
            deadline: block.timestamp,
            clientSignature: ""
        });
    }
}
