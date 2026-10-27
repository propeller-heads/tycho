// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IERC20 {
    function balanceOf(address account) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
}

interface IForwarder {
    function forwardTransfer(address token, address to, uint256 amount) external returns (bool);
    function forwardApprove(address token, address spender, uint256 amount) external returns (bool);
}

/// @title Token Analyzer
/// @notice Injected at a token holder's address via eth_call state override. Simulates a full
/// round-trip ERC20 transfer (holder -> settlement -> recipient) in a single call and reports
/// balances, gas costs, and success flags. Also sells a slice of the received amount from the
/// settlement into each candidate pool, to expose taxes that only apply to transfers into a pool.
/// No on-chain deployment required.
/// @dev The inbound transfer uses a low-level call rather than a typed interface so that tokens with
/// non-standard transfer() implementations (e.g. USDT, which omits the bool return value) are
/// handled correctly. balanceOf and approve are called via the typed interface since they are
/// consistently implemented across tokens.
contract Analyzer {
    /// @notice Gas for one sell leg. A token that burns all gas it gets on a transfer into a pool
    /// must not starve the later legs and the approval check: 8 legs stay well below the 30M
    /// gas limit of the eth_call.
    uint256 internal constant SELL_LEG_GAS = 2_000_000;

    /// @notice One transfer of `sellAmount` from the settlement into a pool.
    /// @param ok             Whether forwardTransfer(token, pool, sellAmount) succeeded.
    /// @param balanceBefore  Pool balance before the transfer.
    /// @param balanceAfter   Pool balance after the transfer.
    struct SellLeg {
        bool ok;
        uint256 balanceBefore;
        uint256 balanceAfter;
    }

    /// @notice Simulate ERC20 transfer in and out, measuring balances and gas at each step.
    /// @param token   The ERC20 token to analyze.
    /// @param amount  The amount to transfer in from this address (the holder).
    /// @param settlement  Intermediary address (injected with Forwarder bytecode).
    /// @param recipient   Final recipient of the outbound transfer.
    /// @param pools   Candidate pools. Each receives `sellAmount` from the settlement after the
    ///                outbound transfer. The settlement keeps `sellAmount * pools.length` of the
    ///                received amount for this and sends only the rest to the recipient.
    /// @return transferInOk    Whether transfer(settlement, amount) succeeded.
    /// @return transferOutOk   Whether forwardTransfer(token, recipient, received - sell budget)
    ///                         succeeded.
    /// @return approvalOk      Whether forwardApprove(token, recipient, MAX_UINT256) succeeded.
    /// @return balanceBeforeIn Settlement balance before transfer in.
    /// @return balanceAfterIn  Settlement balance after transfer in.
    /// @return balanceAfterOut Settlement balance after transfer out, before the sell legs.
    /// @return recipientBefore Recipient balance before any transfer.
    /// @return recipientAfter  Recipient balance after transfer out.
    /// @return gasIn   Gas consumed by the inbound transfer (gasleft() delta).
    /// @return gasOut  Gas consumed by the outbound transfer (gasleft() delta).
    /// @return sellAmount  Amount sent into each pool: received / (2 * pools.length), or 0 when
    ///                     there are no pools. No sell legs run when it is 0.
    /// @return sellLegs    One entry per pool, in `pools` order. Empty when no sell legs ran.
    function analyze(
        address token,
        uint256 amount,
        address settlement,
        address recipient,
        address[] calldata pools
    ) external returns (
        bool transferInOk,
        bool transferOutOk,
        bool approvalOk,
        uint256 balanceBeforeIn,
        uint256 balanceAfterIn,
        uint256 balanceAfterOut,
        uint256 recipientBefore,
        uint256 recipientAfter,
        uint256 gasIn,
        uint256 gasOut,
        uint256 sellAmount,
        SellLeg[] memory sellLegs
    ) {
        IERC20 erc20 = IERC20(token);

        // Read pre-transfer balances for both the settlement and the final recipient.
        balanceBeforeIn = erc20.balanceOf(settlement);
        recipientBefore = erc20.balanceOf(recipient);

        // Transfer from holder (this address) to settlement using a low-level call so that tokens
        // which omit the bool return value (e.g. USDT) do not cause a revert during ABI decoding.
        // Success condition: call did not revert AND, if return data is present, it decodes true.
        uint256 g1 = gasleft();
        {
            (bool ok, bytes memory data) = token.call(
                abi.encodeWithSelector(0xa9059cbb, settlement, amount)
            );
            transferInOk = ok && (data.length == 0 || abi.decode(data, (bool)));
        }
        gasIn = g1 - gasleft();

        if (!transferInOk) {
            return (false, false, false, balanceBeforeIn, 0, 0, recipientBefore, 0, gasIn, 0, 0, sellLegs);
        }

        balanceAfterIn = erc20.balanceOf(settlement);

        // Guard: a token that returns true but reduces the settlement balance is pathological.
        // Without this check Solidity 0.8 checked arithmetic would revert the entire eth_call,
        // making the result undecodable. Instead we surface it as a transfer failure.
        if (balanceAfterIn < balanceBeforeIn) {
            return (false, false, false, balanceBeforeIn, balanceAfterIn, 0, recipientBefore, 0, gasIn, 0, 0, sellLegs);
        }

        // received may be less than amount for fee-on-transfer tokens.
        uint256 received = balanceAfterIn - balanceBeforeIn;

        // Keep half of what arrived for the sell legs, split evenly across the pools.
        uint256 sellBudget;
        if (pools.length > 0) {
            sellAmount = received / (2 * pools.length);
            sellBudget = sellAmount * pools.length;
        }

        // Transfer out from settlement to recipient via the injected Forwarder.
        uint256 g2 = gasleft();
        try IForwarder(settlement).forwardTransfer(token, recipient, received - sellBudget) returns (bool success) {
            transferOutOk = success;
        } catch {
            transferOutOk = false;
        }
        gasOut = g2 - gasleft();

        if (!transferOutOk) {
            balanceAfterOut = erc20.balanceOf(settlement);
            return (true, false, false, balanceBeforeIn, balanceAfterIn, balanceAfterOut, recipientBefore, 0, gasIn, gasOut, sellAmount, sellLegs);
        }

        balanceAfterOut = erc20.balanceOf(settlement);
        recipientAfter = erc20.balanceOf(recipient);

        if (sellAmount > 0) {
            sellLegs = _sell(erc20, settlement, pools, sellAmount);
        }

        // Test that settlement can approve (some tokens block approvals from contracts).
        try IForwarder(settlement).forwardApprove(token, recipient, type(uint256).max) returns (bool success) {
            approvalOk = success;
        } catch {
            approvalOk = false;
        }
    }

    /// @notice Transfer `sellAmount` from the settlement into each pool and record the pool's
    /// balance around each transfer.
    /// @dev A transfer that reverts (e.g. the token calls back into a pool whose code is replaced)
    /// or runs out of its SELL_LEG_GAS is recorded as not ok instead of reverting the whole call.
    function _sell(
        IERC20 erc20,
        address settlement,
        address[] calldata pools,
        uint256 sellAmount
    ) internal returns (SellLeg[] memory legs) {
        legs = new SellLeg[](pools.length);
        for (uint256 i = 0; i < pools.length; i++) {
            address pool = pools[i];
            legs[i].balanceBefore = erc20.balanceOf(pool);
            try IForwarder(settlement).forwardTransfer{gas: SELL_LEG_GAS}(address(erc20), pool, sellAmount) returns (bool success) {
                legs[i].ok = success;
            } catch {
                legs[i].ok = false;
            }
            legs[i].balanceAfter = erc20.balanceOf(pool);
        }
    }
}
