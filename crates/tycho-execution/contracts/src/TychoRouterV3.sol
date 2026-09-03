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
import {
    SignatureChecker
} from "@openzeppelin/contracts/utils/cryptography/SignatureChecker.sol";
import {Dispatcher} from "./Dispatcher.sol";
import {LibSwap} from "../lib/LibSwap.sol";
import {TransferManager} from "./TransferManager.sol";
import {ETH_ADDRESS} from "../lib/NativeETH.sol";
import {FeeRecipient, FeeInput} from "../lib/FeeStructs.sol";
import {IFeeCalculator} from "@interfaces/IFeeCalculator.sol";

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
error TychoRouter__AmountOutZero();
error TychoRouter__InvalidMinAmountOut(
    uint256 minAmountOut, uint256 expectedAmountOut
);
error TychoRouter__InvalidClientSignature();
error TychoRouter__NegativeOutputDelta(int256 amount);
error TychoRouter__ExpiredClientSignature(
    uint256 deadline, uint256 blockTimestamp
);
error TychoRouter__ZeroInput();
error TychoRouter__InvalidClientContributionNonce(
    address client, uint256 contributionNonce
);
error TychoRouter__NonZeroContributionNonce(uint256 contributionNonce);

struct ClientFeeParams {
    uint32 clientFeeBps;
    address clientFeeReceiver;
    uint256 maxClientContribution;
    // Single-use identifier for an authorization that permits a contribution.
    // Must be zero when maxClientContribution is zero.
    uint256 contributionNonce;
    uint256 deadline;
    // EIP-712 signature by clientFeeReceiver: a 65-byte ECDSA signature when
    // the receiver is an EOA, or an ERC-1271 signature of any length when it
    // is a contract.
    bytes clientSignature;
}

error TychoRouter__TimelockNotExpired(
    uint256 activationTimestamp, uint256 blockTimestamp
);
error TychoRouter__NoPendingFeeCalculator();
error TychoRouter__FeesExceedOutput(uint256 totalFees, uint256 actualAmountOut);

