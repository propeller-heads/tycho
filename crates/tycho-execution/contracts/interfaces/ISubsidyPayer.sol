// SPDX-License-Identifier: LicenseRef-Fynd-License-1.1
pragma solidity ^0.8.26;

/// @notice A subsidy signed off-chain for one hop of a swap. A subsidy
///         executor in the router wraps the hop's executor (the inner
///         executor) and calls `pay` after it runs.
struct Subsidy {
    // Must be the router's caller or tx.origin, or `pay` skips with
    // CallerMismatch
    address boundTo;
    // tokenOut of the hop; ETH_ADDRESS for native ETH
    address token;
    uint256 amount;
    // Smallest hop input that earns the subsidy
    uint256 minAmountIn;
    // keccak256(abi.encodePacked(innerExecutor, innerData)): the inner
    // executor's address and the swap data the Dispatcher passes to it
    bytes32 innerSwapHash;
    uint256 nonce;
    uint256 deadline;
}

interface ISubsidyPayer {
    enum SkipReason {
        // No reason: the subsidy can be paid. Never emitted.
        None,
        // The payer is paused
        Paused,
        // block.timestamp is past the deadline
        Expired,
        // The signature does not recover to `signer`
        InvalidSignature,
        // boundTo is neither the router's caller nor tx.origin
        CallerMismatch,
        // The hop's input is below minAmountIn
        BelowMinAmountIn,
        // The nonce was paid or invalidated
        NonceUsed,
        // The payer's balance of `token` is below `amount`, or its
        // `balanceOf` call failed
        InsufficientBalance,
        // The token or ETH transfer to the receiver failed
        TransferFailed
    }

    event SubsidyPaid(
        uint256 indexed nonce,
        address indexed boundTo,
        address to,
        address indexed token,
        uint256 amount
    );
    event SubsidySkipped(uint256 indexed nonce, SkipReason reason);

    /// @notice Pays one signed subsidy to `to`, or skips it.
    /// @dev The caller must check that `token` and `innerSwapHash` match
    ///      the hop it ran; `pay` does not check them.
    /// @param subsidy The subsidy terms that `signature` covers.
    /// @param hopAmountIn Input amount of the hop, checked against
    ///        `subsidy.minAmountIn`.
    /// @param routerMsgSender Whoever called the router (the router's own
    ///        `msg.sender`), as passed by the subsidy executor from its
    ///        delegatecall context; matched against `subsidy.boundTo`.
    /// @param to Receives the subsidy; it is where the Dispatcher measures
    ///        the hop's output. It is not signed, because only the router
    ///        calls `pay` and the router's executor code picks `to`.
    /// @param signature EIP-712 signature of `subsidy` by `signer`.
    /// @return paid False when `pay` skipped the subsidy and emitted
    ///         `SubsidySkipped` with its `SkipReason`. `pay` reverts only
    ///         when the caller is not the router.
    function pay(
        Subsidy calldata subsidy,
        uint256 hopAmountIn,
        address routerMsgSender,
        address to,
        bytes calldata signature
    ) external returns (bool paid);
}
