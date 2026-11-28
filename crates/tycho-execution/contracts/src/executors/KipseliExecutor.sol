// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

/// @dev From Kipseli's docs (docs.kipseli.capital); the router is not source-verified. The
///      selector (0xecd47daa) matches the deployed bytecode on Base, BSC and Robinhood.
interface IKipseliRouter {
    function swap(
        address tokenIn,
        uint256 amountIn,
        address tokenOut,
        uint256 minOutAmount,
        uint256 quoteTimestamp,
        bytes calldata verificationData
    ) external returns (uint256 amountOut);
}

/// @title KipseliExecutor
/// @notice Executes a Kipseli PropAMM swap through the chain's Kipseli Router, with a quote the
///         Kipseli API signed for the TychoRouter.
/// @dev Data is `[tokenIn: 20][tokenOut: 20][quoteTimestamp: 32][verificationData: rest]`.
///      `quoteTimestamp` and `verificationData` are the API's `/v2/swap/sign` answer, passed
///      unmodified. The signature covers neither the amount nor the chain, so the swap trades the
///      whole `amountIn` the Dispatcher hands it.
///
///      Measured on a Base fork (block 62106094), as the router is not source-verified:
///      - The router pulls `tokenIn` from its caller with `transferFrom`, as itself, into its
///        swap implementation. The router is the spender, so it is the `ProtocolWillDebit`
///        receiver.
///      - The router pays `tokenOut` to its caller, the TychoRouter under delegatecall, and
///        takes no recipient, so the output lands at the router.
///      - A quote the venue does not accept is not rejected: the venue keeps the input and pays
///        nothing. The router reverts "return too low" when the output is below `minOutAmount`,
///        so the swap passes a floor of one wei, which turns that silent loss into a revert.
///        Pricing stays with the route's `minAmountOut`.
contract KipseliExecutor is IExecutor {
    error KipseliExecutor__InvalidRouter();
    error KipseliExecutor__InvalidDataLength();

    /// @dev tokenIn (20) + tokenOut (20) + quoteTimestamp (32).
    uint256 private constant _HEADER_LENGTH = 72;
    /// @dev The shortest `abi.encode(uint256 fee, uint256 originRate, address userAddress,
    ///      bytes signature)`: four head words and the signature's length word.
    uint256 private constant _MIN_VERIFICATION_DATA_LENGTH = 160;
    /// @dev See the contract notes: the venue pays zero for a quote it does not accept.
    uint256 private constant _MIN_AMOUNT_OUT = 1;

    /// @notice The chain's Kipseli Router, the contract the swap calls and approves.
    address public immutable kipseliRouter;

    constructor(address kipseliRouter_) {
        if (kipseliRouter_ == address(0)) {
            revert KipseliExecutor__InvalidRouter();
        }
        kipseliRouter = kipseliRouter_;
    }

    /// @inheritdoc IExecutor
    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return msg.sender;
    }

    /// @inheritdoc IExecutor
    // IExecutor is payable for delegatecall from the router; Kipseli swaps ERC20s.
    // slither-disable-next-line locked-ether
    function swap(
        uint256 amountIn,
        bytes calldata data,
        address /* receiver */
    )
        external
        payable
    {
        (
            address tokenIn,
            address tokenOut,
            uint256 quoteTimestamp,
            bytes calldata verificationData
        ) = _decodeData(data);
        // The Dispatcher measures the output by balance diff.
        // slither-disable-next-line unused-return
        IKipseliRouter(kipseliRouter)
            .swap(
                tokenIn,
                amountIn,
                tokenOut,
                _MIN_AMOUNT_OUT,
                quoteTimestamp,
                verificationData
            );
    }

    /// @inheritdoc IExecutor
    function getTransferData(bytes calldata data)
        external
        view
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        (tokenIn, tokenOut,,) = _decodeData(data);
        return (
            TransferManager.TransferType.ProtocolWillDebit,
            kipseliRouter,
            tokenIn,
            tokenOut,
            true
        );
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            address tokenIn,
            address tokenOut,
            uint256 quoteTimestamp,
            bytes calldata verificationData
        )
    {
        if (data.length < _HEADER_LENGTH + _MIN_VERIFICATION_DATA_LENGTH) {
            revert KipseliExecutor__InvalidDataLength();
        }
        tokenIn = address(bytes20(data[0:20]));
        tokenOut = address(bytes20(data[20:40]));
        quoteTimestamp = uint256(bytes32(data[40:72]));
        verificationData = data[_HEADER_LENGTH:];
    }
}
