// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity ^0.8.13;

import {ISwapAdapter} from "src/interfaces/ISwapAdapter.sol";

contract BaselineSwapAdapter is ISwapAdapter {
    address public immutable relay;
    // Keep Tycho's simulation probes away from pool exhaustion paths.
    uint256 internal constant LIMIT_FRACTION_DENOMINATOR = 10;

    error ERC20CallFailed(address token, bytes data);

    constructor(address relay_) {
        relay = relay_;
    }

    // TODO can add later using curvelib calculation
    function price(bytes32, address, address, uint256[] memory)
        external
        pure
        override
        returns (Fraction[] memory)
    {
        revert NotImplemented("BaselineSwapAdapter.price");
    }

    function swap(
        bytes32 poolId,
        address sellToken,
        address buyToken,
        OrderSide side,
        uint256 specifiedAmount
    ) external override returns (Trade memory trade) {
        if (specifiedAmount == 0) {
            trade.price = Fraction(0, 1);
            return trade;
        }

        (address bToken, address reserve) = _poolTokens(poolId);

        bool bTokenBuy = buyToken == bToken && sellToken == reserve;
        bool bTokenSell = sellToken == bToken && buyToken == reserve;

        // NOTE: OrderSide determines if amount is input or output
        if (bTokenBuy) {
            if (side == OrderSide.Sell) {
                (trade.calculatedAmount, trade.gasUsed) =
                    _buyExactIn(bToken, sellToken, specifiedAmount);
            } else {
                (trade.calculatedAmount, trade.gasUsed) =
                    _buyExactOut(bToken, sellToken, specifiedAmount);
            }
        } else if (bTokenSell) {
            if (side == OrderSide.Sell) {
                (trade.calculatedAmount, trade.gasUsed) =
                    _sellExactIn(bToken, buyToken, specifiedAmount);
            } else {
                (trade.calculatedAmount, trade.gasUsed) =
                    _sellExactOut(bToken, buyToken, specifiedAmount);
            }
        } else {
            revert InvalidOrder("Token pair does not match bToken pool");
        }

        // PriceFunction is not advertised, so Tycho will estimate price from
        // swap behavior. Use the documented zero marker for unavailable price.
        trade.price = Fraction(0, 1);
    }

    function getLimits(bytes32 poolId, address sellToken, address buyToken)
        external
        view
        override
        returns (uint256[] memory limits)
    {
        (address bToken, address reserve) = _poolTokens(poolId);
        _validatePair(bToken, reserve, sellToken, buyToken);

        limits = new uint256[](2);
        limits[0] = _tokenLimit(bToken, reserve, sellToken);
        limits[1] = _tokenLimit(bToken, reserve, buyToken);
    }

    function getCapabilities(bytes32 poolId, address sellToken, address buyToken)
        external
        view
        override
        returns (Capability[] memory capabilities)
    {
        (address bToken, address reserve) = _poolTokens(poolId);
        _validatePair(bToken, reserve, sellToken, buyToken);

        capabilities = new Capability[](2);
        capabilities[0] = Capability.SellOrder;
        capabilities[1] = Capability.BuyOrder;
    }

    function getTokens(bytes32)
        external
        pure
        override
        returns (address[] memory)
    {
        revert NotImplemented("BaselineSwapAdapter.getTokens");
    }

    function getPoolIds(uint256, uint256)
        external
        pure
        override
        returns (bytes32[] memory)
    {
        revert NotImplemented("BaselineSwapAdapter.getPoolIds");
    }

    function _buyExactIn(address bToken, address reserve, uint256 amountIn)
        internal
        returns (uint256 amountOut, uint256 gasUsed)
    {
        _safeCall(
            reserve,
            abi.encodeCall(
                IERC20.transferFrom, (msg.sender, address(this), amountIn)
            )
        );
        _approveRelay(reserve, amountIn);
        uint256 gasBefore = gasleft();
        (amountOut,) =
            IBaselineRelay(relay).buyTokensExactIn(bToken, amountIn, 0);
        gasUsed = gasBefore - gasleft();
        _safeCall(
            bToken, abi.encodeCall(IERC20.transfer, (msg.sender, amountOut))
        );
    }

    function _sellExactIn(address bToken, address reserve, uint256 amountIn)
        internal
        returns (uint256 amountOut, uint256 gasUsed)
    {
        _safeCall(
            bToken,
            abi.encodeCall(
                IERC20.transferFrom, (msg.sender, address(this), amountIn)
            )
        );
        _approveRelay(bToken, amountIn);
        uint256 gasBefore = gasleft();
        (amountOut,) =
            IBaselineRelay(relay).sellTokensExactIn(bToken, amountIn, 0);
        gasUsed = gasBefore - gasleft();
        _safeCall(
            reserve, abi.encodeCall(IERC20.transfer, (msg.sender, amountOut))
        );
    }

    function _buyExactOut(address bToken, address reserve, uint256 amountOut)
        internal
        returns (uint256 amountIn, uint256 gasUsed)
    {
        (amountIn,,) = IBaselineRelay(relay).quoteBuyExactOut(bToken, amountOut);
        _safeCall(
            reserve,
            abi.encodeCall(
                IERC20.transferFrom, (msg.sender, address(this), amountIn)
            )
        );
        _approveRelay(reserve, amountIn);
        uint256 gasBefore = gasleft();
        (amountIn,) = IBaselineRelay(relay)
            .buyTokensExactOut(bToken, amountOut, amountIn);
        gasUsed = gasBefore - gasleft();
        _safeCall(
            bToken, abi.encodeCall(IERC20.transfer, (msg.sender, amountOut))
        );
    }

    function _sellExactOut(address bToken, address reserve, uint256 amountOut)
        internal
        returns (uint256 amountIn, uint256 gasUsed)
    {
        (amountIn,,) =
            IBaselineRelay(relay).quoteSellExactOut(bToken, amountOut);
        _safeCall(
            bToken,
            abi.encodeCall(
                IERC20.transferFrom, (msg.sender, address(this), amountIn)
            )
        );
        _approveRelay(bToken, amountIn);
        uint256 gasBefore = gasleft();
        (amountIn,) = IBaselineRelay(relay)
            .sellTokensExactOut(bToken, amountOut, amountIn);
        gasUsed = gasBefore - gasleft();
        _safeCall(
            reserve, abi.encodeCall(IERC20.transfer, (msg.sender, amountOut))
        );
    }

    function _approveRelay(address token, uint256 amount) internal {
        _safeCall(token, abi.encodeCall(IERC20.approve, (relay, 0)));
        _safeCall(token, abi.encodeCall(IERC20.approve, (relay, amount)));
    }

    function _tokenLimit(address bToken, address reserve, address token)
        internal
        view
        returns (uint256)
    {
        if (token == bToken) {
            return IBaselineRelay(relay).totalBTokens(bToken)
                / LIMIT_FRACTION_DENOMINATOR;
        }

        if (token == reserve) {
            return IBaselineRelay(relay).totalReserves(bToken)
                / LIMIT_FRACTION_DENOMINATOR;
        }

        revert InvalidOrder("Token pair does not match bToken pool");
    }

    function _poolTokens(bytes32 poolId)
        internal
        view
        returns (address bToken, address reserve)
    {
        bToken = _bToken(poolId);
        if (bToken == address(0)) revert InvalidOrder("Invalid bToken pool id");

        reserve = IBaselineRelay(relay).reserve(bToken);
        if (reserve == address(0)) {
            revert InvalidOrder("Invalid reserve token");
        }
    }

    function _validatePair(
        address bToken,
        address reserve,
        address sellToken,
        address buyToken
    ) internal pure {
        if (
            !(
                (sellToken == reserve && buyToken == bToken)
                    || (sellToken == bToken && buyToken == reserve)
            )
        ) {
            revert InvalidOrder("Token pair does not match bToken pool");
        }
    }

    function _safeCall(address token, bytes memory callData) internal {
        (bool success, bytes memory returnData) = token.call(callData);
        if (
            !success
                || (returnData.length != 0 && !abi.decode(returnData, (bool)))
        ) {
            revert ERC20CallFailed(token, callData);
        }
    }

    function _bToken(bytes32 poolId) internal pure returns (address) {
        // forge-lint: disable-next-line(unsafe-typecast)
        return address(bytes20(poolId));
    }
}

