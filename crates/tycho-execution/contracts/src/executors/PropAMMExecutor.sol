// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";
import {
    IERC20,
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @notice Minimal surface of PropAMM's venue adapter (same address on Base
///         and Base Sepolia: 0x1123da3Cf775EE932A83f3d3b9eDBe2e151F79B7).
///         One call commits the maker-signed price ladders and fills per
///         maker leg. Struct layouts mirror the adapter exactly; field order
///         is load-bearing for abi decoding.
interface IPropAMMAdapter {
    struct Level {
        uint256 size;
        uint256 price;
    }

    struct PriceLadder {
        address mm;
        address tokenIn;
        address tokenOut;
        Level[] levels;
        uint256 nonce;
        uint256 expiresAt;
    }

    struct FillLeg {
        address provider;
        PriceLadder ladder;
        uint256 amountIn;
    }

    function swap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minOut,
        address receiver,
        bytes calldata commitData,
        FillLeg[] calldata legs
    ) external returns (uint256 delivered);
}

/// @title PropAMMExecutor
/// @notice Executor for PropAMM streaming-maker RFQ swaps (rfq:propamm)
/// @dev The Rust swap encoder requests a binding firm quote immediately
///      before encoding (quotes are valid for seconds; superseded ladders
///      revert on-chain rather than filling stale) and re-shapes the quote's
///      settlement calls into abi.encode(tokenIn, tokenOut, commitData,
///      legs). The only external target is the immutable PropAMM adapter,
///      called through a typed interface - no raw calldata forwarding, so
///      there is no selector spoofing surface.
/// @dev The adapter pulls exactly amountIn from msg.sender and reverts if
///      the leg sum does not match, so partial fills are not supported:
///      amountIn must equal the quoted amount.
contract PropAMMExecutor is IExecutor {
    using SafeERC20 for IERC20;

    /// @notice PropAMM-specific errors
    error PropAMMExecutor__ZeroAddress();
    error PropAMMExecutor__InvalidDataLength();

    /// @dev abi.encode(address, address, bytes, FillLeg[]) is at least four
    ///      head words plus one length word each for the empty bytes and the
    ///      empty array tail: 6 * 32 = 192 bytes.
    uint256 private constant _MIN_DATA_LENGTH = 192;

    /// @notice The PropAMM venue adapter contract address
    address public immutable propammAdapter;

    constructor(address propammAdapter_) {
        if (propammAdapter_ == address(0)) {
            revert PropAMMExecutor__ZeroAddress();
        }
        propammAdapter = propammAdapter_;
    }

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        // The adapter debits the router (this executor runs via delegatecall),
        // so input funds must be at the router before the swap.
        return msg.sender;
    }

    /// @notice Executes a swap through the PropAMM adapter
    /// @param amountIn The amount of input token to swap; must equal the
    ///        quoted amount baked into the encoded legs
    /// @param data abi.encode(tokenIn, tokenOut, commitData, legs) produced
    ///        by the Rust swap encoder from a fresh firm quote
    /// @param receiver The address to receive output tokens
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (
            address tokenIn,
            address tokenOut,
            bytes memory commitData,
            IPropAMMAdapter.FillLeg[] memory legs
        ) = _decodeData(data);

        // Exact approval for the adapter's transferFrom. Under the router's
        // delegatecall this approves router funds; the TransferManager's
        // ProtocolWillDebit approval is equivalent, but the explicit approve
        // keeps the executor usable standalone.
        IERC20(tokenIn).forceApprove(propammAdapter, amountIn);

        // minOut = 0: TychoRouter performs the authoritative output check via
        // balance accounting; the adapter's floor stays available for direct
        // integrators.
        // slither-disable-next-line unused-return
        IPropAMMAdapter(propammAdapter)
            .swap(tokenIn, tokenOut, amountIn, 0, receiver, commitData, legs);
    }

    /// @dev Decodes the abi encoded executor payload
    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            address tokenIn,
            address tokenOut,
            bytes memory commitData,
            IPropAMMAdapter.FillLeg[] memory legs
        )
    {
        if (data.length < _MIN_DATA_LENGTH) {
            revert PropAMMExecutor__InvalidDataLength();
        }
        (tokenIn, tokenOut, commitData, legs) = abi.decode(
            data, (address, address, bytes, IPropAMMAdapter.FillLeg[])
        );
    }

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
        if (data.length < _MIN_DATA_LENGTH) {
            revert PropAMMExecutor__InvalidDataLength();
        }
        (tokenIn, tokenOut,,) = abi.decode(
            data, (address, address, bytes, IPropAMMAdapter.FillLeg[])
        );
        // The adapter pulls tokenIn from the caller via transferFrom, so the
        // approval must go to the adapter.
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        receiver = propammAdapter;
        // The adapter delivers output straight to the receiver argument.
        outputToRouter = false;
    }
}
