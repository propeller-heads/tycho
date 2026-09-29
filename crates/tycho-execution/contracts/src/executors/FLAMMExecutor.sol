// SPDX-License-Identifier: BUSL-1.1
// Copyright (c) 2026 Everlong Labs Limited
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";

/// @dev The subset of `IFLAMM` (blockend `src/interfaces/core/flamm/IFLAMM.sol`)
/// the executor calls. Both fills pull `amountInUsed` of the input token from
/// `msg.sender` with `transferFrom` (`FLAMMStore.sol:324-328`) and pay the
/// output to `to`.
interface IFLAMMPool {
    /// @notice The pool asset (cbBTC on the Base pool).
    function asset() external view returns (address);
    /// @notice Loan asset 0, the numeraire (USDC on the Base pool).
    function loanAsset() external view returns (address);
    /// @notice Exact-in swap between the pool asset and a loan asset, either
    /// direction. May consume less than `amountIn` (`FLAMMSwapLib.sol:65-109`).
    function swap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut,
        address to,
        uint256 deadline
    ) external returns (uint256 amountInUsed, uint256 amountOut);
    /// @notice Deliver pool asset, receive loan asset 0 on the leverage venue
    /// (`FLAMMLeverLib.sol:47-60`).
    function leverUp(
        uint256 poolAssetIn,
        uint256 minLoanOut,
        address to,
        uint256 deadline
    ) external returns (uint256 amountInUsed, uint256 loanOut);
}

/// @dev `FLAMMFactory.isPool` (blockend `src/factory/FLAMMFactory.sol:59`):
/// true for every beacon proxy the factory created, which is provenance, not
/// endorsement. Pool creation is permissionless.
interface IFLAMMFactory {
    function isPool(address pool) external view returns (bool);
}

error FLAMMExecutor__InvalidDataLength();
error FLAMMExecutor__ZeroFactory();
error FLAMMExecutor__UnknownPool(address pool);
error FLAMMExecutor__UnknownVenue(uint8 venue);
error FLAMMExecutor__LeverDownUnsupported();
error FLAMMExecutor__InvalidLeverPair(address tokenIn, address tokenOut);
error FLAMMExecutor__PartialFill(uint256 amountInUsed, uint256 amountIn);

/// @title FLAMMExecutor
/// @notice Executes exact-in fills on an Everlong FLAMM pool: the swap venue in
/// both directions (`venue = 0`) and the leverage venue's lever-up leg
/// (`venue = 1`, pool asset in, loan asset out).
///
/// Swap data is `pool (20) || tokenIn (20) || tokenOut (20) || venue (1)`.
///
/// The pool debits the router (`ProtocolWillDebit`, receiver = pool) and pays
/// the receiver directly. The executor holds no state, moves no tokens and
/// makes no approvals; the Dispatcher grants and revokes the allowance.
///
/// FLAMM may fill less than `amountIn`: a sell can be clipped by the gate room,
/// the notional cap or the funding ceiling (`FLAMMSwapLib.sol:163-172`), a buy
/// may consume less than offered (`FLAMMSwapLib.sol:204-205`), and a lever-up's
/// consumption is up to the invariant hook (`FLAMMLeverLib.sol:101`). The
/// TychoRouterV3 has no way to recover input a `ProtocolWillDebit` protocol did
/// not pull, so the executor reverts on any partial fill instead of stranding
/// the difference in the router. Lever-down pulls only its net pay leg by
/// construction (`FLAMMLeverLib.sol:135-141`) and is therefore not offered.
///
/// The pool address comes from calldata. `factory.isPool` restricts it to
/// proxies the factory created, but their hooks are chosen by the creator, so
/// the pool must be treated as caller-controlled: this executor is covered by
/// the router's security model (`model/src/model/executors.rs`).
contract FLAMMExecutor is IExecutor {
    uint8 internal constant VENUE_SWAP = 0;
    uint8 internal constant VENUE_LEVER_UP = 1;
    uint256 internal constant DATA_LENGTH = 61;

    address public immutable factory;

    constructor(address factory_) {
        if (factory_ == address(0)) revert FLAMMExecutor__ZeroFactory();
        factory = factory_;
    }

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return msg.sender;
    }

    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (address pool, address tokenIn, address tokenOut, uint8 venue) =
            _decodeData(data);

        if (!IFLAMMFactory(factory).isPool(pool)) {
            revert FLAMMExecutor__UnknownPool(pool);
        }

        uint256 amountInUsed;
        if (venue == VENUE_SWAP) {
            // slither-disable-next-line unused-return
            (amountInUsed,) = IFLAMMPool(pool)
                .swap(tokenIn, tokenOut, amountIn, 0, receiver, block.timestamp);
        } else {
            // `_decodeData` admits only VENUE_SWAP and VENUE_LEVER_UP.
            _requireLeverUpPair(pool, tokenIn, tokenOut);
            // slither-disable-next-line unused-return
            (amountInUsed,) = IFLAMMPool(pool)
                .leverUp(amountIn, 0, receiver, block.timestamp);
        }

        // Slither taints `amountInUsed` through the `block.timestamp` deadline
        // argument above; the pool only reads the deadline to revert `Expired`
        // (`FLAMMSwapLib.sol:75`, `FLAMMLeverLib.sol:51`).
        // slither-disable-next-line timestamp
        if (amountInUsed != amountIn) {
            revert FLAMMExecutor__PartialFill(amountInUsed, amountIn);
        }
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
        (receiver, tokenIn, tokenOut,) = _decodeData(data);
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        outputToRouter = false;
    }

    /// @dev The leverage venue is one-directional here: pool asset in, loan
    /// asset 0 out. The reverse pair is a lever-down, which pulls less than it
    /// is offered and is not supported. Any other pair is not a leverage pair.
    function _requireLeverUpPair(
        address pool,
        address tokenIn,
        address tokenOut
    ) internal view {
        address poolAsset = IFLAMMPool(pool).asset();
        address loanAsset = IFLAMMPool(pool).loanAsset();
        if (tokenIn == poolAsset && tokenOut == loanAsset) return;
        if (tokenIn == loanAsset && tokenOut == poolAsset) {
            revert FLAMMExecutor__LeverDownUnsupported();
        }
        revert FLAMMExecutor__InvalidLeverPair(tokenIn, tokenOut);
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address pool, address tokenIn, address tokenOut, uint8 venue)
    {
        if (data.length != DATA_LENGTH) {
            revert FLAMMExecutor__InvalidDataLength();
        }
        pool = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
        venue = uint8(data[60]);
        if (venue > VENUE_LEVER_UP) revert FLAMMExecutor__UnknownVenue(venue);
    }
}
