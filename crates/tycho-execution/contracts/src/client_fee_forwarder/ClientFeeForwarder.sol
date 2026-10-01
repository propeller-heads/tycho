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
/// @notice Charges a client fee for a client without a signing EOA and sends
///         it to the client's wallet in the same transaction.
/// @dev See "Client fee forwarder" in crates/tycho-execution/CLAUDE.md.
contract ClientFeeForwarder is IERC1271, ReentrancyGuardTransient {
    using SafeERC20 for IERC20;

    TychoRouterV3 public immutable router;
    address public immutable feeWallet;
    uint32 public immutable clientFeeBps;

    bool private transient _swapping;

    event ClientFeeForwarded(
        address indexed token, uint256 amount, address indexed feeWallet
    );

    constructor(address router_, address feeWallet_, uint32 clientFeeBps_) {
        if (router_ == address(0) || feeWallet_ == address(0)) {
            revert ClientFeeForwarder__AddressZero();
        }
        router = TychoRouterV3(payable(router_));
        feeWallet = feeWallet_;
        clientFeeBps = clientFeeBps_;
    }

    /// @notice For native ETH input, send `amountIn` as `msg.value`.
    function singleSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        bytes calldata swapData
    ) external payable nonReentrant returns (uint256 amountOut) {
        _beforeSwap(tokenIn, amountIn, receiver);
        amountOut = router.singleSwap{value: msg.value}(
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
            minAmountOut,
            receiver,
            _clientFeeParams(),
            swapData
        );
        _afterSwap(tokenOut);
    }

    function sequentialSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        bytes calldata swaps
    ) external payable nonReentrant returns (uint256 amountOut) {
        _beforeSwap(tokenIn, amountIn, receiver);
        amountOut = router.sequentialSwap{value: msg.value}(
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
            minAmountOut,
            receiver,
            _clientFeeParams(),
            swaps
        );
        _afterSwap(tokenOut);
    }

    function splitSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        uint256 nTokens,
        address receiver,
        bytes calldata swaps
    ) external payable nonReentrant returns (uint256 amountOut) {
        _beforeSwap(tokenIn, amountIn, receiver);
        amountOut = router.splitSwap{value: msg.value}(
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
            minAmountOut,
            nTokens,
            receiver,
            _clientFeeParams(),
            swaps
        );
        _afterSwap(tokenOut);
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

    function _beforeSwap(address tokenIn, uint256 amountIn, address receiver)
        private
    {
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

    function _afterSwap(address tokenOut) private {
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

    function _clientFeeParams() private view returns (ClientFeeParams memory) {
        return ClientFeeParams({
            clientFeeBps: clientFeeBps,
            clientFeeReceiver: address(this),
            maxClientContribution: 0,
            deadline: block.timestamp,
            clientSignature: ""
        });
    }
}
