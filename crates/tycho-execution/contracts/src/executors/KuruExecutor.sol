// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {SafeCast} from "@openzeppelin/contracts/utils/math/SafeCast.sol";
import {TransferManager} from "../TransferManager.sol";
import {ETH_ADDRESS} from "../../lib/NativeETH.sol";

interface IKuruOrderBook {
    function getMarketParams()
        external
        view
        returns (
            uint32 pricePrecision,
            uint96 sizePrecision,
            address baseAsset,
            uint256 baseAssetDecimals,
            address quoteAsset,
            uint256 quoteAssetDecimals,
            uint32 tickSize,
            uint96 minSize,
            uint96 maxSize,
            uint256 takerFeeBps,
            uint256 makerFeeBps
        );

    function placeAndExecuteMarketBuy(
        uint96 quoteSize,
        uint256 minAmountOut,
        bool isMargin,
        bool isFillOrKill
    ) external payable returns (uint256);

    function placeAndExecuteMarketSell(
        uint96 size,
        uint256 minAmountOut,
        bool isMargin,
        bool isFillOrKill
    ) external payable returns (uint256);
}

error KuruExecutor__InvalidDataLength();
error KuruExecutor__TokenNotInMarket();

/// Swaps against a Kuru (Monad) order book market with a fill-or-kill market
/// order. The market pulls the input from the router (`msg.sender` under
/// delegatecall) and pays the output back to it. Market orders take `uint96`
/// sizes in the market's precision: input below one precision unit stays in
/// the router.
contract KuruExecutor is IExecutor {
    using SafeCast for uint256;

    address internal constant _KURU_NATIVE = address(0);
    uint256 internal constant _DATA_LENGTH = 60;

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
    function swap(
        uint256 amountIn,
        bytes calldata data,
        address /* receiver */
    )
        external
        payable
    {
        (address market, address tokenIn,) = _decodeData(data);
        (
            uint32 pricePrecision,
            uint96 sizePrecision,
            address baseAsset,
            uint256 baseDecimals,
            address quoteAsset,
            uint256 quoteDecimals,,,,,
        ) = IKuruOrderBook(market).getMarketParams();

        bool nativeIn = tokenIn == ETH_ADDRESS;
        address kuruTokenIn = nativeIn ? _KURU_NATIVE : tokenIn;

        if (kuruTokenIn == baseAsset) {
            uint256 unit = 10 ** baseDecimals;
            uint96 size = (amountIn * sizePrecision / unit).toUint96();
            uint256 value = nativeIn ? uint256(size) * unit / sizePrecision : 0;
            // slither-disable-next-line arbitrary-send-eth,unused-return
            IKuruOrderBook(market).placeAndExecuteMarketSell{value: value}(
                size, 0, false, true
            );
        } else {
            if (kuruTokenIn != quoteAsset) {
                revert KuruExecutor__TokenNotInMarket();
            }
            uint256 unit = 10 ** quoteDecimals;
            uint96 quoteSize = (amountIn * pricePrecision / unit).toUint96();
            uint256 value =
                nativeIn ? uint256(quoteSize) * unit / pricePrecision : 0;
            // slither-disable-next-line arbitrary-send-eth,unused-return
            IKuruOrderBook(market).placeAndExecuteMarketBuy{value: value}(
                quoteSize, 0, false, true
            );
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
        address market;
        (market, tokenIn, tokenOut) = _decodeData(data);

        if (tokenIn == ETH_ADDRESS) {
            transferType = TransferManager.TransferType.TransferNativeInExecutor;
            receiver = address(0);
        } else {
            transferType = TransferManager.TransferType.ProtocolWillDebit;
            receiver = market;
        }
        outputToRouter = true;
    }

    function _decodeData(bytes calldata data)
        internal
        pure
        returns (address market, address tokenIn, address tokenOut)
    {
        if (data.length != _DATA_LENGTH) {
            revert KuruExecutor__InvalidDataLength();
        }
        market = address(bytes20(data[0:20]));
        tokenIn = address(bytes20(data[20:40]));
        tokenOut = address(bytes20(data[40:60]));
    }
}
