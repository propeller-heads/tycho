// SPDX-License-Identifier: AGPL-3.0-or-later
pragma experimental ABIEncoderV2;
pragma solidity ^0.8.13;

import {ISwapAdapter} from "src/interfaces/ISwapAdapter.sol";
import {
    IERC20,
    SafeERC20
} from "openzeppelin-contracts/contracts/token/ERC20/utils/SafeERC20.sol";
import {ERC20} from "openzeppelin-contracts/contracts/token/ERC20/ERC20.sol";
import "src/libraries/FractionMath.sol";

/// @dev custom RESERVE_LIMIT_FACTOR for limits for this adapter(underestimate)
uint256 constant RESERVE_LIMIT_FACTOR = 2;

/// @title Curve Finance Adapter
/// @dev This contract supports both CryptoSwap and StableSwap Curve pools
contract CurveAdapter is ISwapAdapter {
    using SafeERC20 for IERC20;
    using FractionMath for Fraction;

    struct SellParamsCache {
        address poolAddress; // address of the pool to swap in
        address sellToken; // address of the token to sell
        address buyToken; // address of the token to buy
        int128 sellTokenIndex; // index of the token being sold
        int128 buyTokenIndex; // index of the token being bought
        uint256 specifiedAmount; // amount to trade
        bool isInt128Pool; // pool is int128
    }

    struct PoolCoins {
        address[8] addresses;
        uint256 coinsLength;
    }

    uint256 constant PRECISION = 10 ** 5;

    address constant WETH_ADDRESS = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant ETH_ADDRESS = 0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE;

    constructor() {}

    /// @dev enable receive as this contract supports ETH
    receive() external payable {}

    /// @inheritdoc ISwapAdapter
    function price(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        uint256[] memory specifiedAmounts
    ) external override returns (Fraction[] memory prices) {
        revert NotImplemented("CurveAdapter.price");
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

        SellParamsCache memory sellParams;
        {
            sellParams.poolAddress = address(bytes20(poolId));
            sellParams.sellToken = sellToken;
            sellParams.buyToken = buyToken;
            sellParams.specifiedAmount = specifiedAmount;

            bool isEthPool; // pool is native ETH pool
            PoolCoins memory coins = getCoins(sellParams.poolAddress);
            sellParams.isInt128Pool =
                isInt128Pool(sellParams.poolAddress, coins);

            /// @dev Support for Native ETH pools, ETH pools cannot be Meta
            /// therefore we can directly access coins without using underlying
            if (sellToken == address(0)) {
                for (uint256 i = 0; i < coins.coinsLength; i++) {
                    if (
                        coins.addresses[i] == ETH_ADDRESS
                            || coins.addresses[i] == WETH_ADDRESS
                    ) {
                        sellParams.sellToken = ETH_ADDRESS;
                        if (coins.addresses[i] == ETH_ADDRESS) {
                            isEthPool = true;
                        }
                        break;
                    }
                }
            } else if (buyToken == address(0)) {
                for (uint256 i = 0; i < coins.coinsLength; i++) {
                    if (
                        coins.addresses[i] == ETH_ADDRESS
                            || coins.addresses[i] == WETH_ADDRESS
                    ) {
                        sellParams.buyToken = ETH_ADDRESS;
                        if (coins.addresses[i] == ETH_ADDRESS) {
                            isEthPool = true;
                        }
                        break;
                    }
                }
            }

            (sellParams.sellTokenIndex, sellParams.buyTokenIndex) =
                getCoinsIndices(
                    sellParams.sellToken, sellParams.buyToken, coins, isEthPool
                );
        }

        if (side == OrderSide.Sell) {
            (trade.calculatedAmount, trade.gasUsed) = sell(sellParams);
        } else {
            revert Unavailable("OrderSide.Buy is not available for this adapter");
        }

        trade.price = getPriceAt(sellParams, true);
    }

    /// @inheritdoc ISwapAdapter
    function getLimits(bytes32 poolId, address sellToken, address buyToken)
        external
        view
        override
        returns (uint256[] memory limits)
    {
        address poolAddress = address(bytes20(poolId));
        ICurveStableSwapPool pool = ICurveStableSwapPool(poolAddress);
        address sellToken_ = sellToken;
        address buyToken_ = buyToken;
        bool isEthPool;
        PoolCoins memory coins = getCoins(poolAddress);

        /// @dev Support for Native ETH pools, ETH pools cannot be Meta
        /// therefore we can directly access coins without using underlying
        if (sellToken == address(0)) {
            for (uint256 i = 0; i < coins.coinsLength; i++) {
                if (
                    coins.addresses[i] == ETH_ADDRESS
                        || coins.addresses[i] == WETH_ADDRESS
                ) {
                    sellToken_ = ETH_ADDRESS;
                    if (coins.addresses[i] == ETH_ADDRESS) {
                        isEthPool = true;
                    }
                    break;
                }
            }
        } else if (buyToken == address(0)) {
            for (uint256 i = 0; i < coins.coinsLength; i++) {
                if (
                    coins.addresses[i] == ETH_ADDRESS
                        || coins.addresses[i] == WETH_ADDRESS
                ) {
                    buyToken_ = ETH_ADDRESS;
                    if (coins.addresses[i] == ETH_ADDRESS) {
                        isEthPool = true;
                    }
                    break;
                }
            }
        }

        (int128 sellTokenIndex, int128 buyTokenIndex) =
            getCoinsIndices(sellToken_, buyToken_, coins, isEthPool);

        limits = new uint256[](2);
        uint256 sellTokenIndexUint = uint256(uint128(sellTokenIndex));
        uint256 buyTokenIndexUint = uint256(uint128(buyTokenIndex));
        try pool.balances(sellTokenIndexUint) returns (uint256 bal) {
            limits[0] = bal / RESERVE_LIMIT_FACTOR;
            limits[1] = pool.balances(buyTokenIndexUint) / RESERVE_LIMIT_FACTOR;
        } catch {
            limits[0] = ICurveCustomInt128Pool(poolAddress)
                .balances(sellTokenIndex) / RESERVE_LIMIT_FACTOR;
            limits[1] = ICurveCustomInt128Pool(poolAddress)
                .balances(buyTokenIndex) / RESERVE_LIMIT_FACTOR;
        }
    }

    /// @inheritdoc ISwapAdapter
    function getCapabilities(bytes32, address, address)
        external
        pure
        override
        returns (Capability[] memory capabilities)
    {
        capabilities = new Capability[](1);
        capabilities[0] = Capability.SellOrder;
    }

    /// @inheritdoc ISwapAdapter
    function getTokens(bytes32 poolId)
        external
        view
        override
        returns (address[] memory tokens)
    {
        PoolCoins memory coins = getCoins(address(bytes20(poolId)));
        address[] memory tokensTmp = new address[](coins.coinsLength);
        bool containsETH;
        for (uint256 j = 0; j < coins.coinsLength; j++) {
            if (coins.addresses[j] == WETH_ADDRESS) {
                containsETH = true;
            }
            if (coins.addresses[j] == ETH_ADDRESS) {
                continue;
            }
            tokensTmp[j] = coins.addresses[j];
        }

        if (containsETH) {
            tokens = new address[](coins.coinsLength + 1);
            for (uint256 k = 0; k < coins.coinsLength; k++) {
                tokens[k] = tokensTmp[k];
            }
            tokens[coins.coinsLength] = address(0);
        } else {
            tokens = tokensTmp;
        }
    }

    function getPoolIds(uint256, uint256)
        external
        pure
        override
        returns (bytes32[] memory)
    {
        revert NotImplemented("CurveAdapter.getPoolIds");
    }

    /// @notice Calculates pool prices for specified amounts
    /// @param sellParams Params for the price(see: struct SellParamsCache).
    /// @param useGenericAmount Determine if a amount used to determine the
    /// price is a small amount of the reserve(true) or
    /// sellParams.specifiedAmount(false)
    /// @return (Fraction) price as a fraction corresponding to the provided
    /// amount.
    function getPriceAt(
        SellParamsCache memory sellParams,
        bool useGenericAmount
    ) internal view returns (Fraction memory) {
        uint256 amountIn;
        uint256 sellTokenIndexUint = uint256(uint128(sellParams.sellTokenIndex));
        uint256 buyTokenIndexUint = uint256(uint128(sellParams.buyTokenIndex));
        if (sellParams.isInt128Pool) {
            try ICurveStableSwapPool(sellParams.poolAddress)
                .balances(sellTokenIndexUint) returns (
                uint256 bal
            ) {
                amountIn = useGenericAmount
                    ? (bal / PRECISION)
                    : sellParams.specifiedAmount;
            } catch {
                amountIn = useGenericAmount
                    ? (ICurveCustomInt128Pool(sellParams.poolAddress)
                                .balances(sellParams.sellTokenIndex)
                            / PRECISION)
                    : sellParams.specifiedAmount;
            }
            return Fraction(
                ICurveStableSwapPool(sellParams.poolAddress)
                    .get_dy(
                        sellParams.sellTokenIndex,
                        sellParams.buyTokenIndex,
                        amountIn
                    ),
                amountIn
            );
        } else {
            amountIn = useGenericAmount
                ? (ICurveCryptoSwapPool(sellParams.poolAddress)
                            .balances(sellTokenIndexUint) / PRECISION)
                : sellParams.specifiedAmount;

            return Fraction(
                ICurveCryptoSwapPool(sellParams.poolAddress)
                    .get_dy(sellTokenIndexUint, buyTokenIndexUint, amountIn),
                amountIn
            );
        }
    }

    /// @notice Executes a sell order on a given pool.
    /// @dev Only metapools available (with LP token as counter pair) are
    /// Stable,
    /// but after some research we've found that Curve deployed some pools that
    /// are Crypto and use the int128 interface, therefore we optimistically
    /// support them too.
    /// @param sellParams Params for the trade(see: struct SellParamsCache).
    /// @return calculatedAmount The amount of tokens received.
    /// @return gasUsed Gas consumed by the pool exchange only (excludes
    /// token transfers and approvals).
    function sell(SellParamsCache memory sellParams)
        internal
        returns (uint256 calculatedAmount, uint256 gasUsed)
    {
        IERC20 buyToken = IERC20(sellParams.buyToken);
        IERC20 sellToken = IERC20(sellParams.sellToken);
        uint256 nativeTokenBalBefore = address(this).balance;
        uint256 buyTokenBalBefore = (sellParams.buyToken == ETH_ADDRESS)
            ? address(this).balance
            : buyToken.balanceOf(address(this));

        if (sellParams.isInt128Pool) {
            if (sellParams.sellToken == ETH_ADDRESS) {
                // ETH Pool
                uint256 gasBefore = gasleft();
                ICurveStableSwapPoolEth(sellParams.poolAddress)
                .exchange{value: sellParams.specifiedAmount}(
                    sellParams.sellTokenIndex,
                    sellParams.buyTokenIndex,
                    sellParams.specifiedAmount,
                    0
                );
                gasUsed = gasBefore - gasleft();
            } else {
                sellToken.safeTransferFrom(
                    msg.sender, address(this), sellParams.specifiedAmount
                );
                sellToken.safeIncreaseAllowance(
                    sellParams.poolAddress, sellParams.specifiedAmount
                );
                uint256 gasBefore = gasleft();
                ICurveStableSwapPool(sellParams.poolAddress)
                    .exchange(
                        sellParams.sellTokenIndex,
                        sellParams.buyTokenIndex,
                        sellParams.specifiedAmount,
                        0
                    );
                gasUsed = gasBefore - gasleft();
            }
        } else {
            uint256 sellTokenIndexUint =
                uint256(uint128(sellParams.sellTokenIndex));
            uint256 buyTokenIndexUint =
                uint256(uint128(sellParams.buyTokenIndex));
            if (sellParams.sellToken == ETH_ADDRESS) {
                uint256 gasBefore = gasleft();
                ICurveCryptoSwapPoolEth(sellParams.poolAddress)
                .exchange{value: sellParams.specifiedAmount}(
                    sellTokenIndexUint,
                    buyTokenIndexUint,
                    sellParams.specifiedAmount,
                    0,
                    true,
                    address(this)
                );
                gasUsed = gasBefore - gasleft();
            } else {
                sellToken.safeTransferFrom(
                    msg.sender, address(this), sellParams.specifiedAmount
                );
                sellToken.safeIncreaseAllowance(
                    sellParams.poolAddress, sellParams.specifiedAmount
                );
                // @dev if available try to swap with use_eth set to true.
                uint256 gasBefore = gasleft();
                try ICurveCryptoSwapPoolEth(sellParams.poolAddress)
                    .exchange(
                        sellTokenIndexUint,
                        buyTokenIndexUint,
                        sellParams.specifiedAmount,
                        0,
                        true,
                        address(this)
                    ) {
                    gasUsed = gasBefore - gasleft();
                    // @dev we can't use catch here because some Curve pool have
                    // a fallback function implemented. So this call succeed
                    // without doing anything.
                    uint256 maybeNativeReceived =
                        address(this).balance - nativeTokenBalBefore;
                    if (maybeNativeReceived > 0) {
                        calculatedAmount = maybeNativeReceived; // ETH received
                        (bool sent,) = address(msg.sender)
                        .call{value: maybeNativeReceived}(
                            ""
                        );
                        require(sent, "Eth transfer failed");
                    } else {
                        calculatedAmount = buyToken.balanceOf(address(this))
                            - buyTokenBalBefore;
                        buyToken.safeTransfer(
                            address(msg.sender), calculatedAmount
                        );
                    }
                    if (calculatedAmount > 0) {
                        return (calculatedAmount, gasUsed);
                    }
                } catch {}
                // @dev else use the generic interface.
                gasBefore = gasleft();
                ICurveCryptoSwapPool(sellParams.poolAddress)
                    .exchange(
                        sellTokenIndexUint,
                        buyTokenIndexUint,
                        sellParams.specifiedAmount,
                        0
                    );
                gasUsed = gasBefore - gasleft();
            }
        }

        if (sellParams.buyToken == ETH_ADDRESS) {
            calculatedAmount = address(this).balance - buyTokenBalBefore;
            (bool sent,) = address(msg.sender).call{value: calculatedAmount}("");
            require(sent, "Eth transfer failed");
        } else {
            calculatedAmount =
                buyToken.balanceOf(address(this)) - buyTokenBalBefore;
            buyToken.safeTransfer(address(msg.sender), calculatedAmount);
        }
    }

    /// @dev Check whether a pool supports int128 inputs or uint256(excluded
    /// custom)
    /// @param poolAddress address of the pool
    /// @param coins list of coin addresses in the pool
    function isInt128Pool(address poolAddress, PoolCoins memory coins)
        internal
        view
        returns (bool)
    {
        // @dev We avoid using ETH/WETH as a token here because it might create
        // a requirement to index WETH when it's not needed.
        uint256 sampleTokenIndex = (coins.addresses[0] == ETH_ADDRESS
                || coins.addresses[0] == WETH_ADDRESS)
            ? 1
            : 0;
        uint256 sampleAmount =
            IERC20(coins.addresses[sampleTokenIndex]).balanceOf(poolAddress);

        try ICurveCryptoSwapPool(poolAddress)
            .get_dy(
                sampleTokenIndex == 0 ? 0 : 1,
                sampleTokenIndex == 0 ? 1 : 0,
                sampleAmount / 10
            ) returns (
            uint256
        ) {
            return false;
        } catch {
            return true;
        }
    }

    /// @dev Check whether a pool is a custom int128 pool(balances, coins, ...
    /// accept int128 as input)
    /// @param poolAddress address of the pool
    function isCustomInt128Pool(address poolAddress)
        internal
        view
        returns (bool)
    {
        try ICurveStableSwapPool(poolAddress).balances(0) returns (uint256) {
            return false;
        } catch {
            return true;
        }
    }

    /// @notice Get coins inside a pool
    /// @param poolAddress The address of the pool
    function getCoins(address poolAddress)
        internal
        view
        returns (PoolCoins memory output)
    {
        uint256 len;

        /// @dev as of registry, max addresses that can be included in a pool is
        /// always 8, therefore we limit the loop to it.
        if (!isCustomInt128Pool(poolAddress)) {
            // Pool with coins(uint256)
            for (len; len < 8; len++) {
                try ICurveStableSwapPool(poolAddress).coins(len) returns (
                    address coin
                ) {
                    output.addresses[len] = coin;
                    output.coinsLength++;
                } catch {
                    // Pool has no coins, or the last coin has been found
                    break;
                }
            }
        } else {
            for (len; len < 8; len++) {
                // Pool supports coins(int128)
                try ICurveCustomInt128Pool(poolAddress)
                    .coins(int128(uint128(len))) returns (
                    address coin
                ) {
                    output.addresses[len] = coin;
                    output.coinsLength++;
                } catch {
                    // Pool has no coins, or the last coin has been found
                    break;
                }
            }
        }
    }

    /// @notice Get indices of coins to swap
    /// @dev If the pool is meta the registry.get_coin_indices includes the
    /// underlying addresses (appended to the array from index 1 to length-1)
    /// @param sellToken The token being sold
    /// @param buyToken The token being bought
    /// @param coins output of getCoins()
    /// @param isEthPool determine if pool has native ETH inside
    function getCoinsIndices(
        address sellToken,
        address buyToken,
        PoolCoins memory coins,
        bool isEthPool
    ) internal pure returns (int128 sellTokenIndex, int128 buyTokenIndex) {
        address sellToken_ = sellToken;
        address buyToken_ = buyToken;
        if (sellToken == ETH_ADDRESS && !isEthPool) {
            sellToken_ = WETH_ADDRESS;
        }
        if (buyToken == ETH_ADDRESS && !isEthPool) {
            buyToken_ = WETH_ADDRESS;
        }
        for (uint256 i; i < coins.coinsLength; i++) {
            if (coins.addresses[i] == sellToken_) {
                sellTokenIndex = int128(uint128(i));
            } else if (coins.addresses[i] == buyToken_) {
                buyTokenIndex = int128(uint128(i));
            }
        }
    }
}

