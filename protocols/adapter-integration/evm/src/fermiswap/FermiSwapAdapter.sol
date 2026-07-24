// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import {ISwapAdapter} from "src/interfaces/ISwapAdapter.sol";
import {
    IERC20,
    SafeERC20
} from "openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";

interface IFermiSwapCallback {
    function fermiSwapCallback(
        int256 amountIn,
        int256 amountOut,
        bytes calldata data
    ) external;
}

/// @title FermiSwapAdapter
/// @notice Adapter for swapping tokens on FermiSwap
contract FermiSwapAdapter is ISwapAdapter, IFermiSwapCallback {
    using SafeERC20 for IERC20;

    IFermiSwapper public immutable fermiSwapper;

    constructor(address fermiSwapper_) {
        fermiSwapper = IFermiSwapper(fermiSwapper_);
    }

    receive() external payable {}

    /// @inheritdoc ISwapAdapter
    function price(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        uint256[] memory specifiedAmounts
    ) external view override returns (Fraction[] memory calculatedPrices) {
        _validatePoolTokens(poolId, sellToken, buyToken);
        calculatedPrices = new Fraction[](specifiedAmounts.length);

        for (uint256 i = 0; i < specifiedAmounts.length; i++) {
            calculatedPrices[i] =
                priceAt(sellToken, buyToken, specifiedAmounts[i]);
        }
        return calculatedPrices;
    }

    /// @notice Calculate the FermiSwap quote price for an exact input amount.
    function priceAt(address sellToken, address buyToken, uint256 sellAmount)
        public
        view
        returns (Fraction memory calculatedPrice)
    {
        (uint256 amountIn, uint256 amountOut) = fermiSwapper.quoteAmounts(
            sellToken, buyToken, _amountSpecified(OrderSide.Sell, sellAmount)
        );
        if (amountIn == 0) {
            revert TooSmall(0);
        }

        calculatedPrice = Fraction(amountOut, amountIn);
    }

    /// @inheritdoc ISwapAdapter
    function swap(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        OrderSide side,
        uint256 specifiedAmount
    ) external override returns (Trade memory trade) {
        if (specifiedAmount == 0) {
            return trade;
        }

        _validatePoolTokens(poolId, sellToken, buyToken);

        int256 amountSpecified = _amountSpecified(side, specifiedAmount);
        (uint256 quotedAmountIn, uint256 quotedAmountOut) =
            fermiSwapper.quoteAmounts(sellToken, buyToken, amountSpecified);
        if (
            (side == OrderSide.Sell && quotedAmountOut == 0)
                || (side == OrderSide.Buy && quotedAmountIn == 0)
        ) {
            revert TooSmall(0);
        }

        uint256 amountCheck =
            side == OrderSide.Sell ? quotedAmountOut : quotedAmountIn;
        uint256 gasBefore = gasleft();
        (uint256 amountIn, uint256 amountOut) = fermiSwapper.fermiSwapWithCallback(
            sellToken,
            buyToken,
            amountSpecified,
            amountCheck,
            msg.sender,
            abi.encode(msg.sender, sellToken)
        );

        trade.calculatedAmount = side == OrderSide.Sell ? amountOut : amountIn;
        trade.gasUsed = gasBefore - gasleft();
        trade.price = priceAt(
            sellToken,
            buyToken,
            side == OrderSide.Sell ? specifiedAmount : trade.calculatedAmount
        );
        return trade;
    }

    /// @inheritdoc IFermiSwapCallback
    function fermiSwapCallback(int256 amountIn, int256, bytes calldata data)
        external
        override
    {
        require(msg.sender == address(fermiSwapper), "NotFermiSwapper");
        require(amountIn >= 0, "InvalidAmountIn");

        (address payer, address tokenIn) = abi.decode(data, (address, address));
        // forge-lint: disable-next-line(unsafe-typecast) -- amountIn >= 0
        IERC20(tokenIn).safeTransferFrom(payer, msg.sender, uint256(amountIn));
    }

    /// @inheritdoc ISwapAdapter
    function getLimits(bytes32 poolId, address sellToken, address buyToken)
        external
        view
        override
        returns (uint256[] memory limits)
    {
        _validatePoolTokens(poolId, sellToken, buyToken);
        limits = new uint256[](2);

        if (!_isPoolActive(poolId, sellToken, buyToken)) {
            return limits;
        }

        uint256 buyLimit =
            IERC20(buyToken).balanceOf(fermiSwapper.traderVault());

        if (buyLimit == 0) {
            return limits;
        }
        if (buyLimit > uint256(type(int256).max)) {
            buyLimit = uint256(type(int256).max);
        }

        (uint256 amountIn, uint256 amountOut) = fermiSwapper.quoteAmounts(
            sellToken, buyToken, _amountSpecified(OrderSide.Buy, buyLimit)
        );
        limits[0] = amountIn;
        limits[1] = amountOut;
    }

    /// @inheritdoc ISwapAdapter
    function getCapabilities(bytes32, address, address)
        external
        pure
        override
        returns (Capability[] memory capabilities)
    {
        capabilities = new Capability[](4);
        capabilities[0] = Capability.SellOrder;
        capabilities[1] = Capability.BuyOrder;
        capabilities[2] = Capability.PriceFunction;
        capabilities[3] = Capability.ConstantPrice;
    }

    /// @inheritdoc ISwapAdapter
    function getTokens(bytes32 poolId)
        external
        view
        override
        returns (address[] memory tokens)
    {
        IFermiEngine.PairInfo[] memory pairs = fermiSwapper.getPairs();

        for (uint256 i = 0; i < pairs.length; i++) {
            if (_poolId(pairs[i].baseAsset, pairs[i].quoteAsset) == poolId) {
                tokens = new address[](2);
                tokens[0] = pairs[i].baseAsset;
                tokens[1] = pairs[i].quoteAsset;
                return tokens;
            }
        }

        revert InvalidOrder("Unknown pool");
    }

    /// @inheritdoc ISwapAdapter
    function getPoolIds(uint256 offset, uint256 limit)
        external
        view
        override
        returns (bytes32[] memory ids)
    {
        IFermiEngine.PairInfo[] memory pairs = fermiSwapper.getPairs();
        if (offset >= pairs.length) {
            return new bytes32[](0);
        }

        uint256 endIndex = offset + limit;
        if (endIndex > pairs.length) {
            endIndex = pairs.length;
        }

        ids = new bytes32[](endIndex - offset);
        for (uint256 i = 0; i < ids.length; i++) {
            IFermiEngine.PairInfo memory pair = pairs[offset + i];
            ids[i] = _poolId(pair.baseAsset, pair.quoteAsset);
        }
    }

    function _validatePoolTokens(
        bytes32 poolId,
        address sellToken,
        address buyToken
    ) internal pure {
        if (
            poolId != _poolId(sellToken, buyToken)
                && poolId != _poolId(buyToken, sellToken)
        ) {
            revert InvalidOrder("Pool/token mismatch");
        }
    }

    function _isPoolActive(bytes32 poolId, address sellToken, address buyToken)
        internal
        view
        returns (bool)
    {
        if (poolId == _poolId(sellToken, buyToken)) {
            return fermiSwapper.isActive(sellToken, buyToken);
        }

        return fermiSwapper.isActive(buyToken, sellToken);
    }

    function _amountSpecified(OrderSide side, uint256 specifiedAmount)
        internal
        pure
        returns (int256)
    {
        if (specifiedAmount > uint256(type(int256).max)) {
            revert LimitExceeded(uint256(type(int256).max));
        }

        // Bounded by int256.max above.
        // forge-lint: disable-next-line(unsafe-typecast)
        int256 signedAmount = int256(specifiedAmount);
        return side == OrderSide.Sell ? signedAmount : -signedAmount;
    }

    function _poolId(address baseAsset, address quoteAsset)
        internal
        pure
        returns (bytes32)
    {
        return keccak256(abi.encodePacked(baseAsset, quoteAsset));
    }
}

