// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

interface IBaibaiEntrypoint {
    function quoteToken() external view returns (address);
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
    error BaibaiExecutor__InvalidData();

    IBaibaiEntrypoint public immutable entrypoint;
    address public immutable quoteToken;

    /// @param entrypoint_ BaiBai entrypoint whose token wiring is fixed for this executor.
    constructor(address entrypoint_) {
        entrypoint = IBaibaiEntrypoint(entrypoint_);
        quoteToken = entrypoint.quoteToken();
        if (quoteToken == address(0)) revert BaibaiExecutor__InvalidData();
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
    // IExecutor is payable for delegatecall from the router; BaiBai swaps use ERC20s.
    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address base, address tokenIn,) = _decode(data);
        // The router enforces minAmountOut using the actual output balance delta.
        // slither-disable-next-line unused-return
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
        if (data.length != 21 || uint8(data[20]) > 1) {
            revert BaibaiExecutor__InvalidData();
        }
        base = address(bytes20(data[:20]));
        if (base == address(0) || base == quoteToken) {
            revert BaibaiExecutor__InvalidData();
        }
        bool sellBase = data[20] == bytes1(uint8(1));
        return
            (base, sellBase ? base : quoteToken, sellBase ? quoteToken : base);
    }
}