contract TychoRouterV3 is AccessControl, Dispatcher, EIP712 {
    address private _feeCalculator; // Fee calculator contract
    address private _pendingFeeCalculator;
    uint48 private _feeCalculatorActivationTimestamp;

    // Consumed contribution nonces, packed 256 per word. Keyed by the
    // clientFeeReceiver address, so an ERC-1271 wallet keeps its consumed
    // nonces across owner rotations.
    mapping(address client => mapping(uint248 wordPos => uint256 bitmap)) public
        clientContributionNonceBitmap;

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

    uint256 public constant DELAY_FEE_CALCULATOR_ACTIVATION = 1 days;

    bytes32 public constant CLIENT_FEE_TYPEHASH = keccak256(
        "ClientFee(uint32 clientFeeBps,address clientFeeReceiver,"
        "uint256 maxClientContribution,uint256 contributionNonce,"
        "uint256 deadline,uint256 amountIn,address tokenIn,address tokenOut,"
        "uint256 expectedAmountOut,uint256 minAmountOut,address receiver,bytes swaps)"
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
    event ClientContributionNoncesInvalidated(
        address indexed client, uint248 indexed wordPos, uint256 mask
    );

    constructor(
        address permit2_,
        address feeCalculator,
        address pauserAdmin,
        address unpauserAdmin,
        address executorSetterAdmin,
        address routerFeeSetterAdmin
    ) Dispatcher(permit2_) EIP712("TychoRouter", "2") {
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
     * - Reverts with `TychoRouter__NegativeSlippage` if the final output is below `minAmountOut`
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param expectedAmountOut The quoted output amount; used to detect positive slippage.
     * @param minAmountOut The minimum acceptable output amount (revert guardrail). Must be non-zero and not exceed `expectedAmountOut`.
     * @param nTokens The total number of tokens involved in the swap graph (used to initialize arrays for internal calculations).
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, contribution nonce, deadline and signature.
     * @param swaps Encoded swap graph data containing details of each swap.
     *
     * @return The total amount of the output token received by the receiver.
     */
    function splitSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        uint256 nTokens,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) public payable whenNotPaused nonReentrant returns (uint256) {
        _verifyAndConsumeClientAuthorization(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
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
            expectedAmountOut,
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
     * - Reverts with `TychoRouter__NegativeSlippage` if the final output is below `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param expectedAmountOut The quoted output amount; used to detect positive slippage.
     * @param minAmountOut The minimum acceptable output amount (revert guardrail). Must be non-zero and not exceed `expectedAmountOut`.
     * @param nTokens The total number of tokens involved in the swap graph (used to initialize arrays for internal calculations).
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, contribution nonce, deadline and signature.
     * @param swaps Encoded swap graph data containing details of each swap.
     *
     * @return The total amount of the output token received by the receiver.
     */
    function splitSwapUsingVault(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        uint256 nTokens,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) public whenNotPaused nonReentrant returns (uint256) {
        _verifyAndConsumeClientAuthorization(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
            minAmountOut,
            receiver,
            swaps
        );
        _tstoreTransferFromInfo(tokenIn, amountIn, false, true);

        return _splitSwapChecked(
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
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
     * - Reverts with `TychoRouter__NegativeSlippage` if the final output is below `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param expectedAmountOut The quoted output amount; used to detect positive slippage.
     * @param minAmountOut The minimum acceptable output amount (revert guardrail). Must be non-zero and not exceed `expectedAmountOut`.
     * @param nTokens The total number of tokens involved in the swap graph (used to initialize arrays for internal calculations).
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, contribution nonce, deadline and signature.
     * @param permitSingle A Permit2 structure containing token approval details for the input token.
     * @param signature A valid signature authorizing the Permit2 approval.
     * @param swaps Encoded swap graph data containing details of each swap.
     *
     * @return The total amount of the output token received by the receiver.
     */
    function splitSwapPermit2(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        uint256 nTokens,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        IAllowanceTransfer.PermitSingle calldata permitSingle,
        bytes calldata signature,
        bytes calldata swaps
    ) external whenNotPaused nonReentrant returns (uint256) {
        _verifyAndConsumeClientAuthorization(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
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
            expectedAmountOut,
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
     * - Reverts with `TychoRouter__NegativeSlippage` if the final output is below `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param expectedAmountOut The quoted output amount; used to detect positive slippage.
     * @param minAmountOut The minimum acceptable output amount (revert guardrail). Must be non-zero and not exceed `expectedAmountOut`.
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, contribution nonce, deadline and signature.
     * @param swaps Encoded swap graph data containing details of each swap.
     *
     * @return The total amount of the output token received by the receiver.
     */
    function sequentialSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) public payable whenNotPaused nonReentrant returns (uint256) {
        _verifyAndConsumeClientAuthorization(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
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
            expectedAmountOut,
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
     * - Reverts with `TychoRouter__NegativeSlippage` if the final output is below `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param expectedAmountOut The quoted output amount; used to detect positive slippage.
     * @param minAmountOut The minimum acceptable output amount (revert guardrail). Must be non-zero and not exceed `expectedAmountOut`.
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, contribution nonce, deadline and signature.
     * @param swaps Encoded swap graph data containing details of each swap.
     *
     * @return The total amount of the output token received by the receiver.
     */
    function sequentialSwapUsingVault(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) public whenNotPaused nonReentrant returns (uint256) {
        _verifyAndConsumeClientAuthorization(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
            minAmountOut,
            receiver,
            swaps
        );
        _tstoreTransferFromInfo(tokenIn, amountIn, false, true);

        return _sequentialSwapChecked(
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
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
     * - Reverts with `TychoRouter__NegativeSlippage` if the final output is below `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param expectedAmountOut The quoted output amount; used to detect positive slippage.
     * @param minAmountOut The minimum acceptable output amount (revert guardrail). Must be non-zero and not exceed `expectedAmountOut`.
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, contribution nonce, deadline and signature.
     * @param permitSingle A Permit2 structure containing token approval details for the input token.
     * @param signature A valid signature authorizing the Permit2 approval.
     * @param swaps Encoded swap graph data containing details of each swap.
     *
     * @return The total amount of the output token received by the receiver.
     */
    function sequentialSwapPermit2(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        IAllowanceTransfer.PermitSingle calldata permitSingle,
        bytes calldata signature,
        bytes calldata swaps
    ) external whenNotPaused nonReentrant returns (uint256) {
        _verifyAndConsumeClientAuthorization(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
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
            expectedAmountOut,
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
     * - Reverts with `TychoRouter__NegativeSlippage` if the final output is below `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param expectedAmountOut The quoted output amount; used to detect positive slippage.
     * @param minAmountOut The minimum acceptable output amount (revert guardrail). Must be non-zero and not exceed `expectedAmountOut`.
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, contribution nonce, deadline and signature.
     * @param swapData Encoded swap details.
     *
     * @return The total amount of the output token received by the receiver.
     */
    function singleSwap(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swapData
    ) public payable whenNotPaused nonReentrant returns (uint256) {
        _verifyAndConsumeClientAuthorization(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
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
            expectedAmountOut,
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
     * - Reverts with `TychoRouter__NegativeSlippage` if the final output is below `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param expectedAmountOut The quoted output amount; used to detect positive slippage.
     * @param minAmountOut The minimum acceptable output amount (revert guardrail). Must be non-zero and not exceed `expectedAmountOut`.
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, contribution nonce, deadline and signature.
     * @param swapData Encoded swap details.
     *
     * @return The total amount of the output token received by the receiver.
     */
    function singleSwapUsingVault(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swapData
    ) public whenNotPaused nonReentrant returns (uint256) {
        _verifyAndConsumeClientAuthorization(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
            minAmountOut,
            receiver,
            swapData
        );
        _tstoreTransferFromInfo(tokenIn, amountIn, false, true);

        return _singleSwap(
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
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
     * - Reverts with `TychoRouter__NegativeSlippage` if the final output is below `minAmountOut`.
     *
     * @param amountIn The input token amount to be swapped.
     * @param tokenIn The address of the input token. Use `ETH_ADDRESS` for native ETH
     * @param tokenOut The address of the output token. Use `ETH_ADDRESS` for native ETH
     * @param expectedAmountOut The quoted output amount; used to detect positive slippage.
     * @param minAmountOut The minimum acceptable output amount (revert guardrail). Must be non-zero and not exceed `expectedAmountOut`.
     * @param receiver The address to receive the output tokens.
     * @param clientFeeParams Client fee parameters including fee bps, receiver, max contribution, contribution nonce, deadline and signature.
     * @param permitSingle A Permit2 structure containing token approval details for the input token.
     * @param signature A valid signature authorizing the Permit2 approval.
     * @param swapData Encoded swap details.
     *
     * @return The total amount of the output token received by the receiver.
     */
    function singleSwapPermit2(
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        IAllowanceTransfer.PermitSingle calldata permitSingle,
        bytes calldata signature,
        bytes calldata swapData
    ) external whenNotPaused nonReentrant returns (uint256) {
        _verifyAndConsumeClientAuthorization(
            clientFeeParams,
            amountIn,
            tokenIn,
            tokenOut,
            expectedAmountOut,
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
            expectedAmountOut,
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
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        uint256 nTokens,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) internal returns (uint256 amountOutAfterFees) {
        _validateAmounts(amountIn, expectedAmountOut, minAmountOut);
        _validateAddresses(receiver, tokenIn, tokenOut);

        address client = clientFeeParams.clientFeeReceiver;
        // Stack pressure in this function prevents keeping finalReceiver in scope
        // outside the block, so we cache the interception decision as a bool instead.
        bool intercepting = IFeeCalculator(_feeCalculator)
            .mustOutputThroughRouter(clientFeeParams.clientFeeBps, client);

        uint256 actualAmountOut;
        {
            address finalReceiver = intercepting ? address(this) : receiver;
            actualAmountOut = _splitSwap(
                amountIn,
                nTokens,
                swaps,
                finalReceiver,
                tokenIn == tokenOut // isCyclical
            );
        }

        amountOutAfterFees = _finalizeSwap(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: tokenIn,
                tokenOut: tokenOut,
                clientFeeBps: clientFeeParams.clientFeeBps,
                client: client
            }),
            intercepting,
            minAmountOut,
            clientFeeParams.maxClientContribution,
            receiver
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
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swap_
    ) internal returns (uint256 amountOutAfterFees) {
        _validateAmounts(amountIn, expectedAmountOut, minAmountOut);
        _validateAddresses(receiver, tokenIn, tokenOut);

        address client = clientFeeParams.clientFeeReceiver;
        bool intercepting = IFeeCalculator(_feeCalculator)
            .mustOutputThroughRouter(clientFeeParams.clientFeeBps, client);

        uint256 actualAmountOut;
        {
            (address executor, bytes calldata protocolData) =
                swap_.decodeSingleSwap();
            actualAmountOut = _callSwapOnExecutor(
                executor,
                amountIn,
                protocolData,
                true,
                false,
                intercepting ? address(this) : receiver
            );
        }

        amountOutAfterFees = _finalizeSwap(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: tokenIn,
                tokenOut: tokenOut,
                clientFeeBps: clientFeeParams.clientFeeBps,
                client: client
            }),
            intercepting,
            minAmountOut,
            clientFeeParams.maxClientContribution,
            receiver
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
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        ClientFeeParams calldata clientFeeParams,
        bytes calldata swaps
    ) internal returns (uint256 amountOutAfterFees) {
        _validateAmounts(amountIn, expectedAmountOut, minAmountOut);
        _validateAddresses(receiver, tokenIn, tokenOut);
        if (swaps.length == 0) {
            revert TychoRouter__EmptySwaps();
        }

        address client = clientFeeParams.clientFeeReceiver;
        bool intercepting = IFeeCalculator(_feeCalculator)
            .mustOutputThroughRouter(clientFeeParams.clientFeeBps, client);

        uint256 actualAmountOut = _sequentialSwap(
            amountIn, swaps, intercepting ? address(this) : receiver
        );

        amountOutAfterFees = _finalizeSwap(
            FeeInput({
                actualAmountOut: actualAmountOut,
                expectedAmountOut: expectedAmountOut,
                amountIn: amountIn,
                tokenIn: tokenIn,
                tokenOut: tokenOut,
                clientFeeBps: clientFeeParams.clientFeeBps,
                client: client
            }),
            intercepting,
            minAmountOut,
            clientFeeParams.maxClientContribution,
            receiver
        );
    }

    /**
     * @dev Validates the swap amount inputs shared by all swap strategies.
     *      Reverts unless `amountIn` and `expectedAmountOut` are non-zero and
     *      `0 < minAmountOut <= expectedAmountOut`.
     */
    function _validateAmounts(
        uint256 amountIn,
        uint256 expectedAmountOut,
        uint256 minAmountOut
    ) internal pure {
        if (amountIn == 0) {
            revert TychoRouter__ZeroInput();
        }
        if (expectedAmountOut == 0) {
            revert TychoRouter__AmountOutZero();
        }
        if (minAmountOut == 0 || minAmountOut > expectedAmountOut) {
            revert TychoRouter__InvalidMinAmountOut(
                minAmountOut, expectedAmountOut
            );
        }
    }

    /**
     * @dev Validates the address inputs shared by all swap strategies.
     */
    function _validateAddresses(
        address receiver,
        address tokenIn,
        address tokenOut
    ) internal pure {
        if (
            receiver == address(0) || tokenIn == address(0)
                || tokenOut == address(0)
        ) {
            revert TychoRouter__AddressZero();
        }
    }

    /**
     * @dev Shared post-swap finalization for all swap strategies: takes fees
     *      (when the output was intercepted by the router), tops the output
     *      up with a client contribution if it fell below `minAmountOut`,
     *      and settles the final amount to the receiver.
     *      `feeInput` doubles as the carrier for the swap context (amounts,
     *      tokens, client), keeping caller stacks flat.
     * @return amountOutAfterFees The settled output amount.
     */
    function _finalizeSwap(
        FeeInput memory feeInput,
        bool intercepting,
        uint256 minAmountOut,
        uint256 maxClientContribution,
        address receiver
    ) internal returns (uint256 amountOutAfterFees) {
        amountOutAfterFees = intercepting
            ? _takeFees(feeInput)
            : feeInput.actualAmountOut;

        amountOutAfterFees = _maybeAddClientContribution(
            amountOutAfterFees,
            minAmountOut,
            maxClientContribution,
            feeInput.tokenOut,
            receiver,
            feeInput.client
        );

        amountOutAfterFees = _settleOutput(
            amountOutAfterFees,
            minAmountOut,
            feeInput.amountIn,
            feeInput.tokenIn,
            feeInput.tokenOut,
            receiver
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
     * @notice Calculates and takes fees using the FeeCalculator
     * @param feeInput Fee calculation inputs (amounts, tokens, client)
     * @return amountOutAfterFees Amount remaining after fee deductions
     */
    function _takeFees(FeeInput memory feeInput)
        internal
        returns (uint256 amountOutAfterFees)
    {
        FeeRecipient[] memory fees =
            IFeeCalculator(_feeCalculator).calculateFee(feeInput);
        amountOutAfterFees = feeInput.actualAmountOut;

        uint256 totalFees = 0;
        for (uint256 i = 0; i < fees.length; i++) {
            totalFees += fees[i].feeAmount;
        }
        if (totalFees > feeInput.actualAmountOut) {
            revert TychoRouter__FeesExceedOutput(
                totalFees, feeInput.actualAmountOut
            );
        }

        for (uint256 i = 0; i < fees.length; i++) {
            if (fees[i].feeAmount > 0) {
                // We still need to update the delta accounting to ensure the funds are
                // in the router after the final swap and have not bypassed the router
                // due to incorrect or malicious encoding. Updating the delta
                // accounting without funds will result in an additional negative
                // delta, and cause the _finalizeBalances method to revert.
                _updateDeltaAccounting(
                    feeInput.tokenOut, -int256(fees[i].feeAmount)
                );
                _creditVaultForFees(
                    fees[i].recipient, feeInput.tokenOut, fees[i].feeAmount
                );
                amountOutAfterFees -= fees[i].feeAmount;
            }
        }
        if (fees.length > 0) {
            emit FeesTaken(feeInput.tokenOut, fees);
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
                // slither-disable-next-line incorrect-equality
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
     * @notice Marks a range of the caller's contribution nonces as used, so no
     *         authorization carrying one of them can execute.
     * @dev Callable while the router is paused. Bits can only be set, never
     *      cleared, and each caller writes only its own namespace. Calling it
     *      twice with the same arguments is idempotent.
     * @param wordPos The bitmap word, equal to `contributionNonce >> 8`.
     * @param mask The bits to set within that word.
     */
    function invalidateClientContributionNonces(uint248 wordPos, uint256 mask)
        external
    {
        clientContributionNonceBitmap[msg.sender][wordPos] |= mask;
        emit ClientContributionNoncesInvalidated(msg.sender, wordPos, mask);
    }

    /**
     * @dev Consumes a contribution nonce for a client, reverting when that
     *      nonce is already used. Mirrors the Permit2 unordered nonce bitmap:
     *      the flip happens before the check because any revert on the
     *      enclosing call rolls the write back.
     * @param client The clientFeeReceiver whose namespace the nonce belongs to.
     * @param contributionNonce The nonce to consume.
     */
    function _useClientContributionNonce(
        address client,
        uint256 contributionNonce
    ) internal {
        uint248 wordPos = uint248(contributionNonce >> 8);
        uint256 bit = 1 << uint8(contributionNonce);
        uint256 flipped = clientContributionNonceBitmap[client][wordPos] ^= bit;

        if (flipped & bit == 0) {
            revert TychoRouter__InvalidClientContributionNonce(
                client, contributionNonce
            );
        }
    }

    /**
     * @dev Verifies the client's EIP-712 signature over the fee parameters,
     *      the core swap parameters, and the encoded swap routing bytes, and
     *      consumes the contribution nonce when the authorization permits a
     *      contribution. A nonce commits at most one successful swap, so a
     *      successful call consumes it even when the swap needed no
     *      contribution. Any later revert rolls the consumption back.
     *      When clientFeeReceiver is address(0), no signature is required and
     *      every other field must be zero or empty.
     *      An EOA receiver signs with ECDSA; a contract receiver validates the
     *      signature itself through ERC-1271. Contract signatures are
     *      revocable, so a signature that verifies in one block may stop
     *      verifying in the next.
     * @param p The client fee parameters including the signature to verify.
     * @param amountIn The input token amount.
     * @param tokenIn The input token address.
     * @param tokenOut The output token address.
     * @param expectedAmountOut The quoted output amount.
     * @param minAmountOut The minimum acceptable output amount.
     * @param receiver The address to receive the output tokens.
     * @param swapData The encoded swap routing data.
     */
    function _verifyAndConsumeClientAuthorization(
        ClientFeeParams calldata p,
        uint256 amountIn,
        address tokenIn,
        address tokenOut,
        uint256 expectedAmountOut,
        uint256 minAmountOut,
        address receiver,
        bytes calldata swapData
    ) internal {
        if (p.clientFeeReceiver == address(0)) {
            if (
                p.maxClientContribution > 0 || p.clientFeeBps > 0
                    || p.contributionNonce > 0 || p.deadline > 0
                    || p.clientSignature.length > 0
            ) {
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
        if (p.maxClientContribution > 0) {
            _useClientContributionNonce(
                p.clientFeeReceiver, p.contributionNonce
            );
        } else if (p.contributionNonce > 0) {
            // A fee-only authorization stays replayable and pays no storage
            // cost, so it must not carry a nonce that looks like protection.
            revert TychoRouter__NonZeroContributionNonce(p.contributionNonce);
        }
        bytes32 digest = _hashTypedDataV4(
            keccak256(
                abi.encode(
                    CLIENT_FEE_TYPEHASH,
                    p.clientFeeBps,
                    p.clientFeeReceiver,
                    p.maxClientContribution,
                    p.contributionNonce,
                    p.deadline,
                    amountIn,
                    tokenIn,
                    tokenOut,
                    expectedAmountOut,
                    minAmountOut,
                    receiver,
                    keccak256(swapData)
                )
            )
        );
        // ECDSA runs before ERC-1271 so that an EOA carrying delegated code
        // (EIP-7702) keeps signing with its own key. tryRecover's third return
        // value only describes the error, which err already reports.
        // slither-disable-next-line unused-return
        (address recovered, ECDSA.RecoverError err,) =
            ECDSA.tryRecoverCalldata(digest, p.clientSignature);
        if (
            err == ECDSA.RecoverError.NoError
                && recovered == p.clientFeeReceiver
        ) {
            return;
        }
        // A contract receiver validates the digest itself
        if (!SignatureChecker.isValidERC1271SignatureNowCalldata(
                p.clientFeeReceiver, digest, p.clientSignature
            )) {
            revert TychoRouter__InvalidClientSignature();
        }
    }
}