/// @dev Wrapped ported version of Curve Plain Pool to Solidity
/// For params informations see:
/// https://docs.curve.fi/cryptoswap-exchange/cryptoswap/pools/crypto-pool/
interface ICurveCryptoSwapPool {
    function get_dy(uint256 i, uint256 j, uint256 dx)
        external
        view
        returns (uint256);

    function exchange(uint256 i, uint256 j, uint256 dx, uint256 min_dy)
        external
        payable;

    function balances(uint256 arg0) external view returns (uint256);

    function fee() external view returns (uint256);
}

interface ICurveCryptoSwapPoolEth is ICurveCryptoSwapPool {
    function exchange(
        uint256 i,
        uint256 j,
        uint256 dx,
        uint256 min_dy,
        bool use_eth,
        address receiver
    ) external payable;
}

/// @dev Wrapped ported version of Curve Plain Pool to Solidity
/// For params informations see:
/// https://docs.curve.fi/stableswap-exchange/stableswap/pools/plain_pools/
interface ICurveStableSwapPool {
    function get_dy(int128 i, int128 j, uint256 dx)
        external
        view
        returns (uint256);

    function exchange(int128 i, int128 j, uint256 dx, uint256 min_dy) external;

    function balances(uint256 arg0) external view returns (uint256);

    function fee() external view returns (uint256);

    function coins(uint256 i) external view returns (address);
}

interface ICurveStableSwapPoolEth {
    function exchange(int128 i, int128 j, uint256 dx, uint256 min_dy)
        external
        payable;
}

/// @dev TODO future implementation, not used at the moment since StableSwap
/// Meta Pools are not supported yet
interface ICurveStableSwapMetaPool is ICurveStableSwapPool {
    function get_dy_underlying(int128 i, int128 j, uint256 dx)
        external
        view
        returns (uint256);

    function exchange_underlying(int128 i, int128 j, uint256 dx, uint256 min_dy)
        external
        returns (uint256);
}

interface ICurveCustomInt128Pool {
    function coins(int128 arg0) external view returns (address);
    function balances(int128 arg0) external view returns (uint256);
}
