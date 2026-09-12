// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

interface IBaibaiEntrypoint {
    function quoteToken() external view returns (address);
    function takerFeeBps(address taker, address base)
        external
        view
        returns (uint16);
    function swapExactAmountIn(
        address base,
        address tokenIn,
        uint256 amountIn,
        uint256 minAmountOut,
        address to
    ) external returns (uint256);
}

/// @notice Executes exact-input swaps through one BaiBai entrypoint.
/// @dev Data is base (20 bytes) followed by sellBase (one byte, 0 or 1).
contract BaibaiExecutor is IExecutor {
    error InvalidData();
    error NonzeroTakerFee();

    IBaibaiEntrypoint public immutable entrypoint;
    address public immutable quoteToken;

    /// @param entrypoint_ BaiBai entrypoint whose token wiring is fixed for this executor.
    constructor(address entrypoint_) {
        entrypoint = IBaibaiEntrypoint(entrypoint_);
        quoteToken = entrypoint.quoteToken();
        if (quoteToken == address(0)) revert InvalidData();
    }

    /// @inheritdoc IExecutor
    function fundsExpectedAddress(bytes calldata)
        external
        view
        returns (address)
    {
        return msg.sender;
    }

    /// @inheritdoc IExecutor
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address base, address tokenIn,) = _decode(data);
        // The native simulator quotes the zero-fee path. Under delegatecall,
        // address(this) is the router, which is also the entrypoint's fee identity.
        if (entrypoint.takerFeeBps(address(this), base) != 0) {
            revert NonzeroTakerFee();
        }
        entrypoint.swapExactAmountIn(base, tokenIn, amountIn, 0, receiver);
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
        (, tokenIn, tokenOut) = _decode(data);
        return (
            TransferManager.TransferType.ProtocolWillDebit,
            address(entrypoint),
            tokenIn,
            tokenOut,
            false
        );
    }

    /// @dev Validates the packed direction and derives token roles from the fixed quote token.
    function _decode(bytes calldata data)
        internal
        view
        returns (address base, address tokenIn, address tokenOut)
    {
        if (data.length != 21 || uint8(data[20]) > 1) revert InvalidData();
        base = address(bytes20(data[:20]));
        if (base == address(0) || base == quoteToken) revert InvalidData();
        bool sellBase = data[20] == bytes1(uint8(1));
        return
            (base, sellBase ? base : quoteToken, sellBase ? quoteToken : base);
    }
}
