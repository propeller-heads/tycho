// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

/// @dev From Hanji's aggregator documentation
///      (docs.hanji.io/for-developers/liquidity-source-integration); the implementations behind
///      the proxy orderbooks are not source-verified.
interface IHanjiOrderbook {
    function getConfig()
        external
        view
        returns (
            uint256 scalingFactorTokenX,
            uint256 scalingFactorTokenY,
            address tokenX,
            address tokenY,
            bool supportsNativeEth,
            bool isTokenXWeth,
            address askTrie,
            address bidTrie,
            uint64 adminCommissionRate,
            uint64 totalAggressiveCommissionRate,
            uint64 totalPassiveCommissionRate,
            uint64 passiveOrderPayoutRate,
            bool shouldInvokeOnTrade
        );

    function placeOrder(
        bool isAsk,
        uint128 quantity,
        uint72 price,
        uint128 maxCommission,
        bool marketOnly,
        bool postOnly,
        bool transferExecutedTokens,
        uint256 expires
    )
        external
        payable
        returns (
            uint64 orderId,
            uint128 executedShares,
            uint128 executedValue,
            uint128 aggressiveFee
        );

    function placeMarketOrderWithTargetValue(
        bool isAsk,
        uint128 targetTokenYValue,
        uint72 price,
        uint128 maxCommission,
        bool transferExecutedTokens,
        uint256 expires
    )
        external
        payable
        returns (
            uint128 executedShares,
            uint128 executedValue,
            uint128 aggressiveFee
        );
}

