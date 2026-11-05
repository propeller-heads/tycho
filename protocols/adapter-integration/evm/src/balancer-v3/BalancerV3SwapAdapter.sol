// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.26;

import "./lib/BalancerSwapHelpers.sol";

/**
 * @title Balancer V3 Swap Adapter
 * @dev Supports:
 * Direct Swaps:
 * - ETH<->ERC20
 * - ERC20<->ERC20
 * - ERC4626<->ERC4626
 * - ERC4626<->ERC20
 *
 * 2 steps:
 * - (ERC20->ERC20)->ERC4626: swap, wrap_0
 * - (ERC4626->ERC20)->ERC4626: swap, wrap_1
 *
 * - (ERC4626->ERC4626)->ERC20: swap, unwrap_0
 * - (ERC20->ERC4626)->ERC20; swap, unwrap_1
 *
 * - ERC20->(ERC4626->ERC4626): wrap, swap_0
 * - ERC20->(ERC4626->ERC20); wrap, swap_1
 *
 * - ERC4626->(ERC20->ERC20): unwrap, swap_0
 * - ERC4626->(ERC20->ERC4626): unwrap, swap_1
 *
 * 3 steps:
 * - ERC20->(ERC4626->ERC4626)->ERC20
 * - ERC4626->(ERC20->ERC20)->ERC4626
 */
contract BalancerV3SwapAdapter is BalancerSwapHelpers {
    constructor(
        address payable vault_,
        address _router,
        address _permit2,
        address _WETH_ADDRESS
    ) {
        vault = IVault(vault_);
        router = IBatchRouter(_router);
        permit2 = _permit2;
        WETH_ADDRESS = _WETH_ADDRESS;
    }

    /// @dev Enable ETH receiving
    receive() external payable {}

    /// @inheritdoc ISwapAdapter
    function price(
        bytes32 _poolId,
        address _sellToken,
        address _buyToken,
        uint256[] memory _specifiedAmounts
    ) external override returns (Fraction[] memory _prices) {
        _prices = new Fraction[](_specifiedAmounts.length);

        for (uint256 i = 0; i < _specifiedAmounts.length; i++) {
            _prices[i] = getPriceAt(
                _poolId, _sellToken, _buyToken, _specifiedAmounts[i]
            );
        }
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
            // Price defaults to Fraction(0, 0) which breaks simulation. We need
            // to explicitly set it.
            trade.price = Fraction(0, 1);
            return trade;
        }

        uint256 gasBefore = gasleft();

        // perform swap (forward to middleware)
        trade.calculatedAmount =
            swapMiddleware(poolId, sellToken, buyToken, side, specifiedAmount);

        trade.gasUsed = gasBefore - gasleft();

        // as post-trade price cannot be calculated in an external call, we
        // return the trade price here
        trade.price = Fraction(trade.calculatedAmount, specifiedAmount);
    }

    /// @inheritdoc ISwapAdapter
    function getLimits(bytes32 poolId, address sellToken, address buyToken)
        external
        view
        override
        returns (uint256[] memory limits)
    {
        limits = getLimitsMiddleware(poolId, sellToken, buyToken);
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
        capabilities[3] = Capability.HardLimits;
    }

    /// @inheritdoc ISwapAdapter
    function getTokens(bytes32 poolId)
        external
        view
        override
        returns (address[] memory tokens)
    {
        address poolAddress = address(bytes20(poolId));
        // Is accessing to vault to get the tokens of a pool / Here could be
        // where it was reverting the test
        IERC20[] memory tokens_ = vault.getPoolTokens(poolAddress);
        tokens = new address[](tokens_.length);

        for (uint256 i = 0; i < tokens_.length; i++) {
            tokens[i] = address(tokens_[i]);
        }
    }

    function getPoolIds(uint256, uint256)
        external
        pure
        override
        returns (bytes32[] memory)
    {
        revert NotImplemented("BalancerV3SwapAdapter.getPoolIds");
    }

    /**
     * @dev Returns the price of the swap
     * @dev The price is not scaled by the token decimals
     * @param pool The ID of the trading pool.
     * @param sellToken The token being sold.
     * @param buyToken The token being bought.
     * @param specifiedAmount The amount to be traded.
     */
    function getPriceAt(
        bytes32 pool,
        address sellToken,
        address buyToken,
        uint256 specifiedAmount
    ) internal returns (Fraction memory calculatedPrice) {
        calculatedPrice = Fraction(
            getAmountOutMiddleware(pool, sellToken, buyToken, specifiedAmount),
            specifiedAmount
        );
    }
}
