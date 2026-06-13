pragma solidity ^0.8.27;

/// @title PartyPool - LMSR-backed multi-asset pool with LP ERC20 token
/// @notice A multi-asset liquidity pool backed by the LMSRStabilized pricing
/// model. The pool issues an ERC20 LP token representing proportional
/// ownership.
/// It supports:
/// - Proportional minting and burning of LP _tokens,
/// - Single-token mint (swapMint) and single-asset withdrawal (burnSwap),
/// - Exact-input swaps and swaps-to-price-limits,
/// - Flash loans via a callback interface.
interface IPartyPool {
    /// @notice If a security problem is found, the vault owner may call this
    /// function to permanently disable swap and mint functionality, leaving
    /// only burns (withdrawals) working.
    function killed() external view returns (bool);

    /// @notice Returns the list of all token addresses in the pool (copy).
    function allTokens() external view returns (address[] memory);

    /// @notice Swap input token inputTokenIndex -> token outputTokenIndex.
    /// @param payer address of the account that pays for the swap
    /// @param fundingSelector USE_APPROVALS: payer pre-approves the pool.
    /// USE_PREFUNDING: tokens already sent to the pool. Any other selector:
    /// callback invoked on payer.
    /// @param receiver address that will receive the output tokens
    /// @param inputTokenIndex index of input asset
    /// @param outputTokenIndex index of output asset
    /// @param maxAmountIn exact input to transfer (fee is on the output side,
    /// not added to the input)
    /// @param minAmountOut minimum net output tokens to receive; reverts with
    /// "slippage control" if not met (0 = disabled)
    /// @param deadline timestamp after which the call reverts; pass 0 to ignore
    /// @param unwrap if true, native wrapper output is unwrapped
    /// @param cbData callback data for callback-style fundingSelectors
    /// @return amountIn actual input transferred (uint256), amountOut net
    /// output sent to the receiver (uint256), outFee fee taken from the gross
    /// output
    /// (uint256)
    function swap(
        address payer,
        bytes4 fundingSelector,
        address receiver,
        uint256 inputTokenIndex,
        uint256 outputTokenIndex,
        uint256 maxAmountIn,
        uint256 minAmountOut,
        uint256 deadline,
        bool unwrap,
        bytes memory cbData
    )
        external
        payable
        returns (uint256 amountIn, uint256 amountOut, uint256 outFee);
}
