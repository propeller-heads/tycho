// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {
    LibPrefixLengthEncodedByteArray
} from "../lib/bytes/LibPrefixLengthEncodedByteArray.sol";

import {AccessControl} from "@openzeppelin/contracts/access/AccessControl.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {
    ReentrancyGuard
} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";
import {
    IAllowanceTransfer
} from "@permit2/src/interfaces/IAllowanceTransfer.sol";
import {ERC6909} from "@openzeppelin/contracts/token/ERC6909/ERC6909.sol";
import {EIP712} from "@openzeppelin/contracts/utils/cryptography/EIP712.sol";
import {ECDSA} from "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";
import {Dispatcher} from "./Dispatcher.sol";
import {LibSwap} from "../lib/LibSwap.sol";
import {TransferManager} from "./TransferManager.sol";
import {ETH_ADDRESS} from "../lib/NativeETH.sol";
import {FeeRecipient} from "../lib/FeeStructs.sol";

//                                         ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                                   ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                             ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                          ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                       ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷   ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                   ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷       ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                 ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷      ✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷      ✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//              ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷    ✷✷✷✷✷✷✷✷✷✷✷✷✷
//             ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷      ✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷       ✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//            ✷✷✷✷✷✷✷✷✷✷✷✷           ✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷     ✷✷✷✷✷✷✷✷✷         ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//            ✷✷✷✷✷✷✷✷✷✷✷✷                   ✷✷✷✷✷✷           ✷✷✷✷✷✷         ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//            ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷                                   ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//            ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷                  ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//            ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷                  ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//            ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷                                   ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//            ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷         ✷✷✷✷✷✷           ✷✷✷✷✷✷                   ✷✷✷✷✷✷✷✷✷✷✷✷
//            ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷         ✷✷✷✷✷✷✷✷✷     ✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷           ✷✷✷✷✷✷✷✷✷✷✷✷
//             ✷✷✷✷✷✷✷✷✷✷✷✷✷✷       ✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//              ✷✷✷✷✷✷✷✷✷✷✷✷✷    ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷      ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                 ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷      ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                   ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷      ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷    ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                       ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                          ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                             ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                                  ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//                                         ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//
//
//     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷   ✷✷✷✷✷✷       ✷✷✷✷✷✷       ✷✷✷✷✷✷✷         ✷✷✷✷✷✷      ✷✷✷✷✷✷         ✷✷✷✷✷✷✷
//     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷    ✷✷✷✷✷✷    ✷✷✷✷✷✷✷    ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷     ✷✷✷✷✷✷      ✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//           ✷✷✷✷✷✷           ✷✷✷✷✷✷ ✷✷✷✷✷✷     ✷✷✷✷✷✷     ✷✷✷✷✷✷✷   ✷✷✷✷✷✷      ✷✷✷✷✷✷    ✷✷✷✷✷✷     ✷✷✷✷✷✷✷
//           ✷✷✷✷✷✷            ✷✷✷✷✷✷✷✷✷✷      ✷✷✷✷✷✷✷               ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷   ✷✷✷✷✷✷✷      ✷✷✷✷✷✷
//           ✷✷✷✷✷✷              ✷✷✷✷✷✷✷        ✷✷✷✷✷✷      ✷✷✷✷✷✷   ✷✷✷✷✷✷      ✷✷✷✷✷✷    ✷✷✷✷✷✷      ✷✷✷✷✷✷
//           ✷✷✷✷✷✷               ✷✷✷✷✷          ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷    ✷✷✷✷✷✷      ✷✷✷✷✷✷     ✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷✷
//           ✷✷✷✷✷✷               ✷✷✷✷✷              ✷✷✷✷✷✷✷✷        ✷✷✷✷✷✷      ✷✷✷✷✷✷         ✷✷✷✷✷✷✷✷

error TychoRouter__AddressZero();
error TychoRouter__NotAContract(address addr);
error TychoRouter__EmptySwaps();
error TychoRouter__MsgValueDoesNotMatchAmountIn(
    uint256 msgValue, uint256 amountIn
);
error TychoRouter__NegativeSlippage(uint256 amount, uint256 minAmount);
error TychoRouter__InvalidDataLength();
error TychoRouter__UndefinedMinAmountOut();
error TychoRouter__InvalidClientSignature();
error TychoRouter__NegativeOutputDelta(int256 amount);
error TychoRouter__ExpiredClientSignature(
    uint256 deadline, uint256 blockTimestamp
);
error TychoRouter__ZeroInput();

struct ClientFeeParams {
    uint16 clientFeeBps;
    address clientFeeReceiver;
    uint256 maxClientContribution;
    uint256 deadline;
    bytes clientSignature; // 65-byte EIP-712 ECDSA sig by clientFeeReceiver
}

error TychoRouter__TimelockNotExpired(
    uint256 activationTimestamp, uint256 blockTimestamp
);
error TychoRouter__NoPendingFeeCalculator();