interface IFermiEngine {
    struct PairInfo {
        address baseAsset;
        address quoteAsset;
        bool isActive;
    }

    function traderVault() external view returns (address payable);

    function isActive(address baseAsset, address quoteAsset)
        external
        view
        returns (bool);

    function getPairs() external view returns (PairInfo[] memory);

    function swap(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified,
        address sender
    ) external returns (uint256 amountIn, uint256 amountOut);

    function quote(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified,
        address sender
    ) external view returns (uint256 amountIn, uint256 amountOut);
}

interface IFermiSwapper {
    event FermiSwap(
        address indexed recipient,
        address indexed tokenIn,
        address indexed tokenOut,
        uint256 amountIn,
        uint256 amountOut
    );

    function fermi() external view returns (IFermiEngine);
    function traderVault() external view returns (address payable);

    function quoteAmounts(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified
    ) external view returns (uint256 amountIn, uint256 amountOut);

    function isActive(address baseAsset, address quoteAsset)
        external
        view
        returns (bool);

    function getPairs() external view returns (IFermiEngine.PairInfo[] memory);

    function fermiSwapWithAllowances(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified,
        uint256 amountCheck,
        address recipient
    ) external returns (uint256 amountIn, uint256 amountOut);

    function fermiSwapWithCallback(
        address tokenIn,
        address tokenOut,
        int256 amountSpecified,
        uint256 amountCheck,
        address recipient,
        bytes calldata callbackData
    ) external returns (uint256 amountIn, uint256 amountOut);
}
