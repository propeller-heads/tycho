// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

/// @title IPropAMM
/// @notice Standard interface proprietary AMMs (pAMMs) implement to be
/// routable, as pushed by Titan Builder.
/// @dev Callers use a push-payment model: before calling `swap` they transfer
/// `amountIn` of `tokenIn` to the propAMM, then `swap` is expected to consume
/// that balance.
interface IPropAMM {
    /// @notice Emitted once per successful swap after `tokenOut` is delivered
    /// to `recipient`.
    /// @param sender The address that invoked the swap entrypoint and supplied
    /// `amountIn` of `tokenIn`. Indexed so consumers can fetch a given
    /// account's recent swaps.
    /// @param tokenIn The token sold.
    /// @param tokenOut The token bought.
    /// @param amountIn The exact amount of `tokenIn` pulled from `sender`.
    /// @param amountOut The amount of `tokenOut` delivered to `recipient`,
    /// measured as a balance delta.
    /// @param recipient The address that received `tokenOut`.
    event Swapped(
        address indexed sender,
        address indexed tokenIn,
        address indexed tokenOut,
        uint256 amountIn,
        uint256 amountOut,
        address recipient
    );

    /// @notice A token pair a propAMM supports.
    struct TokenPair {
        address token0;
        address token1;
    }

    /// @notice Returns true if the propAMM can swap `tokenIn` for `tokenOut`
    /// in the current block.
    /// @dev A `view` fast-path a router can use to skip inactive propAMMs
    /// before paying for a full `quote`. Reported per-pair so a propAMM can be
    /// live for some pairs and not others.
    /// @param tokenIn The address of the token being sold.
    /// @param tokenOut The address of the token being bought.
    /// @return active True if a swap for the pair would succeed right now.
    function isActive(address tokenIn, address tokenOut)
        external
        view
        returns (bool active);

    /// @notice Returns all token pairs the propAMM supports, both active and
    /// inactive.
    /// @dev Advisory, for off-chain discovery; routers do not call this on
    /// their swap path.
    /// @return pairs The supported pairs.
    function getPairs() external view returns (TokenPair[] memory pairs);

    /// @notice Quotes `amountIn` of `tokenIn` and returns the `tokenOut`
    /// amount a swap would deliver.
    /// @dev MUST revert if the propAMM is inactive for the pair.
    /// MUST NOT require a `tokenIn` balance or allowance from the caller.
    /// @param tokenIn The address of the token being sold.
    /// @param tokenOut The address of the token being bought.
    /// @param amountIn The exact amount of `tokenIn` to quote against.
    /// @return amountOut The amount of `tokenOut` the swap would deliver.
    function quote(address tokenIn, address tokenOut, uint256 amountIn)
        external
        returns (uint256 amountOut);

    /// @notice Swaps an exact `amountIn` of `tokenIn` for as much `tokenOut`
    /// as possible, delivering it to `recipient`.
    /// @dev Expects `amountIn` of `tokenIn` to have ALREADY been transferred
    /// to the propAMM by the caller (push-payment).
    /// SHALL revert if it cannot deliver at least `minAmountOut` of `tokenOut`
    /// to `recipient`.
    /// @param tokenIn The address of the token being sold.
    /// @param tokenOut The address of the token being bought.
    /// @param amountIn The exact amount of `tokenIn` to sell.
    /// @param minAmountOut The minimum acceptable amount of `tokenOut`.
    /// @param recipient The address that will receive `tokenOut`.
    /// @param deadline Unix timestamp after which the swap is no longer valid.
    /// @return amountOut The amount of `tokenOut` received by `recipient`.
    function swap(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 minAmountOut,
        address recipient,
        uint256 deadline
    ) external returns (uint256 amountOut);
}