contract TychoRouter is AccessControl, Dispatcher, EIP712 {
    address private _feeCalculator; // Fee calculator contract
    address private _pendingFeeCalculator;
    uint48 private _feeCalculatorActivationTimestamp;

    using SafeERC20 for IERC20;
    using LibPrefixLengthEncodedByteArray for bytes;
    using LibSwap for bytes;

    //keccak256("NAME_OF_ROLE") : save gas on deployment
    bytes32 public constant EXECUTOR_SETTER_ROLE =
        0x6a1dd52dcad5bd732e45b6af4e7344fa284e2d7d4b23b5b09cb55d36b0685c87;
    bytes32 public constant PAUSER_ROLE =
        0x65d7a28e3265b37a6474929f336521b332c1681b933f6cb9f3376673440d862a;
    bytes32 public constant UNPAUSER_ROLE =
        0x427da25fe773164f88948d3e215c94b6554e2ed5e5f203a821c9f2f6131cf75a;
    bytes32 public constant ROUTER_FEE_SETTER_ROLE =
        0x9939157be7760e9462f1d5a0dcad88b616ddc64138e317108b40b1cf55601348;

    uint256 public constant DELAY_FEE_CALCULATOR_ACTIVATION = 3 days;

    bytes32 public constant CLIENT_FEE_TYPEHASH = keccak256(
        "ClientFee(uint16 clientFeeBps,address clientFeeReceiver,"
        "uint256 maxClientContribution,uint256 deadline,"
        "uint256 amountIn,address tokenIn,address tokenOut,"
        "uint256 minAmountOut,address receiver,bytes swaps)"
    );

    event Withdrawal(
        address indexed token, uint256 amount, address indexed receiver
    );
    event FeeCalculatorSet(
        address indexed feeCalculator, uint256 timelockExpiresAt
    );
    event FeeCalculatorActivated(
        address indexed oldCalculator, address indexed newCalculator
    );
    event FeesTaken(address indexed token, FeeRecipient[] fees);

    constructor(
        address permit2_,
        address feeCalculator,
        address pauserAdmin,
        address unpauserAdmin,
        address executorSetterAdmin,
        address routerFeeSetterAdmin
    ) Dispatcher(permit2_) EIP712("TychoRouter", "1") {
        if (feeCalculator.code.length == 0) {
            revert TychoRouter__NotAContract(feeCalculator);
        }
        _feeCalculator = feeCalculator;

        // Make each role its own admin so role holders can manage their own role
        _setRoleAdmin(PAUSER_ROLE, PAUSER_ROLE);
        _setRoleAdmin(UNPAUSER_ROLE, UNPAUSER_ROLE);
        _setRoleAdmin(EXECUTOR_SETTER_ROLE, EXECUTOR_SETTER_ROLE);
        _setRoleAdmin(ROUTER_FEE_SETTER_ROLE, ROUTER_FEE_SETTER_ROLE);

        // Grant initial roles - only these ones are admin of the corresponding role
        _grantRole(PAUSER_ROLE, pauserAdmin);
        _grantRole(UNPAUSER_ROLE, unpauserAdmin);
        _grantRole(EXECUTOR_SETTER_ROLE, executorSetterAdmin);
        _grantRole(ROUTER_FEE_SETTER_ROLE, routerFeeSetterAdmin);
    }

    /**
     * @notice Override supportsInterface to resolve conflict between AccessControl and ERC6909
     */
    function supportsInterface(bytes4 interfaceId)
        public
        view
        virtual
        override(AccessControl, ERC6909)
        returns (bool)
    {
        return AccessControl.supportsInterface(interfaceId)
            || ERC6909.supportsInterface(interfaceId);
    }

    /**
     * @notice Executes a swap operation based on a predefined swap graph, supporting internal token amount splits.
     *         This function enables multi-step swaps and validates the output amount against a user-specified minimum.
     *         Takes funds from the user's wallet using transferFrom.
     *
     * @dev
     * - Swaps are executed sequentially using the `_swap` function.
     * - Reverts with `TychoRouter__NegativeSlippage` if the output amount is less than `minAmountOut`
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param minAmountOut The minimum acceptable amount of the output token. Reverts if this condition is not met. This should always be set to avoid losing funds due to slippage.
     * @param nTokens The total number of tokens involved in the swap graph (used to initialize arrays for internal calculations).
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, deadline and signature.
     * @param swaps Encoded swap graph data containing details of each swap.
     *
     * @return amountOut The total amount of the output token received by the receiver.
     */
    function splitSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        uint256 nTokens,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) public payable whenNotPaused nonReentrant returns (uint256 amountOut) {
        _verifyClientSignature(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            swaps
        );
        _updateNativeDeltaAccounting(amountIn);
        _tstoreTransferFromInfo(tokenIn, amountIn, false, false);

        return _splitSwapChecked(
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            nTokens,
            receiver,
            clientFeeParams,
            swaps
        );
    }

    /**
     * @notice Executes a swap operation based on a predefined swap graph, supporting internal token amount splits.
     *         This function enables multi-step swaps and validates the output amount against a user-specified minimum.
     *         Takes funds from the user's vault balance.
     *
     * @dev
     * - Swaps are executed sequentially using the `_swap` function.
     * - Reverts with `TychoRouter__NegativeSlippage` if the output amount is less than `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param minAmountOut The minimum acceptable amount of the output token. Reverts if this condition is not met. This should always be set to avoid losing funds due to slippage.
     * @param nTokens The total number of tokens involved in the swap graph (used to initialize arrays for internal calculations).
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, deadline and signature.
     * @param swaps Encoded swap graph data containing details of each swap.
     *
     * @return amountOut The total amount of the output token received by the receiver.
     */
    function splitSwapUsingVault(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        uint256 nTokens,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) public whenNotPaused nonReentrant returns (uint256 amountOut) {
        _verifyClientSignature(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            swaps
        );
        _tstoreTransferFromInfo(tokenIn, amountIn, false, true);

        return _splitSwapChecked(
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            nTokens,
            receiver,
            clientFeeParams,
            swaps
        );
    }

    /**
     * @notice Executes a swap operation based on a predefined swap graph, supporting internal token amount splits.
     *         This function enables multi-step swaps and validates the output amount against a user-specified minimum.
     *
     * @dev
     * - For ERC20 tokens, Permit2 is used to approve and transfer tokens from the caller to the router.
     * - Swaps are executed sequentially using the `_swap` function.
     * - Reverts with `TychoRouter__NegativeSlippage` if the output amount is less than `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param minAmountOut The minimum acceptable amount of the output token. Reverts if this condition is not met. This should always be set to avoid losing funds due to slippage.
     * @param nTokens The total number of tokens involved in the swap graph (used to initialize arrays for internal calculations).
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, deadline and signature.
     * @param permitSingle A Permit2 structure containing token approval details for the input token.
     * @param signature A valid signature authorizing the Permit2 approval.
     * @param swaps Encoded swap graph data containing details of each swap.
     *
     * @return amountOut The total amount of the output token received by the receiver.
     */
    function splitSwapPermit2(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        uint256 nTokens,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        IAllowanceTransfer.PermitSingle calldata permitSingle,
        bytes calldata signature,
        bytes calldata swaps
    ) external whenNotPaused nonReentrant returns (uint256 amountOut) {
        _verifyClientSignature(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            swaps
        );
        // For native ETH, assume funds already in our router. Else, handle approval.
        if (tokenIn != ETH_ADDRESS) {
            permit2.permit(msg.sender, permitSingle, signature);
        }
        _tstoreTransferFromInfo(tokenIn, amountIn, true, false);

        return _splitSwapChecked(
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            nTokens,
            receiver,
            clientFeeParams,
            swaps
        );
    }

    /**
     * @notice Executes a swap operation based on a predefined swap graph with no split routes.
     *         This function enables multi-step swaps and validates the output amount against a user-specified minimum.
     *         Takes funds from the user's wallet using transferFrom.
     *
     * @dev
     * - Swaps are executed sequentially using the `_swap` function.
     * - Reverts with `TychoRouter__NegativeSlippage` if the output amount is less than `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param minAmountOut The minimum acceptable amount of the output token. Reverts if this condition is not met. This should always be set to avoid losing funds due to slippage.
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, deadline and signature.
     * @param swaps Encoded swap graph data containing details of each swap.
     *
     * @return amountOut The total amount of the output token received by the receiver.
     */
    function sequentialSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) public payable whenNotPaused nonReentrant returns (uint256 amountOut) {
        _verifyClientSignature(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            swaps
        );
        _updateNativeDeltaAccounting(amountIn);
        _tstoreTransferFromInfo(tokenIn, amountIn, false, false);

        return _sequentialSwapChecked(
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            clientFeeParams,
            swaps
        );
    }

    /**
     * @notice Executes a swap operation based on a predefined swap graph with no split routes.
     *         This function enables multi-step swaps and validates the output amount against a user-specified minimum.
     *         Takes funds from the user's vault balance.
     *
     * @dev
     * - Swaps are executed sequentially using the `_swap` function.
     * - Reverts with `TychoRouter__NegativeSlippage` if the output amount is less than `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param minAmountOut The minimum acceptable amount of the output token. Reverts if this condition is not met. This should always be set to avoid losing funds due to slippage.
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, deadline and signature.
     * @param swaps Encoded swap graph data containing details of each swap.
     *
     * @return amountOut The total amount of the output token received by the receiver.
     */
    function sequentialSwapUsingVault(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) public whenNotPaused nonReentrant returns (uint256 amountOut) {
        _verifyClientSignature(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            swaps
        );
        _tstoreTransferFromInfo(tokenIn, amountIn, false, true);

        return _sequentialSwapChecked(
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            clientFeeParams,
            swaps
        );
    }

    /**
     * @notice Executes a swap operation based on a predefined swap graph with no split routes.
     *         This function enables multi-step swaps and validates the output amount against a user-specified minimum.
     *
     * @dev
     * - For ERC20 tokens, Permit2 is used to approve and transfer tokens from the caller to the router.
     * - Reverts with `TychoRouter__NegativeSlippage` if the output amount is less than `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param minAmountOut The minimum acceptable amount of the output token. Reverts if this condition is not met. This should always be set to avoid losing funds due to slippage.
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, deadline and signature.
     * @param permitSingle A Permit2 structure containing token approval details for the input token.
     * @param signature A valid signature authorizing the Permit2 approval.
     * @param swaps Encoded swap graph data containing details of each swap.
     *
     * @return amountOut The total amount of the output token received by the receiver.
     */
    function sequentialSwapPermit2(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        IAllowanceTransfer.PermitSingle calldata permitSingle,
        bytes calldata signature,
        bytes calldata swaps
    ) external whenNotPaused nonReentrant returns (uint256 amountOut) {
        _verifyClientSignature(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            swaps
        );
        // For native ETH, assume funds already in our router. Else, handle approval.
        if (tokenIn != ETH_ADDRESS) {
            permit2.permit(msg.sender, permitSingle, signature);
        }

        _tstoreTransferFromInfo(tokenIn, amountIn, true, false);

        return _sequentialSwapChecked(
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            clientFeeParams,
            swaps
        );
    }

    /**
     * @notice Executes a single swap operation.
     *         This function validates the output amount against a user-specified minimum.
     *         Takes funds from the user's wallet using transferFrom.
     *
     * @dev
     * - Reverts with `TychoRouter__NegativeSlippage` if the output amount is less than `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param minAmountOut The minimum acceptable amount of the output token. Reverts if this condition is not met. This should always be set to avoid losing funds due to slippage.
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, deadline and signature.
     * @param swapData Encoded swap details.
     *
     * @return amountOut The total amount of the output token received by the receiver.
     */
    function singleSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swapData
    ) public payable whenNotPaused nonReentrant returns (uint256 amountOut) {
        _verifyClientSignature(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            swapData
        );
        _updateNativeDeltaAccounting(amountIn);
        _tstoreTransferFromInfo(tokenIn, amountIn, false, false);

        return _singleSwap(
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            clientFeeParams,
            swapData
        );
    }

    /**
     * @notice Executes a single swap operation.
     *         This function validates the output amount against a user-specified minimum.
     *         Takes funds from the user's vault balance.
     *
     * @dev
     * - Reverts with `TychoRouter__NegativeSlippage` if the output amount is less than `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param minAmountOut The minimum acceptable amount of the output token. Reverts if this condition is not met. This should always be set to avoid losing funds due to slippage.
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, deadline and signature.
     * @param swapData Encoded swap details.
     *
     * @return amountOut The total amount of the output token received by the receiver.
     */
    function singleSwapUsingVault(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swapData
    ) public whenNotPaused nonReentrant returns (uint256 amountOut) {
        _verifyClientSignature(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            swapData
        );
        _tstoreTransferFromInfo(tokenIn, amountIn, false, true);

        return _singleSwap(
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            clientFeeParams,
            swapData
        );
    }

    /**
     * @notice Executes a single swap operation.
     *         This function validates the output amount against a user-specified minimum.
     *
     * @dev
     * - For ERC20 tokens, Permit2 is used to approve and transfer tokens from the caller to the router.
     * - Reverts with `TychoRouter__NegativeSlippage` if the output amount is less than `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param minAmountOut The minimum acceptable amount of the output token. Reverts if this condition is not met. This should always be set to avoid losing funds due to slippage.
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, deadline and signature.
     * @param permitSingle A Permit2 structure containing token approval details for the input token.
     * @param signature A valid signature authorizing the Permit2 approval.
     * @param swapData Encoded swap details.
     *
     * @return amountOut The total amount of the output token received by the receiver.
     */
    function singleSwapPermit2(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        IAllowanceTransfer.PermitSingle calldata permitSingle,
        bytes calldata signature,
        bytes calldata swapData
    ) external whenNotPaused nonReentrant returns (uint256 amountOut) {
        _verifyClientSignature(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            swapData
        );
        // For native ETH, assume funds already in our router. Else, handle approval.
        if (tokenIn != ETH_ADDRESS) {
            permit2.permit(msg.sender, permitSingle, signature);
        }
        _tstoreTransferFromInfo(tokenIn, amountIn, true, false);

        return _singleSwap(
            amountIn,
            tokenIn,
            tokenOut,
            minAmountOut,
            receiver,
            clientFeeParams,
            swapData
        );
    }

    /**
     * @notice Internal implementation of the core swap logic shared between splitSwap() and splitSwapPermit2().
     *
     * @notice This function centralizes the swap execution logic.
     * @notice For detailed documentation on parameters and behavior, see the documentation for
     * splitSwap() and splitSwapPermit2() functions.
     *
     */
    // State writes in _takeFees after external calls are safe because all public entry points use nonReentrant modifier
    // slither-disable-next-line reentrancy-benign
    function _splitSwapChecked(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        uint256 nTokens,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) internal returns (uint256 amountOut) {
        if (amountIn == 0) {
            revert TychoRouter__ZeroInput();
        }
        if (
            receiver == address(0) || tokenIn == address(0)
                || tokenOut == address(0)
        ) {
            revert TychoRouter__AddressZero();
        }
        if (minAmountOut == 0) {
            revert TychoRouter__UndefinedMinAmountOut();
        }

        uint16 routerFeeOnOutputBps = _callGetEffectiveRouterFeeOnOutput(
            _feeCalculator, clientFeeParams.clientFeeReceiver
        );

        uint256 amountOutBeforeFees;
        {
            address finalReceiver = _determineFinalReceiver(
                receiver, clientFeeParams.clientFeeBps, routerFeeOnOutputBps
            );
            amountOutBeforeFees = _splitSwap(
                amountIn,
                nTokens,
                swaps,
                finalReceiver,
                tokenIn == tokenOut // isCyclical
            );
        }

        // Skip _takeFees call if no fees exist
        if (clientFeeParams.clientFeeBps == 0 && routerFeeOnOutputBps == 0) {
            amountOut = amountOutBeforeFees;
        } else {
            amountOut = _takeFees(
                tokenOut,
                amountOutBeforeFees,
                clientFeeParams.clientFeeBps,
                clientFeeParams.clientFeeReceiver
            );
        }

        amountOut = _maybeAddClientContribution(
            amountOut,
            minAmountOut,
            clientFeeParams.maxClientContribution,
            tokenOut,
            receiver,
            clientFeeParams.clientFeeReceiver
        );

        amountOut = _settleOutput(
            amountOut, minAmountOut, amountIn, tokenIn, tokenOut, receiver
        );
    }

    /**
     * @notice Internal implementation of the core swap logic shared between singleSwap() and singleSwapPermit2().
     *
     * @notice This function centralizes the swap execution logic.
     * @notice For detailed documentation on parameters and behavior, see the documentation for
     * singleSwap() and singleSwapPermit2() functions.
     *
     */
    // State writes in _takeFees after external calls are safe because all public entry points use nonReentrant modifier
    // slither-disable-next-line reentrancy-benign
    function _singleSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swap_
    ) internal returns (uint256 amountOut) {
        if (amountIn == 0) {
            revert TychoRouter__ZeroInput();
        }
        if (
            receiver == address(0) || tokenIn == address(0)
                || tokenOut == address(0)
        ) {
            revert TychoRouter__AddressZero();
        }
        if (minAmountOut == 0) {
            revert TychoRouter__UndefinedMinAmountOut();
        }

        (address executor, bytes calldata protocolData) =
            swap_.decodeSingleSwap();

        address client = clientFeeParams.clientFeeReceiver;
        // Get router fee once and pass it down to avoid duplicate external calls
        uint16 routerFeeOnOutputBps =
            _callGetEffectiveRouterFeeOnOutput(_feeCalculator, client);

        address finalReceiver = _determineFinalReceiver(
            receiver, clientFeeParams.clientFeeBps, routerFeeOnOutputBps
        );
        uint256 amountOutBeforeFees = _callSwapOnExecutor(
            executor, amountIn, protocolData, true, false, finalReceiver
        );

        // Skip _takeFees call if no fees exist
        if (clientFeeParams.clientFeeBps == 0 && routerFeeOnOutputBps == 0) {
            amountOut = amountOutBeforeFees;
        } else {
            amountOut = _takeFees(
                tokenOut,
                amountOutBeforeFees,
                clientFeeParams.clientFeeBps,
                client
            );
        }

        amountOut = _maybeAddClientContribution(
            amountOut,
            minAmountOut,
            clientFeeParams.maxClientContribution,
            tokenOut,
            receiver,
            client
        );

        amountOut = _settleOutput(
            amountOut, minAmountOut, amountIn, tokenIn, tokenOut, receiver
        );
    }

    /**
     * @notice Internal implementation of the core swap logic shared between sequentialSwap() and sequentialSwapPermit2().
     *
     * @notice This function centralizes the swap execution logic.
     * @notice For detailed documentation on parameters and behavior, see the documentation for
     * sequentialSwap() and sequentialSwapPermit2() functions.
     *
     */
    // State writes in _takeFees after external calls are safe because all public entry points use nonReentrant modifier
    // slither-disable-next-line reentrancy-benign
    function _sequentialSwapChecked(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) internal returns (uint256 amountOut) {
        if (amountIn == 0) {
            revert TychoRouter__ZeroInput();
        }
        if (
            receiver == address(0) || tokenIn == address(0)
                || tokenOut == address(0)
        ) {
            revert TychoRouter__AddressZero();
        }
        if (minAmountOut == 0) {
            revert TychoRouter__UndefinedMinAmountOut();
        }
        if (swaps.length == 0) {
            revert TychoRouter__EmptySwaps();
        }

        address client = clientFeeParams.clientFeeReceiver;
        // Get router fee once and pass it down to avoid duplicate external calls
        uint16 routerFeeOnOutputBps =
            _callGetEffectiveRouterFeeOnOutput(_feeCalculator, client);

        address finalReceiver = _determineFinalReceiver(
            receiver, clientFeeParams.clientFeeBps, routerFeeOnOutputBps
        );
        uint256 amountOutBeforeFees =
            _sequentialSwap(amountIn, swaps, finalReceiver);

        // Skip _takeFees call if no fees exist
        if (clientFeeParams.clientFeeBps == 0 && routerFeeOnOutputBps == 0) {
            amountOut = amountOutBeforeFees;
        } else {
            amountOut = _takeFees(
                tokenOut,
                amountOutBeforeFees,
                clientFeeParams.clientFeeBps,
                client
            );
        }

        amountOut = _maybeAddClientContribution(
            amountOut,
            minAmountOut,
            clientFeeParams.maxClientContribution,
            tokenOut,
            receiver,
            client
        );

        amountOut = _settleOutput(
            amountOut, minAmountOut, amountIn, tokenIn, tokenOut, receiver
        );
    }

    /**
     * @dev Transfers output tokens to receiver (or credits vault),
     *      finalizes transient deltas, and checks slippage.
     */
    function _settleOutput(
        uint256 amountOut,
        uint256 minAmountOut,
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        address receiver
    ) internal returns (uint256) {
        int256 outputDelta = _getDelta(tokenOut);
        if (outputDelta > 0) {
            _updateDeltaAccounting(tokenOut, -int256(amountOut));
            // out tokens are still in the Router and need to be sent to the final receiver
            // or credited to the vault
            if (receiver == address(this)) {
                _creditVault(msg.sender, tokenOut, amountOut);
            } else {
                // the amountOut might actually be lower at this point (if fee/rebasing token)
                amountOut = _transferOut(tokenOut, receiver, amountOut);
            }
        }

        _finalizeBalances(msg.sender, tokenIn, amountIn);

        // Check final amount to account for fee tokens or rebasing tokens
        if (amountOut < minAmountOut) {
            revert TychoRouter__NegativeSlippage(amountOut, minAmountOut);
        }

        return amountOut;
    }

    /**
     * @dev Executes sequential swaps as defined by the provided swap graph.
     *
     * This function processes a series of swaps encoded in the `swaps_` byte array. Each swap operation determines:
     * - The indices of the input and output tokens (via `tokenInIndex()` and `tokenOutIndex()`).
     * - The portion of the available amount to be used for the swap, indicated by the `split` value.
     *
     * Four important notes:
     * - The contract assumes that token indexes follow a specific order: the sell token is at index 0, followed by any
     *  intermediary tokens, and finally the buy token.
     * - A `split` value of 0 is interpreted as 100% of the available amount (i.e., the entire remaining balance).
     *  This means that in scenarios without explicit splits the value should be 0, and when splits are present,
     *  the last swap should also have a split value of 0.
     * - In case of cyclic swaps, the output token is the same as the input token.
     *  `cyclicSwapAmountOut` is used to track the amount of the output token, and is updated when
     *  the `tokenOutIndex` is 0.
     * - The receiver of the hop is chosen depending on the position:
     *     - if it's any other than not the last hops (to the token out), the receiver is address(this)
     *     - if it's the last hops, the receiver will be the one passed in the input arguments. Note that for regular
     * split swaps, checking that the `tokenOutIndex` is the last value is enough for this but for cyclical split swaps
     * we need to rely on the `isCyclical` passed from the outside.
     *
     * @param amountIn The initial amount of the sell token to be swapped.
     * @param nTokens The total number of tokens involved in the swap path, used to initialize arrays for internal tracking.
     * @param swaps_ Encoded swap graph data containing the details of each swap operation.
     * @param receiver The address of the receiver of the swap
     * @param isCyclical Bool to determine if the swap is cyclical or not (token in == token out)
     *
     * @return The total amount of the buy token obtained after all swaps have been executed.
     */
    function _splitSwap(
        uint256 amountIn,
        uint256 nTokens,
        bytes calldata swaps_,
        address receiver,
        bool isCyclical
    ) internal returns (uint256) {
        if (swaps_.length == 0) {
            revert TychoRouter__EmptySwaps();
        }

        uint256[] memory remainingAmounts = new uint256[](nTokens);
        uint256[] memory amounts = new uint256[](nTokens);
        uint256 cyclicSwapAmountOut = 0;
        amounts[0] = amountIn;
        remainingAmounts[0] = amountIn;

        while (swaps_.length > 0) {
            bytes calldata swapData;
            (swapData, swaps_) = swaps_.next();

            (
                uint8 tokenInIndex,
                uint8 tokenOutIndex,
                uint24 split,
                address executor,
                bytes calldata protocolData
            ) = swapData.decodeSplitSwap();

            uint256 currentAmountIn = split > 0
                ? (amounts[tokenInIndex] * split) / 0xffffff
                : remainingAmounts[tokenInIndex];

            address swapReceiver = address(this);
            if (
                (tokenOutIndex == nTokens - 1 && !isCyclical)
                    || (isCyclical && tokenOutIndex == 0)
            ) {
                swapReceiver = receiver;
            }

            uint256 currentAmountOut = _callSwapOnExecutor(
                executor,
                currentAmountIn,
                protocolData,
                tokenInIndex == 0,
                true,
                swapReceiver
            );
            // Checks if the output token is the same as the input token
            if (tokenOutIndex == 0) {
                cyclicSwapAmountOut += currentAmountOut;
            } else {
                amounts[tokenOutIndex] += currentAmountOut;
            }
            remainingAmounts[tokenOutIndex] += currentAmountOut;
            remainingAmounts[tokenInIndex] -= currentAmountIn;
        }
        // For cyclic routes the output token is at index 0; for regular routes
        // it is at the last index (nTokens - 1).
        return isCyclical ? cyclicSwapAmountOut : amounts[nTokens - 1];
    }

    /**
     * @dev Executes sequential swaps as defined by the provided swap graph.
     *
     * @param amountIn The initial amount of the sell token to be swapped.
     * @param swaps_ Encoded swap graph data containing the details of each swap operation.
     * @param finalReceiver Address of the receiver of the last swap.
     *
     * @return calculatedAmount The total amount of the buy token obtained after all swaps have been executed.
     */
    function _sequentialSwap(
        uint256 amountIn,
        bytes calldata swaps_,
        address finalReceiver
    ) internal returns (uint256 calculatedAmount) {
        calculatedAmount = amountIn;
        uint256 swapCount = swaps_.size();
        bytes calldata remainingSwaps = swaps_;

        for (uint256 i = 0; i < swapCount; i++) {
            bytes calldata currentSwap;
            (currentSwap, remainingSwaps) = remainingSwaps.next();

            (address executor, bytes calldata protocolData) =
                currentSwap.decodeSequentialSwap();

            address receiver;
            bool isLastSwap = (i == swapCount - 1);

            if (isLastSwap) {
                receiver = finalReceiver;
            } else {
                bytes calldata nextSwap;
                // slither-disable-next-line unused-return
                (nextSwap,) = remainingSwaps.next();
                (address nextExecutor, bytes calldata nextProtocolData) =
                    nextSwap.decodeSequentialSwap();
                receiver =
                    _callFundsExpectedAddress(nextExecutor, nextProtocolData);
            }

            calculatedAmount = _callSwapOnExecutor(
                executor,
                calculatedAmount,
                protocolData,
                i == 0, // isFirstSwap
                false,
                receiver
            );
        }
    }

    /**
     * @dev We use the fallback function to allow flexibility on callback.
     */
    fallback(bytes calldata data)
        external
        whenNotPaused
        returns (bytes memory)
    {
        return _callHandleCallbackOnExecutor(data, msg.sender);
    }

    /**
     * @dev Pauses the contract
     */
    function pause() external onlyRole(PAUSER_ROLE) {
        _pause();
    }

    /**
     * @dev Unpauses the contract
     */
    function unpause() external onlyRole(UNPAUSER_ROLE) {
        _unpause();
    }

    /**
     * @dev Entrypoint to add or replace an approved executor contract address
     * @param targets address of the executor contract
     */
    function setExecutors(address[] memory targets)
        external
        onlyRole(EXECUTOR_SETTER_ROLE)
        whenNotPaused
    {
        for (uint256 i = 0; i < targets.length; i++) {
            _setExecutor(targets[i]);
        }
    }

    /**
     * @dev Entrypoint to remove an approved executor contract address
     * @param target address of the executor contract
     */
    function removeExecutor(address target)
        external
        onlyRole(EXECUTOR_SETTER_ROLE)
    {
        _removeExecutor(target);
    }

    /**
     * @notice Queues a new fee calculator with a timelock delay.
     * @param feeCalculator The address of the fee calculator contract
     */
    function setFeeCalculator(address feeCalculator)
        external
        onlyRole(ROUTER_FEE_SETTER_ROLE)
        whenNotPaused
    {
        if (feeCalculator.code.length == 0) {
            revert TychoRouter__NotAContract(feeCalculator);
        }

        uint256 expiry = block.timestamp + DELAY_FEE_CALCULATOR_ACTIVATION;

        _pendingFeeCalculator = feeCalculator;
        _feeCalculatorActivationTimestamp = uint48(expiry);
        emit FeeCalculatorSet(feeCalculator, expiry);
    }

    /**
     * @dev Returns the current fee calculator address
     */
    function getFeeCalculator() external view returns (address) {
        return _feeCalculator;
    }

    /**
     * @dev Returns the pending fee calculator and activation timestamp
     */
    function getPendingFeeCalculator()
        external
        view
        returns (address, uint256)
    {
        return (_pendingFeeCalculator, _feeCalculatorActivationTimestamp);
    }

    /**
     * @notice Activates the pending fee calculator once the timelock has expired.
     */
    function activateFeeCalculator() external onlyRole(ROUTER_FEE_SETTER_ROLE) {
        uint48 activationTs = _feeCalculatorActivationTimestamp;
        // slither-disable-next-line incorrect-equality
        if (activationTs == 0) {
            revert TychoRouter__NoPendingFeeCalculator();
        }
        // slither-disable-next-line timestamp
        if (block.timestamp < activationTs) {
            revert TychoRouter__TimelockNotExpired(
                activationTs, block.timestamp
            );
        }
        address oldCalc = _feeCalculator;
        address pending = _pendingFeeCalculator;
        _feeCalculator = pending;
        _pendingFeeCalculator = address(0);
        _feeCalculatorActivationTimestamp = 0;
        emit FeeCalculatorActivated(oldCalc, pending);
    }

    /**
     * @notice Calculates and takes fees using the FeeCalculator contract
     * @param token The token address for which fees are being taken
     * @param amountIn The amount before fee deduction
     * @param clientFeeBps Client fee in basis points
     * @param client Address to receive client fees
     * @return amountOut The amount remaining after all fee deductions
     */
    function _takeFees(
        address token,
        uint256 amountIn,
        uint16 clientFeeBps,
        address client
    ) internal returns (uint256 amountOut) {
        FeeRecipient[] memory fees;
        (amountOut, fees) =
            _callCalculateFee(_feeCalculator, amountIn, clientFeeBps, client);

        for (uint256 i = 0; i < fees.length; i++) {
            if (fees[i].feeAmount > 0) {
                // We still need to update the delta accounting to ensure the funds are
                // in the router after the final swap and have not bypassed the router
                // due to incorrect or malicious encoding. Updating the delta
                // accounting without funds will result in an additional negative
                // delta, and cause the _finalizeBalances method to revert.
                _updateDeltaAccounting(token, -int256(fees[i].feeAmount));
                _creditVaultForFees(fees[i].recipient, token, fees[i].feeAmount);
            }
        }
        if (fees.length > 0) {
            emit FeesTaken(token, fees);
        }
    }

    /**
     * @dev Allows this contract to receive native token with empty msg.data from contracts
     */
    receive() external payable whenNotPaused {
        require(msg.sender.code.length != 0);
    }

    /**
     * @dev Updates delta accounting for native ETH received via msg.value
     * @notice This should be called at each entry point to credit the delta when ETH is sent
     */
    function _updateNativeDeltaAccounting(uint256 amountIn) internal {
        if (msg.value > 0) {
            // prevent unpredictable scenarios where the amountIn does not match exactly
            // what the caller sent
            if (msg.value != amountIn) {
                revert TychoRouter__MsgValueDoesNotMatchAmountIn(
                    msg.value, amountIn
                );
            }
            _updateDeltaAccounting(ETH_ADDRESS, int256(msg.value));
        }
    }

    /**
     * @dev If the amountOut is below the minAmountOut, it tries to add a client contribution (if within limits).
     * If it can't, it raises NegativeSlippage.
     *   - If the out tokens are still in the Tycho Router, it adds the contribution to the amount out
     *     (the transfer will be done later)
     *   - If the out tokens are already in the receiver, it transfers the contribution separately
     */
    function _maybeAddClientContribution(
        uint256 amountOut,
        uint256 minAmountOut,
        uint256 maxClientContribution,
        address tokenOut,
        address receiver,
        address client
    ) internal returns (uint256 amount) {
        if (amountOut < minAmountOut) {
            uint256 requiredContribution =
                minAmountOut - amountOut;
            if (requiredContribution > maxClientContribution) {
                revert TychoRouter__NegativeSlippage(amountOut, minAmountOut);
            }
            // Debit the client's vault balance
            _debitVault(client, tokenOut, requiredContribution);
            int256 outputDelta = _getDelta(tokenOut);
            if (outputDelta > 0) {
                // Output tokens are still in the Router. This could be because no
                // output transfer has been performed yet, or the user has specified the
                // receiver to be the router in order to rebalance their vault.
                _updateDeltaAccounting(tokenOut, int256(requiredContribution));
            } else if (outputDelta == 0) {
                if (receiver == address(this)) {
                    _creditVault(msg.sender, tokenOut, requiredContribution);
                } else if (tokenOut == ETH_ADDRESS) {
                    Address.sendValue(payable(receiver), requiredContribution);
                } else {
                    // Measure user balance before and after required contribution to
                    // account for fee tokens
                    uint256 balanceBefore = IERC20(tokenOut).balanceOf(receiver);
                    IERC20(tokenOut)
                        .safeTransfer(receiver, requiredContribution);
                    uint256 actualContribution =
                        IERC20(tokenOut).balanceOf(receiver) - balanceBefore;
                    return amountOut + actualContribution;
                }
            } else {
                // Negative output delta indicates unprofitable arbitrage.
                revert TychoRouter__NegativeOutputDelta(outputDelta);
            }
            amount = minAmountOut;
        } else {
            amount = amountOut;
        }
    }

    /**
     * @dev Determines the final receiver address for the last swap output tokens
     * @param receiver The receiver address
     * @param clientFeeBps Client fee in basis points
     * @param routerFeeOnOutputBps Router fee on output in basis points
     * @return The final receiver address - either the router (for fee processing) or the intended receiver
     */
    function _determineFinalReceiver(
        address receiver,
        uint16 clientFeeBps,
        uint16 routerFeeOnOutputBps
    ) internal view returns (address) {
        // Fast path: if no fees at all, send directly to receiver
        if (clientFeeBps == 0 && routerFeeOnOutputBps == 0) {
            return receiver;
        }
        // Fees exist, must route through this contract
        return address(this);
    }

    /**
     * @dev Verifies the client's EIP-712 signature over the fee parameters,
     *      the core swap parameters, and the encoded swap routing bytes.
     *      When clientFeeReceiver is address(0), no signature is required.
     * @param p The client fee parameters including the signature to verify.
     * @param amountIn The input token amount.
     * @param tokenIn The input token address.
     * @param tokenOut The output token address.
     * @param minAmountOut The minimum acceptable output amount.
     * @param receiver The address to receive the output tokens.
     * @param swapData The encoded swap routing data.
     */
    function _verifyClientSignature(
        ClientFeeParams calldata p,
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 minAmountOut,
        address receiver,
        bytes calldata swapData
    ) internal view {
        if (p.clientFeeReceiver == address(0)) {
            if (p.maxClientContribution > 0 || p.clientFeeBps > 0) {
                revert TychoRouter__AddressZero();
            }
            return;
        }
        // slither-disable-next-line timestamp
        if (block.timestamp > p.deadline) {
            revert TychoRouter__ExpiredClientSignature(
                p.deadline, block.timestamp
            );
        }
        bytes32 digest = _hashTypedDataV4(
            keccak256(
                abi.encode(
                    CLIENT_FEE_TYPEHASH,
                    p.clientFeeBps,
                    p.clientFeeReceiver,
                    p.maxClientContribution,
                    p.deadline,
                    amountIn,
                    tokenIn,
                    tokenOut,
                    minAmountOut,
                    receiver,
                    keccak256(swapData)
                )
            )
        );
        if (ECDSA.recover(digest, p.clientSignature) != p.clientFeeReceiver) {
            revert TychoRouter__InvalidClientSignature();
        }
    }
}
