// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

contract LiquidityPartyExecutor is IExecutor {
    /// @dev We avoid declaring any IERC20 types, since it is critical to use the router's transfer facility and never
    /// the inherent ERC20 transfer methods, not even the SafeERC20 versions.
    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        // Decode swap data
        (IPartyPool pool,,, uint8 indexIn, uint8 indexOut) = _decodeData(data);

        // Perform the swap. Tokens have already been sent to the pool by the Dispatcher.
        // slither-disable-next-line unused-return
        pool.swap(
            address(this), // pool requires msg.sender == payer for PREFUNDING
            Funding.PREFUNDING,
            receiver,
            indexIn,
            indexOut,
            amountIn,
            0, // no minimum output: checked by router
            0, // no deadline
            false, // no unwrap
            "" // no callback data
        );
    }

    function getTransferData(bytes calldata data)
        external
        pure
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        (IPartyPool pool, address _tokenIn, address _tokenOut,,) =
            _decodeData(data);
        transferType = TransferManager.TransferType.Transfer;
        receiver = address(pool);
        tokenIn = _tokenIn;
        tokenOut = _tokenOut;
        outputToRouter = false;
    }

    function fundsExpectedAddress(bytes calldata data)
        external
        pure
        returns (address receiver)
    {
        // Funds go directly to the pool (prefunding approach)
        return address(bytes20(data[0:20]));
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            IPartyPool pool,
            address tokenIn,
            address tokenOut,
            uint8 indexIn,
            uint8 indexOut
        )
    {
        pool = IPartyPool(address(bytes20(data[0:20])));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
        indexIn = uint8(data[60]);
        indexOut = uint8(data[61]);
    }
}

library Funding {
    /// @notice a constant passed to swap as the fundingSelector to indicate that the payer has used regular ERC20 approvals to allow the pool to move the necessary input tokens.
    // Slither analysis of this line is literally wrong and broken. The extra zero digits are REQUIRED by Solidity since it is a bytes4 literal.
    // slither-disable-next-line too-many-digits
    bytes4 internal constant APPROVALS = 0x00000000;

    /// @notice a constant passed to swap as the fundingSelector to indicate that the payer has already sent sufficient input tokens to the pool before calling swap, so no movement of input tokens is required.
    // Slither analysis of this line is literally wrong and broken. The extra zero digits are REQUIRED by Solidity since it is a bytes4 literal.
    // slither-disable-next-line too-many-digits
    bytes4 internal constant PREFUNDING = 0x00000001;
}

interface IPartyPool {
    /// @notice Protocol fee ledger accessor. Returns tokens owed (raw uint token units) from this pool as protocol fees
    ///         that have not yet been transferred out.
    function allProtocolFeesOwed() external view returns (uint256[] memory);

    /// @notice Swap input token inputTokenIndex -> token outputTokenIndex. Payer must approve token inputTokenIndex.
    /// @dev This function transfers the exact gross input (including fee) from payer and sends the computed output to receiver.
    ///      Non-standard tokens (fee-on-transfer, rebasers) are rejected via balance checks.
    /// @param payer address of the account that pays for the swap
    /// @param fundingSelector If set to USE_APPROVALS, then the payer must use regular ERC20 approvals to authorize the pool to move the required input amount. If this fundingSelector is USE_PREFUNDING, then all of the input amount is expected to have already been sent to the pool and no additional transfers are needed. Refunds of excess input amount are NOT provided and it is illegal to use this funding method with a limit price. Otherwise, for any other fundingSelector value, a callback style funding mechanism is used where the given selector is invoked on the payer, passing the arguments of (address inputToken, uint256 inputAmount). The callback function must send the given amount of input coin to the pool in order to continue the swap transaction, otherwise "Insufficient funds" is thrown.
    /// @param receiver address that will receive the output tokens
    /// @param inputTokenIndex index of input asset
    /// @param outputTokenIndex index of output asset
    /// @param maxAmountIn maximum amount of token inputTokenIndex (uint256) to transfer in (inclusive of fees)
    /// @param minAmountOut minimum output tokens to receive; reverts if not met (0 = disabled)
    /// @param deadline timestamp after which the transaction will revert. Pass 0 to ignore.
    /// @param cbData callback data if fundingSelector is of the callback type.
    /// @return amountIn actual input used (uint256), amountOut actual output sent (uint256), inFee fee taken from the input (uint256)
    function swap(
        address payer,
        bytes4 fundingSelector,
        address receiver,
        uint256 inputTokenIndex,
        uint256 outputTokenIndex,
        uint256 maxAmountIn,
        uint256 minAmountOut,
        uint256 deadline,
        bool unwrap,
        bytes memory cbData
    )
        external
        payable
        returns (uint256 amountIn, uint256 amountOut, uint256 inFee);
}
