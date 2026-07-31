// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {TransferManager} from "../TransferManager.sol";
import {IExecutor} from "@interfaces/IExecutor.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {
    SafeERC20
} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {Address} from "@openzeppelin/contracts/utils/Address.sol";

error HashflowExecutor__InvalidHashflowRouter();
error HashflowExecutor__InvalidDataLength();

interface IHashflowRouter {
    struct RFQTQuote {
        address pool;
        address externalAccount;
        address trader;
        address effectiveTrader;
        address baseToken;
        address quoteToken;
        uint256 effectiveBaseTokenAmount;
        uint256 baseTokenAmount;
        uint256 quoteTokenAmount;
        uint256 quoteExpiry;
        uint256 nonce;
        bytes32 txid;
        bytes signature; // ECDSA signature of the quote, 65 bytes
    }

    function tradeRFQT(RFQTQuote calldata quote) external payable;
}

contract HashflowExecutor is IExecutor {
    using SafeERC20 for IERC20;

    address public constant NATIVE_TOKEN =
        0x0000000000000000000000000000000000000000;

    /// @notice The Hashflow router address
    address public immutable hashflowRouter;

    constructor(address hashflowRouter_) {
        if (hashflowRouter_ == address(0)) {
            revert HashflowExecutor__InvalidHashflowRouter();
        }
        hashflowRouter = hashflowRouter_;
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

    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (IHashflowRouter.RFQTQuote memory quote) = _decodeData(data);

        // Slippage checks
        if (amountIn > quote.baseTokenAmount) {
            // Do not transfer more than the quote's maximum permitted amount.
            amountIn = quote.baseTokenAmount;
        }
        quote.effectiveBaseTokenAmount = amountIn;

        uint256 ethValue = 0;
        if (quote.baseToken == NATIVE_TOKEN) {
            ethValue = quote.effectiveBaseTokenAmount;
        }

        IHashflowRouter(hashflowRouter).tradeRFQT{value: ethValue}(quote);
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (IHashflowRouter.RFQTQuote memory quote)
    {
        if (data.length != 325) {
            revert HashflowExecutor__InvalidDataLength();
        }

        quote.pool = address(bytes20(data[0:20]));
        quote.externalAccount = address(bytes20(data[20:40]));
        quote.trader = address(bytes20(data[40:60]));
        // Assumes we never set the effectiveTrader when requesting a quote.
        quote.effectiveTrader = quote.trader;
        quote.baseToken = address(bytes20(data[60:80]));
        quote.quoteToken = address(bytes20(data[80:100]));
        // Not included in the calldata. Will be set in the swap function.
        quote.effectiveBaseTokenAmount = 0;
        quote.baseTokenAmount = uint256(bytes32(data[100:132]));
        quote.quoteTokenAmount = uint256(bytes32(data[132:164]));
        quote.quoteExpiry = uint256(bytes32(data[164:196]));
        quote.nonce = uint256(bytes32(data[196:228]));
        quote.txid = bytes32(data[228:260]);
        quote.signature = data[260:325];
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
        if (data.length != 325) {
            revert HashflowExecutor__InvalidDataLength();
        }

        transferType = TransferManager.TransferType.ProtocolWillDebit;
        tokenIn = address(bytes20(data[60:80]));
        tokenOut = address(bytes20(data[80:100]));
        receiver = hashflowRouter;
        outputToRouter = true;
    }
}