/// @notice Executes exact-input swaps on a Hanji proxy orderbook: selling token X (the base) with
///         `placeOrder` and buying it with `placeMarketOrderWithTargetValue`.
/// @dev Data is the proxy orderbook, the token in and the token out (20 bytes each, ETH_ADDRESS
///      for native ETH). The orderbook is the caller's choice, so its `getConfig()` is not trusted
///      to decide anything that moves the router's funds: the tokens and whether the input is
///      native come from the data alone, and both `getTransferData` and `swap` revert unless the
///      config names the same pair (with native ETH on the side the data has it). The config only
///      supplies the input's scaling and the direction. The orderbook pulls what it fills from the
///      router, which approves it (`ProtocolWillDebit`), or takes native ETH as `msg.value`, and
///      sends the output to its caller, the router.
///
///      The orderbook fills whole shares: selling X spends `amountIn` rounded down to a multiple of
///      `scalingFactorTokenX`, and buying X spends the value of the shares it buys plus their fee,
///      at most `amountIn` rounded down to a multiple of `scalingFactorTokenY`. What it does not
///      spend stays at the router, where it is lost to the user; native ETH it does not spend it
///      refunds there. It does not revert when its book cannot fill the input, so the executor
///      does: a sell must fill every share, and a buy must leave unspent less than the cost of
///      `_MAX_UNSPENT_SHARES` more shares at the fill's average price. What a swap leaves behind is
///      therefore less than one share of a sell's input, and less than the cost of two shares of a
///      buy's.
contract HanjiExecutor is IExecutor {
    error HanjiExecutor__InvalidData();
    error HanjiExecutor__InvalidConfig();
    error HanjiExecutor__TokenMismatch();
    error HanjiExecutor__AmountTooLarge();
    error HanjiExecutor__NothingFilled();
    error HanjiExecutor__PartialFill(uint256 filled, uint256 requested);

    /// @dev The lowest price a sell accepts: any bid fills.
    uint72 internal constant _MIN_PRICE = 1;
    /// @dev The highest price a buy accepts: any ask fills. Prices carry at most six significant
    ///      digits, and the largest the orderbooks accept is 999,999e15: 1e21 reverts with
    ///      InvalidPriceRange (0x47ebe172). Measured on the ETH/USDC orderbook at Base block
    ///      61,663,161; the price range check is the CLOB's, shared by every market.
    uint72 internal constant _MAX_PRICE = 999_999e15;
    /// @dev How many shares' cost, at the fill's average price, a buy may leave unspent: see
    ///      `_requireBuyFilled`.
    uint256 internal constant _MAX_UNSPENT_SHARES = 2;

    /// @dev One swap as the data and the orderbook's config describe it.
    struct Swap {
        IHanjiOrderbook orderbook;
        address tokenIn;
        address tokenOut;
        bool sellBase;
        /// Atoms of the input token per unit the orderbook takes for it.
        uint256 scaling;
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
    function swap(uint256 amountIn, bytes calldata data, address)
        external
        payable
    {
        Swap memory s = _swap(data);
        uint256 units = amountIn / s.scaling;
        if (units > type(uint128).max) revert HanjiExecutor__AmountTooLarge();
        // A native input is rounded down to whole units: the orderbook takes no more than that,
        // and sending only that keeps its refund of the rest off the path.
        // forge-lint: disable-next-item(divide-before-multiply) The rounding is intended.
        // slither-disable-next-line divide-before-multiply The rounding to whole units is intended.
        uint256 nativeValue = s.tokenIn == ETH_ADDRESS ? units * s.scaling : 0;
        // The orderbook requires expires > block.timestamp, so this is "now"; the router's
        // minAmountOut is the price check.
        uint256 expires = block.timestamp + 1;
        if (s.sellBase) {
            // The commission is uncapped: the simulation prices it from the orderbook's rate.
            // forge-lint: disable-next-item(arbitrary-send-eth) Sends the native input to the orderbook the swap data names; the router's minAmountOut checks what comes back.
            // slither-disable-next-line unused-return Only the executed shares are checked; the router measures the output.
            (, uint128 executedShares,,) = s.orderbook
            .placeOrder{value: nativeValue}(
                true,
                // forge-lint: disable-next-line(unsafe-typecast) Checked against uint128 above.
                uint128(units),
                _MIN_PRICE,
                type(uint128).max,
                true,
                false,
                true,
                expires
            );
            _requireSellFilled(executedShares, units);
        } else {
            // forge-lint: disable-next-item(arbitrary-send-eth) Sends the native input to the orderbook the swap data names; the router's minAmountOut checks what comes back.
            (
                uint128 executedShares,
                uint128 executedValue,
                uint128 aggressiveFee
            ) = s.orderbook.placeMarketOrderWithTargetValue{value: nativeValue}(
                false,
                // forge-lint: disable-next-line(unsafe-typecast) Checked against uint128 above.
                uint128(units),
                _MAX_PRICE,
                type(uint128).max,
                true,
                expires
            );
            _requireBuyFilled(
                executedShares, uint256(executedValue) + aggressiveFee, units
            );
        }
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
        Swap memory s = _swap(data);
        transferType = s.tokenIn == ETH_ADDRESS
            ? TransferManager.TransferType.TransferNativeInExecutor
            : TransferManager.TransferType.ProtocolWillDebit;
        receiver = address(s.orderbook);
        tokenIn = s.tokenIn;
        tokenOut = s.tokenOut;
        outputToRouter = true;
    }

    /// @dev A sell must fill every share it asked for.
    function _requireSellFilled(uint256 executedShares, uint256 units)
        internal
        pure
    {
        if (executedShares == 0) revert HanjiExecutor__NothingFilled();
        if (executedShares < units) {
            revert HanjiExecutor__PartialFill(executedShares, units);
        }
    }

    /// @dev A buy must leave unspent less than the cost of `_MAX_UNSPENT_SHARES` more shares at
    ///      the fill's average price. Whole-share rounding leaves less than one share at the
    ///      marginal price, which within a book's spread stays under two at the average; a book
    ///      that ran out leaves more.
    function _requireBuyFilled(
        uint256 executedShares,
        uint256 spent,
        uint256 target
    ) internal pure {
        if (executedShares == 0) {
            revert HanjiExecutor__NothingFilled();
        }
        uint256 unspent = target - spent;
        if (unspent * executedShares >= _MAX_UNSPENT_SHARES * spent) {
            revert HanjiExecutor__PartialFill(spent, target);
        }
    }

    /// @dev The swap the data describes, checked against the orderbook's config: the data's pair
    ///      must be the market's, in either direction, with native ETH on the same side.
    function _swap(bytes calldata data) internal view returns (Swap memory s) {
        if (data.length != 60) revert HanjiExecutor__InvalidData();
        s.orderbook = IHanjiOrderbook(address(bytes20(data[:20])));
        s.tokenIn = address(bytes20(data[20:40]));
        s.tokenOut = address(bytes20(data[40:60]));
        if (address(s.orderbook) == address(0)) {
            revert HanjiExecutor__InvalidData();
        }
        // slither-disable-next-line unused-return The pair, native side and scaling are all a swap needs.
        (
            uint256 scalingX,
            uint256 scalingY,
            address tokenX,
            address tokenY,
            bool supportsNativeEth,
            bool isTokenXWeth,,,,,,,
        ) = s.orderbook.getConfig();
        if (scalingX == 0 || scalingY == 0) {
            revert HanjiExecutor__InvalidConfig();
        }
        if (supportsNativeEth) {
            if (isTokenXWeth) tokenX = ETH_ADDRESS;
            else tokenY = ETH_ADDRESS;
        }
        if (s.tokenIn == tokenX && s.tokenOut == tokenY) {
            (s.sellBase, s.scaling) = (true, scalingX);
        } else if (s.tokenIn == tokenY && s.tokenOut == tokenX) {
            (s.sellBase, s.scaling) = (false, scalingY);
        } else {
            revert HanjiExecutor__TokenMismatch();
        }
    }
}
