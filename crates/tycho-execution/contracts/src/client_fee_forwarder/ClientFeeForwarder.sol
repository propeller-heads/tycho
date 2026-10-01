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

/**
 * @title ClientFeeForwarder
 * @notice Calls TychoRouterV3 with a fixed client fee and transfers the fee to
 *         the client's wallet in the same transaction.
 * @dev This contract is the `clientFeeReceiver`, so it is also the client
 *      address that the FeeCalculator sees. The router credits the client fee
 *      to this contract's vault balance. The contract then withdraws that
 *      balance and transfers it to `feeWallet`.
 *
 *      The router verifies the client fee through ERC-1271. `isValidSignature`
 *      accepts only while this contract's own router call runs, so no other
 *      caller can use this contract as their `clientFeeReceiver`.
 *
 *      The caller funds the swap with ERC20 `transferFrom` or with native ETH.
 *      Permit2 and vault funding are not supported: the router takes the input
 *      from this contract, which holds no vault balance between transactions.
 */
contract ClientFeeForwarder is IERC1271, ReentrancyGuardTransient {
    using SafeERC20 for IERC20;

    TychoRouterV3 public immutable router;
    address public immutable feeWallet;
    // In FeeCalculator units: 100_000_000 = 100%
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

    /**
     * @notice Runs `TychoRouterV3.singleSwap` with this contract's client fee.
     * @dev Takes `amountIn` of `tokenIn` from the caller. Send `amountIn` as
     *      `msg.value` when `tokenIn` is `ETH_ADDRESS`.
     * @return amountOut The output amount the router sent to `receiver`.
     */
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

    /**
     * @notice Runs `TychoRouterV3.sequentialSwap` with this contract's client
     *         fee.
     * @dev Takes `amountIn` of `tokenIn` from the caller. Send `amountIn` as
     *      `msg.value` when `tokenIn` is `ETH_ADDRESS`.
     * @return amountOut The output amount the router sent to `receiver`.
     */
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

    /**
     * @notice Runs `TychoRouterV3.splitSwap` with this contract's client fee.
     * @dev Takes `amountIn` of `tokenIn` from the caller. Send `amountIn` as
     *      `msg.value` when `tokenIn` is `ETH_ADDRESS`.
     * @return amountOut The output amount the router sent to `receiver`.
     */
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

    /**
     * @notice Accepts the router's client fee check while this contract's own
     *         router call runs, and rejects it otherwise.
     * @dev The router is `nonReentrant`, so the only signature check during
     *      this contract's router call is the one for that call.
     */
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

    /**
     * @dev The router sends native ETH here when it withdraws an ETH fee.
     */
    receive() external payable {
        if (msg.sender != address(router)) {
            revert ClientFeeForwarder__UnexpectedSender(msg.sender);
        }
    }

    function _beforeSwap(address tokenIn, uint256 amountIn, address receiver)
        private
    {
        // With the router as receiver, the router credits the output to this
        // contract's vault balance, and `_afterSwap` sends it to `feeWallet`.
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