interface IBaselineRelay {
    function buyTokensExactIn(
        address bToken,
        uint256 amountIn,
        uint256 limitAmount
    ) external returns (uint256 amountOut, uint256 feesReceived);

    function buyTokensExactOut(
        address bToken,
        uint256 amountOut,
        uint256 limitAmount
    ) external returns (uint256 amountIn, uint256 feesReceived);

    function quoteBuyExactOut(address bToken, uint256 amountOut)
        external
        view
        returns (uint256 amountIn, uint256 feesReceived, uint256 slippage);

    function quoteSellExactOut(address bToken, uint256 amountOut)
        external
        view
        returns (uint256 amountIn, uint256 feesReceived, uint256 slippage);

    function reserve(address bToken) external view returns (address);

    function totalBTokens(address bToken) external view returns (uint256);

    function totalReserves(address bToken) external view returns (uint256);

    function sellTokensExactIn(
        address bToken,
        uint256 amountIn,
        uint256 limitAmount
    ) external returns (uint256 amountOut, uint256 feesReceived);

    function sellTokensExactOut(
        address bToken,
        uint256 amountOut,
        uint256 limitAmount
    ) external returns (uint256 amountIn, uint256 feesReceived);
}

interface IERC20 {
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount)
        external
        returns (bool);
}
