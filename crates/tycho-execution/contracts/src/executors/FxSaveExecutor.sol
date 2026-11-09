// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {
    SafeERC20,
    IERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {IERC4626} from "@openzeppelin/contracts/interfaces/IERC4626.sol";
import {TransferManager} from "../TransferManager.sol";

/// @notice f(x) Protocol's stability pool (fxSP), which mints shares for
/// fxUSD at $1 or USDC at its Chainlink price.
interface IFxUSDBasePool {
    function deposit(
        address receiver,
        address tokenIn,
        uint256 amountTokenToDeposit,
        uint256 minSharesOut
    ) external returns (uint256 amountSharesOut);
}

error FxSaveExecutor__InvalidDataLength();
error FxSaveExecutor__ZeroAddress();

/// @notice Deposits USDC or fxUSD into fxSAVE: the token into fxSP, then the
/// fxSP into fxSAVE, an ERC4626 vault over it.
contract FxSaveExecutor is IExecutor {
    using SafeERC20 for IERC20;

    // tokenIn (20)
    uint256 private constant _DATA_LENGTH = 20;

    IFxUSDBasePool public immutable fxBasePool;
    IERC4626 public immutable fxSave;

    constructor(address fxBasePool_, address fxSave_) {
        if (fxBasePool_ == address(0) || fxSave_ == address(0)) {
            revert FxSaveExecutor__ZeroAddress();
        }
        fxBasePool = IFxUSDBasePool(fxBasePool_);
        fxSave = IERC4626(fxSave_);
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

    // slither-disable-next-line locked-ether The router delegatecalls swap, so any ETH it receives is held by the router.
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        address tokenIn = _decodeData(data);
        IERC20 fxSp = IERC20(address(fxBasePool));
        // The router may already hold fxSP for vault users, so only what this
        // deposit mints moves on to fxSAVE. It is measured, not taken from
        // fxSP's return value. fxSP rejects any token but its USDC and fxUSD.
        uint256 before = fxSp.balanceOf(address(this));
        // slither-disable-next-line unused-return The minted shares are measured by balance diff.
        fxBasePool.deposit(address(this), tokenIn, amountIn, 0);
        uint256 shares = fxSp.balanceOf(address(this)) - before;
        fxSp.forceApprove(address(fxSave), shares);
        // slither-disable-next-line unused-return The Dispatcher derives the output from balance diffs, not from return values.
        fxSave.deposit(shares, receiver);
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address tokenIn)
    {
        if (data.length != _DATA_LENGTH) {
            revert FxSaveExecutor__InvalidDataLength();
        }
        tokenIn = address(bytes20(data[0:20]));
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
        tokenIn = _decodeData(data);
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        receiver = address(fxBasePool);
        tokenOut = address(fxSave);
        outputToRouter = false;
    }
}
